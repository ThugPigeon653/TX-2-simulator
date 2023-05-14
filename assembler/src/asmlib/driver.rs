mod output;
mod symtab;
#[cfg(test)]
mod tests;

use std::cmp::max;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs::OpenOptions;
use std::io::{BufReader, BufWriter, Read};
#[cfg(test)]
use std::ops::Range;
use std::path::Path;

use chumsky::error::Rich;
use tracing::{event, span, Level};

use super::ast::*;
use super::eval::SymbolContext;
use super::parser::parse_source_file;
use super::state::NumeralMode;
use super::symbol::SymbolName;
use super::types::*;
use base::prelude::{Address, Unsigned36Bit, Unsigned6Bit};
use base::subword;
use symtab::SymbolTable;
use symtab::*;

#[cfg(test)]
use base::charset::Script;
#[cfg(test)]
use base::prelude::Unsigned18Bit;
#[cfg(test)]
use base::u36;

/// Represents the meta commands which are still relevant in the
/// directive.  Excludes things like the PUNCH meta command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirectiveMetaCommand {
    Invalid, // e.g."☛☛BOGUS"
    BaseChange(NumeralMode),
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct OutputOptions {
    // TODO: implement arguments of the LIST, PLIST, TYPE
    // metacommands.
    list: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SymbolLookupFailureKind {
    MissingDefault,
    Inconsistent(String),
    Loop { deps_in_order: Vec<SymbolName> },
    MachineLimitExceeded(MachineLimitExceededFailure),
}

#[derive(Debug, PartialEq, Eq)]
pub struct SymbolLookupFailure {
    symbol_name: SymbolName,
    location: Span,
    kind: SymbolLookupFailureKind,
}

impl From<SymbolLookupFailure> for AssemblerFailure {
    fn from(f: SymbolLookupFailure) -> AssemblerFailure {
        let symbol = f.symbol_name;
        let span = f.location;
        match f.kind {
            SymbolLookupFailureKind::MissingDefault => AssemblerFailure::InternalError(format!(
                "no default value was assigned for {symbol}"
            )),
            SymbolLookupFailureKind::MachineLimitExceeded(limit_exceeded) => {
                AssemblerFailure::MachineLimitExceeded(limit_exceeded)
            }
            SymbolLookupFailureKind::Loop { deps_in_order } => {
                let chain: String = deps_in_order
                    .iter()
                    .map(|dep| dep.to_string())
                    .collect::<Vec<_>>()
                    .join("->");
                AssemblerFailure::InvalidProgram {
                    span,
                    msg: format!("definition of symbol {symbol} has a dependency loop ({chain})",),
                }
            }
            SymbolLookupFailureKind::Inconsistent(msg) => AssemblerFailure::InvalidProgram {
                span,
                msg: format!("program is inconsistent: {msg}",),
            },
        }
    }
}

impl From<(SymbolName, Span, SymbolLookupFailureKind)> for SymbolLookupFailure {
    fn from(
        (symbol_name, span, kind): (SymbolName, Span, SymbolLookupFailureKind),
    ) -> SymbolLookupFailure {
        SymbolLookupFailure {
            symbol_name,
            location: span,
            kind,
        }
    }
}

impl SymbolLookupFailure {
    pub(crate) fn symbol_name(&self) -> &SymbolName {
        &self.symbol_name
    }
    pub(crate) fn kind(&self) -> &SymbolLookupFailureKind {
        &self.kind
    }
}

impl std::error::Error for SymbolLookupFailure {}

fn convert_source_file_to_directive(source_file: &SourceFile) -> Directive {
    let mut directive: Directive = Directive::default();
    for (block_number, mblock) in source_file.blocks.iter().enumerate() {
        // We still include zero-word blocks in the directive output
        // so that we don't change the block numbering.
        let len = mblock.instruction_count();
        let location: Option<Address> = match mblock.origin.as_ref() {
            None => {
                let address = Origin::default_address();
                event!(
                    Level::DEBUG,
                    "Locating directive block {block_number} having {len} words at default origin {address:o}",
                );
                Some(address)
            }
            Some(Origin::Literal(_, address)) => {
                event!(
                    Level::DEBUG,
                    "Locating directive block {block_number} having {len} words at origin {address:o}",
                );
                Some(*address)
            }
            Some(Origin::Symbolic(_, name)) => {
                event!(
                    Level::DEBUG,
                    "Locating directive block {block_number} having {len} words at symbolic location {name}, which is not resolved yet",
                );
                None
            }
        };
        let mut block = Block {
            origin: mblock.origin.clone(),
            location,
            items: Vec::with_capacity(mblock.statements.len()),
        };
        for statement in mblock.statements.iter() {
            match statement {
                Statement::Instruction(inst) => {
                    block.push(inst.clone());
                }
                Statement::Assignment(_, _, _) => (),
            }
        }
        directive.push(block);
    }

    match source_file.punch {
        Some(PunchCommand(Some(address))) => {
            event!(
                Level::INFO,
                "program entry point was specified as {address:o}"
            );
            directive.set_entry_point(address);
        }
        Some(PunchCommand(None)) => {
            event!(Level::INFO, "program entry point was not specified");
        }
        None => {
            event!(
                Level::WARN,
                "No PUNCH directive was given, program has no start address"
            );
        }
    }
    // Because the PUNCH instruction causes the assembler
    // output to be punched to tape, this effectively
    // marks the end of the input.  On the real M4
    // assembler it is likely possible for there to be
    // more manuscript after the PUNCH metacommand, and
    // for this to generate a fresh reader leader and so
    // on.  But this is not supported here.  The reason we
    // don't support it is that we'd need to know the
    // answers to a lot of quesrtions we don't have
    // answers for right now.  For example, should the
    // existing program be cleared?  Should the symbol
    // table be cleared?

    directive
}

/// Pass 1 converts the program source into an abstract syntax representation.
fn assemble_pass1<'a>(
    source_file_body: &'a str,
    errors: &mut Vec<Rich<'a, char>>,
) -> Result<(Option<SourceFile>, OutputOptions), AssemblerFailure> {
    let span = span!(Level::ERROR, "assembly pass 1");
    let _enter = span.enter();
    let options = OutputOptions {
        // Because we don't parse the LIST etc. metacommands yet, we
        // simply hard-code the list option so that the symbol table isn't
        // unused.
        list: true,
    };

    fn setup(state: &mut NumeralMode) {
        // Octal is actually the default numeral mode, we just call
        // set_numeral_mode here to keep Clippy happy until we
        // implement ☛☛DECIMAL and ☛☛OCTAL.
        state.set_numeral_mode(NumeralMode::Decimal); // appease Clippy
        state.set_numeral_mode(NumeralMode::Octal);
    }

    let (sf, mut new_errors) = parse_source_file(source_file_body, setup);
    errors.append(&mut new_errors);
    Ok((sf, options))
}

/// This test helper is defined here so that we don't have to expose
/// assemble_pass1, assemble_pass2.
#[cfg(test)]
pub(crate) fn assemble_nonempty_valid_input<'a>(input: &'a str) -> (Directive, FinalSymbolTable) {
    let mut errors: Vec<Rich<'_, char>> = Vec::new();
    let result: Result<(Option<SourceFile>, OutputOptions), AssemblerFailure> =
        assemble_pass1(input, &mut errors);
    if !errors.is_empty() {
        panic!("assemble_nonempty_valid_input: errors were reported: {errors:?}");
    }
    match result {
        Ok((None, _)) => unreachable!("parser should generate output if there are no errors"),
        Ok((Some(source_file), _options)) => {
            let p2output = assemble_pass2(&source_file)
                .expect("test program should not extend beyong physical memory");
            if !p2output.errors.is_empty() {
                panic!("input should be valid: {:?}", &p2output.errors);
            }
            match p2output.directive {
                Some(directive) => (directive, p2output.symbols),
                None => {
                    panic!("assembly pass 2 generated no errors but also no output");
                }
            }
        }
        Err(e) => {
            panic!("input should be valid: {}", e);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct BinaryChunk {
    pub(crate) address: Address,
    pub(crate) words: Vec<Unsigned36Bit>,
}

impl BinaryChunk {
    fn is_empty(&self) -> bool {
        self.words.is_empty()
    }

    fn count_words(&self) -> usize {
        self.words.len()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Binary {
    entry_point: Option<Address>,
    chunks: Vec<BinaryChunk>,
}

impl Binary {
    fn count_words(&self) -> usize {
        self.chunks().iter().map(|chunk| chunk.count_words()).sum()
    }

    fn entry_point(&self) -> Option<Address> {
        self.entry_point
    }

    fn set_entry_point(&mut self, address: Address) {
        self.entry_point = Some(address)
    }

    fn add_chunk(&mut self, chunk: BinaryChunk) {
        self.chunks.push(chunk)
    }

    fn chunks(&self) -> &[BinaryChunk] {
        &self.chunks
    }

    fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }
}

fn origin_as_address(
    origin: &Origin,
    block_number: usize,
    symtab: &SymbolTable,
    next_address: Option<Address>,
) -> (Option<SymbolName>, Address) {
    match origin {
        Origin::Literal(_span, addr) => (None, *addr),
        Origin::Symbolic(span, name) => {
            match symtab.lookup_final(name, *span, &SymbolContext::origin(block_number)) {
                Ok(address) => (Some(name.clone()), subword::right_half(address).into()),
                Err(e) => match next_address {
                    Some(addr) => {
                        event!(
                            Level::WARN,
                            "unable to evaluate origin {name} ({e}), using next free address {addr:o}"
                        );
                        (Some(name.clone()), addr)
                    }
                    None => {
                        let addr = Origin::default_address();
                        event!(
                            Level::WARN,
                            "unable to evaluate origin {name} ({e}), using default {addr:o}"
                        );
                        (Some(name.clone()), addr)
                    }
                },
            }
        }
    }
}

fn calculate_block_origins(
    source_file: &SourceFile,
    symtab: &mut SymbolTable,
) -> Result<(Vec<(Option<SymbolName>, Address)>, Option<Address>), MachineLimitExceededFailure> {
    let mut result = Vec::new();
    let mut next_address: Option<Address> = None;

    for (block_number, block) in source_file.blocks.iter().enumerate() {
        let (maybe_name, base): (Option<SymbolName>, Address) = match block.origin.as_ref() {
            Some(origin) => origin_as_address(origin, block_number, symtab, next_address),
            None => (None, next_address.unwrap_or_else(Origin::default_address)),
        };
        result.push((maybe_name, base));
        let next = match offset_from_origin(&base, block.instruction_count()) {
            Ok(a) => a,
            Err(_) => {
                return Err(MachineLimitExceededFailure::BlockTooLarge {
                    block_number,
                    block_origin: base,
                    offset: block.instruction_count(),
                });
            }
        };
        next_address = Some(if let Some(n) = next_address {
            max(n, next)
        } else {
            next
        });
    }
    Ok((result, next_address))
}

fn unique_symbols_in_order<I>(items: I) -> Vec<(SymbolName, Span)>
where
    I: IntoIterator<Item = (SymbolName, Span)>,
{
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    for (sym, span) in items {
        if !seen.contains(&sym) {
            seen.insert(sym.clone());
            result.push((sym, span));
        }
    }
    result
}

struct Pass2Output<'a> {
    directive: Option<Directive>,
    symbols: FinalSymbolTable,
    errors: Vec<Rich<'a, char>>,
}

/// Pass 2 converts the abstract syntax representation into a
/// `Directive`, which is closer to binary code.
fn assemble_pass2<'a>(source_file: &SourceFile) -> Result<Pass2Output<'a>, AssemblerFailure> {
    let span = span!(Level::ERROR, "assembly pass 2");
    let _enter = span.enter();

    let mut errors = Vec::new();
    let mut symtab = SymbolTable::default();
    for (symbol, span, context) in source_file.global_symbol_references() {
        symtab.record_usage_context(symbol.clone(), span, context)
    }
    for (symbol, span, definition) in source_file.global_symbol_definitions() {
        match symtab.define(symbol.clone(), definition.clone()) {
            Ok(_) => (),
            Err(e) => {
                errors.push(Rich::custom(span, format!("bad symbol definition: {e}")));
            }
        }
    }
    let (origins, mut next_free_address): (Vec<(Option<SymbolName>, Address)>, Option<Address>) =
        match calculate_block_origins(source_file, &mut symtab) {
            Ok((origins, next)) => (origins, next),
            Err(e) => {
                return Err(e.into());
            }
        };
    for (block_number, (_maybe_name, address)) in origins.iter().enumerate() {
        symtab.record_block_origin(block_number, *address);
        let size = source_file.blocks[block_number].instruction_count();
        let after_end = match offset_from_origin(address, size) {
            Ok(a) => a,
            Err(_) => {
                return Err(MachineLimitExceededFailure::BlockTooLarge {
                    block_number,
                    block_origin: *address,
                    offset: size,
                }
                .into());
            }
        };
        next_free_address = next_free_address
            .map(|current| max(current, after_end))
            .or(Some(after_end));
    }

    let final_symbols = match next_free_address {
        Some(next_free) => {
            let mut rc_block: Vec<Unsigned36Bit> = Vec::new();
            let symbol_refs_in_program_order: Vec<(SymbolName, Span)> = unique_symbols_in_order(
                source_file
                    .global_symbol_definitions()
                    .map(|(symbol, span, _)| (symbol, span))
                    .chain(
                        source_file
                            .global_symbol_references()
                            .map(|(symbol, span, _)| (symbol, span)),
                    ),
            );
            match finalise_symbol_table(
                symtab,
                symbol_refs_in_program_order.iter(),
                next_free.into(),
                &mut rc_block,
                Unsigned6Bit::ZERO,
            ) {
                Ok(fs) => fs,
                Err(e) => {
                    return Err(e.into());
                }
            }
        }
        None => {
            event!(
                Level::WARN,
                "the program appears to be empty; generating 0 instructions"
            );
            return Ok(Pass2Output {
                directive: None,
                symbols: FinalSymbolTable::default(),
                errors,
            });
        }
    };

    let directive = convert_source_file_to_directive(source_file);
    event!(
        Level::INFO,
        "assembly generated {} instructions",
        directive.instruction_count()
    );
    Ok(Pass2Output {
        directive: Some(directive),
        symbols: final_symbols,
        errors,
    })
}

/// Pass 3 generates binary code.
fn assemble_pass3(
    directive: Directive,
    final_symtab: &mut FinalSymbolTable,
) -> Result<Binary, AssemblerFailure> {
    let span = span!(Level::ERROR, "assembly pass 3");
    let _enter = span.enter();

    let mut binary = Binary::default();
    if let Some(address) = directive.entry_point() {
        binary.set_entry_point(address);
    }

    for (block_number, block) in directive.blocks.iter().enumerate() {
        let words: Vec<Unsigned36Bit> = block
            .items
            .iter()
            .map(|inst| {
                inst.value(final_symtab)
                    .expect("lookup on FinalSymbolTable is infallible")
            })
            .collect::<Vec<_>>();
        let address: Address = match final_symtab.get_block_origin(&block_number) {
            Some(a) => {
                event!(
                    Level::DEBUG,
                    "Block {block_number} of output has address {:o} and length {}",
                    *a,
                    words.len()
                );
                *a
            }
            None => {
                return Err(AssemblerFailure::InternalError(
                    format!("starting address for block {block_number} was not calculated by calculate_block_origins")
                ));
            }
        };
        if words.is_empty() {
            event!(
                Level::DEBUG,
                "block {block_number} will not be included in the output because it is empty"
            );
        } else {
            binary.add_chunk(BinaryChunk { address, words });
        }
    }
    Ok(binary)
}

fn pos_line_column(s: &str, pos: usize) -> Result<(usize, usize), ()> {
    let mut line = 1;
    let mut column = 1;
    for (i, ch) in s.chars().enumerate() {
        if i == pos {
            return Ok((line, column));
        }
        match ch {
            '\n' => {
                column = 1;
                line += 1;
            }
            _ => {
                column += 1;
            }
        }
    }
    Err(())
}

fn fail_with_diagnostics(source_file_body: &str, errors: Vec<Rich<char>>) -> AssemblerFailure {
    match errors.as_slice() {
        [first, ..] => {
            for e in errors.iter() {
                eprintln!("{}", e);
            }
            let (line, column) = pos_line_column(source_file_body, first.span().start)
                .expect("span for error message should be inside the file");
            return AssemblerFailure::SyntaxError {
                line: line as u32,
                column: Some(column),
                msg: first.to_string(),
            };
        }
        [] => {
            unreachable!("should not be called if errors is empty")
        }
    }
}

pub(crate) fn assemble_source(source_file_body: &str) -> Result<Binary, AssemblerFailure> {
    let mut errors = Vec::new();
    let (source_file, options) = assemble_pass1(source_file_body, &mut errors)?;
    if !errors.is_empty() {
        return Err(fail_with_diagnostics(source_file_body, errors));
    }
    let source_file =
        source_file.expect("assembly pass1 generated no errors, an AST should have been returned");

    // Now we do pass 2.
    let Pass2Output {
        directive,
        mut symbols,
        errors,
    } = assemble_pass2(&source_file)?;
    if !errors.is_empty() {
        return Err(fail_with_diagnostics(source_file_body, errors));
    }
    let directive = match directive {
        None => {
            return Err(AssemblerFailure::InternalError(
                "assembly pass 2 generated no errors, so it should have generated ouptut code (even if empty)".to_string()
            ));
        }
        Some(d) => d,
    };

    // Now we do pass 3.
    let binary = {
        event!(
            Level::INFO,
            "assembly pass 2 generated {} instructions",
            directive.instruction_count()
        );

        if options.list {
            // List the symbols.
            for (name, definition) in symbols.list() {
                println!("{name:>20} = {definition:12o}");
            }
        }

        // Pass 3 generates the binary output
        assemble_pass3(directive, &mut symbols)?
    };

    // The count here also doesn't include the size of the RC-block as
    // that is not yet implemented.
    event!(
        Level::INFO,
        "assembly pass 3 generated {} words of binary output (not counting the reader leader)",
        binary.count_words()
    );
    Ok(binary)
}

#[cfg(test)]
fn span(range: Range<usize>) -> Span {
    Span::from(range)
}

#[test]
fn test_assemble_pass1() {
    let input = concat!("14\n", "☛☛PUNCH 26\n");
    let expected_directive_entry_point = Some(Address::new(Unsigned18Bit::from(0o26_u8)));
    let expected_block = ManuscriptBlock {
        origin: None,
        statements: vec![Statement::Instruction(ProgramInstruction {
            span: span(0..2),
            tag: None,
            holdbit: HoldBit::Unspecified,
            parts: vec![InstructionFragment {
                value: Expression::Literal(LiteralValue::from((
                    span(0..2),
                    Script::Normal,
                    u36!(0o14),
                ))),
            }],
        })],
    };

    let mut errors = Vec::new();
    assert_eq!(
        assemble_pass1(input, &mut errors).expect("assembly should succeed"),
        (
            Some(SourceFile {
                punch: Some(PunchCommand(expected_directive_entry_point)),
                blocks: vec![expected_block],
            }),
            OutputOptions { list: true }
        )
    );
    assert!(errors.is_empty());
}

pub fn assemble_file(
    input_file_name: &OsStr,
    output_file_name: &Path,
) -> Result<(), AssemblerFailure> {
    let input_file = OpenOptions::new()
        .read(true)
        .open(input_file_name)
        .map_err(|e| AssemblerFailure::IoErrorOnInput {
            filename: input_file_name.to_owned(),
            error: e,
            line_number: None,
        })?;

    let source_file_body = {
        let mut body = String::new();
        match BufReader::new(input_file).read_to_string(&mut body) {
            Err(e) => {
                return Err(AssemblerFailure::IoErrorOnInput {
                    filename: input_file_name.to_owned(),
                    error: e,
                    line_number: None,
                })
            }
            Ok(_) => body,
        }
    };

    let user_program: Binary = assemble_source(&source_file_body)?;

    // The Users Guide explains on page 6-23 how the punched binary
    // is created (and read back in).
    let output_file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(output_file_name)
        .map_err(|e| AssemblerFailure::IoErrorOnOutput {
            filename: output_file_name.to_owned(),
            error: e,
        })?;
    let mut writer = BufWriter::new(output_file);
    output::write_user_program(&user_program, &mut writer, output_file_name)
}
