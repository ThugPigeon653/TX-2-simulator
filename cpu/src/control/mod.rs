//! Emulates the control unit of the TX-2.
//!
//! The control unit is conceptually similar to the CPU of a modern
//! computer, except that the arithmetic unit and registers are
//! separate.  Within this emulator, the control unit performs the
//! following functions:
//!
//! - Instruction decoding
//! - Handle CODABO and STARTOVER
//! - Keep track of the flags of each sequence
//! - Keep track of the placeholder of each sequence
//! - Manage switching between sequences
//! - Remember the setting of the TSP (Toggle Start Point) register

use base::instruction::{Inst, Instruction, Opcode, OperandAddress, SymbolicInstruction};
use base::prelude::*;
use base::subword;

use crate::alarm::Alarm;
use crate::exchanger::{
    exchanged_value,
    SystemConfiguration,
};
use crate::memory::{
    ExtraBits,
    MemoryMapped,
    MemoryUnit,
    MemoryOpFailure,
    MetaBitChange,
    self,
};

mod op_configuration;
mod op_index;
mod op_jump;


#[derive(Debug)]
enum ProgramCounterChange {
    SequenceChange(Unsigned6Bit),
    CounterUpdate,
    Jump(Address),
}

/// Flags represent requests to run for instruction sequences (today
/// one might describe these as threads).  Some sequences are special:
///
/// 0: Sequence which is run to start the computer (e.g. when "CODABO"
/// or "START OVER" is pressed).
///
/// 41: Handles various I/O alarm conditions.
/// 42: Handles various trap conditions (see Users Handbook page 42).
/// 47: Handles miscellaneous inputs
/// 50: DATRAC (A/D converter)
/// 51: Xerox printer
/// 52: PETR (paper tape reader)
/// 54: Interval timer
/// 55: Light pen
/// 60: Oscilloscope display
/// 61: RNG
/// 63: Punch
/// 65: Lincoln Writer input
//  66: Lincoln Writer output
/// 71: Lincoln Writer input
/// 72: Lincoln Writer output
/// 75: Misc output
/// 76: Not for physical devices.
/// 77: Not for physical devices.
///
/// The flag for sequences 76 and 77 may only be raised/lowered by
/// program control.
///
/// The standard readin program executes the program it read in from
/// the address specified by the program.  The program executes as
/// sequence 52 (PETR) with the PETR unit initially turned off.
///
#[derive(Debug)]
struct SequenceFlags {
    flag_values: u64,
}

impl SequenceFlags {
    fn new() -> SequenceFlags {
        // New instances start with no flags raised (i.e. in "Limbo",
        // STARTOVER not running).
        SequenceFlags {
	    flag_values: 0,
	}
    }

    fn lower_all(&mut self) {
        self.flag_values = 0;
    }

    fn flagbit(flag: &SequenceNumber) -> u64 {
	1_u64 << u64::from(*flag)
    }

    fn lower(&mut self, flag: &SequenceNumber) {
        assert!(u16::from(*flag) < 0o100_u16);
        self.flag_values &= !SequenceFlags::flagbit(flag);
    }

    fn raise(&mut self, flag: &SequenceNumber) {
        assert!(u16::from(*flag) < 0o100_u16);
        self.flag_values |= SequenceFlags::flagbit(flag);
    }

    /// Return the index of the highest-priority (lowest-numbered)
    /// flag.  If no flag is raised (the machine is in "Limbo"),
    /// return None.
    fn highest_priority_raised_flag(&self) -> Option<SequenceNumber> {
        let n = self.flag_values.trailing_zeros();
        if n == 64 {
            None
        } else {
            Some(n.try_into().unwrap())
        }
    }
}

#[test]
fn test_sequence_flags() {
    let mut flags = SequenceFlags::new();

    flags.lower_all();
    assert_eq!(flags.highest_priority_raised_flag(), None);

    flags.raise(&Unsigned6Bit::ZERO);
    assert_eq!(flags.highest_priority_raised_flag().map(i8::from), Some(0_i8));
    flags.raise(&Unsigned6Bit::ONE);
    // 0 is still raised, so it still has the highest priority.
    assert_eq!(flags.highest_priority_raised_flag(), Some(Unsigned6Bit::ZERO));

    flags.lower(&SequenceNumber::ZERO);
    assert_eq!(flags.highest_priority_raised_flag(), Some(Unsigned6Bit::ONE));
    flags.lower(&SequenceNumber::ONE);
    assert_eq!(flags.highest_priority_raised_flag(), None);

    let four = SequenceNumber::try_from(4_i8).expect("valid test data");
    let six = SequenceNumber::try_from(6_i8).expect("valid test data");
    flags.raise(&four);
    flags.raise(&six);
    assert_eq!(flags.highest_priority_raised_flag(), Some(four));
    flags.lower(&four);
    assert_eq!(flags.highest_priority_raised_flag(), Some(six));
}

#[derive(Debug)]
struct ControlRegisters {
    pub e: Unsigned36Bit,
    pub n: Instruction,
    pub n_sym: Option<SymbolicInstruction>,
    pub p: Address,
    pub q: Address,

    // The k register (User guide 4-3.1) holds the current sequence
    // number (User guide 5-24).  k is Option<SequenceNumber> in order
    // to allow the (emulated) control unit to recognise a CODABO
    // button as indicating a need to change sequence from the control
    // unit's initial state to sequence 0.
    //
    // This likely doesn't reflect the actual operation of the TX-2
    // very well, and better understanding of the real operation of
    // the machine will likely change this.
    //
    // I think that perhaps section 12-2.6.2 of Volume 2 of the
    // technical manual may explain how the real TX-2 avoided this
    // problem, but I don't think I understand what that section says.
    // The text is:
    //
    // """12-2.6.2 XPS FLIP-FLOP LOGIC.  This flip-floop inhibits the
    // X Memory strobe pulse into X when the register selected has the
    // same address or the current program counter, is not register 0,
    // and this is the first reference to this register since the last
    // sequence change.  In this case all the cores of the register
    // are clearered and only "junk" (with a 50-50 chance of a bad
    // parity) would be strobed into X.  If XPS¹, then a clear pulse
    // is substituted for the strobe pulse.
    //
    // The flip-flop is set whenever a sequence change occurs, and is
    // cleared the first time thereafter that the program counter
    // register is referenced during a PK cycle (if ever).  See Fig
    // 12-8."""
    pub k: Option<SequenceNumber>,

    spr: Address, // Start Point Register

    /// Index register 0 is the Toggle Start point.
    /// Index registers 40-77 are program counters for the sequences.
    ///
    /// The index registers form an 18-bit ring (as stated in the
    /// description of the AUX instruction) and are described on page
    /// 3-68 of the User Handbook (section 3-3.1) as being signed
    /// integers.
    index_regs: [Signed18Bit; 0o100],	 // AKA the X memory
    f_memory: [SystemConfiguration; 32], // the F memory
    flags: SequenceFlags,
    current_sequence_is_runnable: bool,
}

impl ControlRegisters {
    fn new() -> ControlRegisters {
        let fmem = {
            let default_val = SystemConfiguration::try_from(0).unwrap();
            [default_val; 32]
        };
        let mut result = ControlRegisters {
            e: Unsigned36Bit::ZERO,
            n: Instruction::invalid(), // not a valid instruction
            n_sym: None,
            p: Address::default(),
            q: Address::default(),
            k: None, // not 0, so that we can recognise CODABO.
            index_regs: [Signed18Bit::default(); 0o100],
            f_memory: fmem,
            flags: SequenceFlags::new(),
	    current_sequence_is_runnable: false,
            spr: Address::default(),
        };
        // Index register 0 always contains 0.  This should still be
        // true if we modify the behaviour of Address::default(),
        // which is why we override it here.
        result.index_regs[0] = Signed18Bit::ZERO;
        result
    }

    fn previous_instruction_hold(&self) -> bool {
        self.n.is_held()
    }

    fn set_spr(&mut self, addr: &Address) {
        self.spr = *addr;
    }

    fn get_index_register(&self, n: Unsigned6Bit) -> Signed18Bit {
	let n = usize::from(n);
        assert_eq!(self.index_regs[0], 0);
        assert!(n < 0o100);
        return self.index_regs[usize::from(n)];
    }

    fn get_index_register_as_address(&mut self, n: Unsigned6Bit) -> Address {
	let value: Signed18Bit = self.get_index_register(n);
	Address::from(value.reinterpret_as_unsigned())
    }

    fn set_index_register(&mut self, n: Unsigned6Bit, value: &Signed18Bit) {
	let n = usize::from(n);
        assert_eq!(self.index_regs[0], 0);
        assert_ne!(n, 0, "Index register 0 should be fixed at 0");
        assert!(n < 0o100);
        self.index_regs[n] = *value;
    }

    fn set_index_register_from_address(&mut self, n: Unsigned6Bit, addr: &Address) {
	let value: Unsigned18Bit = Unsigned18Bit::from(*addr);
	self.set_index_register(n, &value.reinterpret_as_signed());
    }

    fn get_f_mem(&self, n: Unsigned5Bit) -> SystemConfiguration {
        assert!(u8::from(n) < 0o37_u8);
        assert_eq!(self.f_memory[0], SystemConfiguration::zero());
        let pos: usize = n.into();
        self.f_memory[pos]
    }
}

#[derive(Clone, Copy, Debug)]
pub enum ResetMode {
    ResetTSP = 0,
    Reset0 = 0o03777710,
    Reset1 = 0o03777711,
    Reset2 = 0o03777712,
    Reset3 = 0o03777713,
    Reset4 = 0o03777714,
    Reset5 = 0o03777715,
    Reset6 = 0o03777716,
    Reset7 = 0o03777717,
}

impl ResetMode {
    fn address(&self) -> Option<Address> {
        use ResetMode::*;
        match self {
            Reset0 | Reset1 | Reset2 | Reset3 | Reset4 | Reset5 | Reset6 | Reset7 => Some(
                Address::from(Unsigned18Bit::try_from(*self as u32).unwrap()),
            ),
            ResetTSP => None, // need to read the TSP toggle switch.
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
enum SetMetabit {
    Never,
    //ForcedNever,		// by console switch
    Instructions,
    DeferredAddresses,
    Operands,
}


/// ControlUnit simulates the operation of the Control Element of the TX-2 computer.
///
#[derive(Debug)]
pub struct ControlUnit {
    regs: ControlRegisters,
    running: bool,
    /// `trap_on_change_sequence` is described in Users Handbook
    /// section 4-5 No. 42, Trapping.
    trap_on_change_sequence: bool,
    set_metabit_mode: SetMetabit,
}

fn sign_extend_index_value(index_val: &Signed18Bit) -> Unsigned36Bit {
    let left = if index_val.is_negative() {
	Unsigned18Bit::MAX
    } else {
	Unsigned18Bit::ZERO
    };
    subword::join_halves(left, index_val.reinterpret_as_unsigned())
}

impl ControlUnit {
    pub fn new() -> ControlUnit {
        ControlUnit {
            regs: ControlRegisters::new(),
            running: false,
            trap_on_change_sequence: false,
            set_metabit_mode: SetMetabit::Never,
        }
    }

    /// There are actually 9 different CODABO buttons (see page 5-18
    /// of the User Guide).  and 9 different RESET buttons.
    /// Each RESET button has a corresponding CODABO button.
    /// See the `reset` method for address assignments.
    ///
    /// The CODABO operation leaves the Start Point Register set to
    /// the selected start point.  There are also 9 reset buttons
    /// which perform a similar task.
    pub fn codabo(&mut self, reset_mode: &ResetMode) {
        // TODO: clear alarms.
        // We probably don't need an equivalent of resetting the
        // control flip-flops in an emulator.  But if we did, that
        // would happen here.
        //
        // On the other hand, the P register is described as an
        // "18-bit flip-flop" in section 4-2.2 of the User Handbook,
        // so perhaps all the registers in V memory are cleared by
        // CODABO.
        //
        // TODO: clear the "connected" ("C") flip-flops in the I/O
        // system.
        println!("Starting CODABO {:?}", &reset_mode);
        self.reset(reset_mode);
        self.regs.flags.lower_all();
	self.regs.current_sequence_is_runnable = false;
        self.startover();
        // TODO: begin issuing clock cycles.
        println!("After CODABO, control unit contains {:#?}", &self);
    }

    /// There are 9 separate RESET buttons, for 8 fixed addresses and
    /// another which uses the Toggle Start Point register.  There
    /// appear to be two Toggle Start Point switches, one on the front
    /// panel and a second on a remote control unit.  The
    /// fixed-address RESET buttons correspond to the fixed
    /// addresses 3777710 through 3777717, inclusive.
    ///
    /// RESET *only* loads the Start Point Register, nothing else.
    pub fn reset(&mut self, reset_mode: &ResetMode) {
        self.regs.set_spr(&match reset_mode.address() {
            Some(address) => address,
            None => self.tsp(),
        });
    }

    /// Handle press of STARTOVER (or part of the operation of
    /// CODABO).
    pub fn startover(&mut self) {
        self.regs.flags.raise(&SequenceNumber::ZERO);
    }

    /// Return the value in the Toggle Start Register.  It is likely
    /// that this was memory-mapped in the real machine, but if that's
    /// the case the user guide doesn't specify where.  For now, we
    /// haven't made it configurable (i.e. have not emulated the
    /// hardware) yet, either.  We just hard-code it to point at the
    /// "Memory Clear / Memory Smear" program in the plugboard.
    fn tsp(&self) -> Address {
        // The operation of RESET (or CODABO) will copy this value
        // into the zeroth index register (which the program counter
        // placeholder for sequence 0).
        memory::STANDARD_PROGRAM_CLEAR_MEMORY
    }

    fn change_sequence(&mut self, prev_seq: Option<SequenceNumber>, mut next_seq: SequenceNumber) {
        // If the "Trap on Change Sequence" is enabled and the new
        // sequence is marked (bit 2.9 of its index register is set).
        // Activate unit 42, unless that's the unit which is giving up
        // control.
        //
        // I'm not sure what should happen for the alternative case,
        // where a unit of higher priority than 42 is marked for
        // trap-on-sequence-change.
        if prev_seq == Some(next_seq) {
            // TODO: log a warning event.
            return;
        }

	fn is_marked_placeholder(index_val: &Signed18Bit) -> bool {
	    index_val < &0
	}

	let trap_seq = Unsigned6Bit::try_from(42).unwrap();
        let sequence_change_trap = self.trap_on_change_sequence
            && is_marked_placeholder(&self.regs.get_index_register(next_seq))
            && self.regs.k != Some(trap_seq)
            && next_seq > trap_seq;

        let previous_sequence: Unsigned6Bit = match prev_seq {
            None => Unsigned6Bit::ZERO,
            Some(n) => n,
        };
        self.regs.e = join_halves(
            join_quarters(Unsigned9Bit::from(previous_sequence),
			  Unsigned9Bit::from(next_seq)),
            Unsigned18Bit::from(self.regs.p),
        );

        if sequence_change_trap {
            self.regs.flags.raise(&trap_seq);
            next_seq = trap_seq;
        }
        self.regs.k = Some(next_seq);
	if let Some(prev) = prev_seq {
            let p = self.regs.p;
            self.regs.set_index_register_from_address(prev, &p);
        }
	self.set_program_counter(ProgramCounterChange::SequenceChange(next_seq));
    }

    fn set_program_counter(&mut self, change: ProgramCounterChange) {
	match change {
	    ProgramCounterChange::SequenceChange(next_seq) => {
		// According to the Technical Manual, page 12-6,
		// change of seqeuence is the only time in which P₂.₉
		// is altered.
		if next_seq != 0 {
		    self.regs.p = self.regs.get_index_register_as_address(next_seq);
		} else {
		    // Index register 0 is always 0, but by setting
		    // the Toggle Status Register, the user can run
		    // sequence 0 from an arbitrary address. That
		    // address can't be stored in index register 0
		    // since that's always 0, so we use an internal
		    // "spr" register which is updated by the
		    // RESET/CODABO buttons.  Here, we copy that saved
		    // value into P.
		    self.regs.p = self.regs.spr;
		}
	    }
	    ProgramCounterChange::CounterUpdate => {
		// Volume 2 of the Technical Manual (section 12-2.3 "P
		// REGISTER DRIVER LOGIC") states, """Information can
		// be transferred into the P register only from the X
		// Adder.  In addition to this single transfer path,
		// ther P register has a counter which can index the
		// contents of the P register by one.  Note that count
		// circuit does not alter the contents of P₂.₉"""
		//
		// Since P₂.₉ is the sign bit, this means that the P
		// register wraps rather than overflows.
		//
		// As a practical matter, this wrap-around case means
		// that self.regs.p previously contained 377,777.
		// That is the last instruction in V Memory.  This is
		// normally an unconditional JMP.  So in the standard
		// plugboard configuration, we're going to take the
		// jmp, meaning that we're never going to fetch an
		// instruction from the address we just computed.
		// But, this case may be needed to cover non-standard
		// plugboard configurations.  According to the
		// Technical Manual, page 12-6, change of seqeuence is
		// the only time in which P₂.₉ is altered.
		let (_old_physical, old_mark) = self.regs.p.split();
		let new_p = self.regs.p.successor(); // p now points at the next instruction.
		let (_new_physical, new_mark) = new_p.split();
		assert_eq!(old_mark, new_mark);
		self.regs.p = new_p;
	    }
	    ProgramCounterChange::Jump(new_p) => {
		// Copy the value of P₂.₉ into `old_mark`.
		let (_old_physical, old_mark) = self.regs.p.split();
		// Update P, keeping the old value of P₂.₉.
		self.regs.p = Address::join(new_p.into(), old_mark);
	    }
	}
    }

    pub fn fetch_instruction(&mut self, mem: &mut MemoryUnit) -> Result<bool, Alarm> {
	// If the previous instruction was held, we don't even scan
	// the flags.  This follows the description of how control
	// handles flags in section 4-3.5 of the User Handbook (page
	// 4-8).
	if !self.regs.previous_instruction_hold() {
            // Handle any possible change of sequence.
            match self.regs.flags.highest_priority_raised_flag() {
		None => {
                    // The current sequence's flag is no longer raised.
		    //
		    // This happens either because the sequence was
		    // dismissed (permanent or temporary drop-out) or IOSj
		    // 40000 ("LOWER FLAG J") had been issued.  In the
		    // latter case, the current sequence should continue
		    // to run until another sequence's flag is raised.
		    if !self.regs.current_sequence_is_runnable {
			return Ok(false);
		    }
		}
		Some(seq) => {
                    println!("Highest-priority sequence is {}", seq);
                    if Some(seq) == self.regs.k {
			// just carry on.
                    } else {
			// Change of sequence.  Either seq is a higher
			// priority than the current sequence, or the
			// (previously) current sequence dropped out.
			self.change_sequence(self.regs.k, seq);
                    }
		}
            }
	}

        // self.regs.k now identifies the sequence we should be
        // running and self.regs.p contains its program counter.

        // Calculate the address from which we will fetch the
        // instruction, and the increment the program counter.
        let p_physical_address = Address::from(self.regs.p.split().0);
	self.set_program_counter(ProgramCounterChange::CounterUpdate);

	// Actually fetch the instruction.
	let meta_op = match self.set_metabit_mode {
            SetMetabit::Instructions => MetaBitChange::Set,
            _ => MetaBitChange::None,
        };
        let instruction_word = match mem.fetch(&p_physical_address, &meta_op) {
            Ok((inst, _meta)) => inst,
            Err(e) => match e {
                MemoryOpFailure::NotMapped => {
                    return Err(Alarm::PSAL(
                        u32::from(p_physical_address),
                        "memory unit indicated physical address is not mapped".to_string(),
                    ));
                }
                MemoryOpFailure::ReadOnly => unreachable!(),
            },
        };
        println!(
            "Fetched instruction {:?} from physical address {:?}",
            instruction_word, p_physical_address
        );
	self.update_n_register(instruction_word)?;
	Ok(true)		// not in Limbo (i.e. a sequence should run)
    }

    fn update_n_register(&mut self, instruction_word: Unsigned36Bit) -> Result<(), Alarm> {
        self.regs.n = Instruction::from(instruction_word);
        if let Ok(symbolic) = SymbolicInstruction::try_from(&self.regs.n) {
            self.regs.n_sym = Some(symbolic);
            Ok(()) // valid instruction
        } else {
            Err(self.invalid_opcode_alarm())
        }
    }

    fn invalid_opcode_alarm(&self) -> Alarm {
        Alarm::OCSAL(
            self.regs.n,
            format!("invalid opcode {:#o}", self.regs.n.opcode_number()),
        )
    }

    /// Execute the instruction in the N register (i.e. the
    /// instruction just fetched by fetch_instruction().  The P
    /// register already points to the next instruction.
    pub fn execute_instruction(&mut self, mem: &mut MemoryUnit) -> Result<(), Alarm> {
        let sym = match &self.regs.n_sym {
            None => return Err(self.invalid_opcode_alarm()),
            Some(s) => s,
        };
        println!("Executing instruction {}...", sym);
        use Opcode::*;
        match sym.opcode() {
            Skx => self.op_skx(),
            Dpx => self.op_dpx(mem),
            Jmp => self.op_jmp(),
	    Jpx => self.op_jpx(mem),
	    Jnx => self.op_jnx(mem),
	    Skm => self.op_skm(mem),
	    Spg => self.op_spg(mem),
            _ => {
                return Err(Alarm::ROUNDTUITAL(format!(
                    "The emulator does not yet implement opcode {}",
                    sym.opcode()
                )));
            }
        }
        //Ok(())
    }

    fn get_config(&self) -> SystemConfiguration {
        let cf = self.regs.n.configuration();
        self.regs.get_f_mem(cf)
    }

    fn fetch_operand_from_address(
        &self,
        mem: &mut MemoryUnit,
        operand_address: &Address,
    ) -> Result<(Unsigned36Bit, ExtraBits), Alarm> {
        let meta_op: MetaBitChange = match self.set_metabit_mode {
            SetMetabit::Operands => MetaBitChange::Set,
            _ => MetaBitChange::None,
        };
        match mem.fetch(operand_address, &meta_op) {
            Ok((word, extra_bits)) => Ok((word, extra_bits)),
            Err(MemoryOpFailure::NotMapped) => Err(Alarm::QSAL(
		self.regs.n,
                Unsigned36Bit::from(*operand_address),
                format!(
                    "memory unit indicated address {:o} is not mapped",
                    operand_address
                ),
            )),
            Err(MemoryOpFailure::ReadOnly) => unreachable!(),
        }
    }

    fn memory_store_without_exchange(
        &self,
        mem: &mut MemoryUnit,
        target: &Address,
        value: &Unsigned36Bit,
        meta_op: &MetaBitChange,
    ) -> Result<(), Alarm> {
	println!(
	    "memory_store_without_exchange: write @{:>06o} <- {:o}",
	    target,
	    value,
	);
        mem.store(target, value, meta_op).map_err(|e| {
            Alarm::QSAL(
                self.regs.n,
                Unsigned36Bit::from(*target),
                format!("memory store to address {:#o} failed: {}", target, e,),
            )
        })
    }

    fn memory_store_with_exchange(
        &self,
        mem: &mut MemoryUnit,
        target: &Address,
        value: &Unsigned36Bit,
        existing: &Unsigned36Bit,
        meta_op: &MetaBitChange,
    ) -> Result<(), Alarm> {
        self.memory_store_without_exchange(
            mem,
            target,
            &exchanged_value(&self.get_config(), value, existing),
            meta_op,
        )
    }

    fn operand_address_with_optional_defer_and_index(
        self: &mut ControlUnit,
        mem: &mut MemoryUnit,
    ) -> Result<Address, Alarm> {
	self.resolve_operand_address(mem, None)
    }

    fn resolve_operand_address(
        self: &mut ControlUnit,
        mem: &mut MemoryUnit,
	mut initial_index_override: Option<Unsigned6Bit>,
    ) -> Result<Address, Alarm> {
	// The deferred addressing process may be performed more than
	// once, in other words it is a loop.  This is explained in
	// section 9-7, "DEFERRED ADDRESSING CYCLES" of Volume 2 of
	// the technical manual.
	while let OperandAddress::Deferred(physical) = self.regs.n.operand_address() {
	    // In effect, this loop emulates a non-ultimate deferred
	    // address cycle.
	    //
            // According to the description of PK3 on page 5-9 of the
            // User handbook, the deferred address calculation and
            // indexing occurs in (i.e. by modifying) the N register.
	    //
	    // JPX and JNX seem to be handled differently, but I don't
	    // think I understand exactly what the difference is
	    // supposed to be.
	    //
	    // (Vol 2, page 12-9): It should also be noted that the
	    // N₂.₉ bit is presented as an input to the X Adder only
	    // when no deferred address cycles are called for.  When
	    // PI¹₂, the input to the X Adder from the N₂.₉ position
	    // is forced to appear as a ZERO.

            println!(
		"deferred addressing: deferred address is {:o}",
		&physical
            );
            let meta_op = match self.set_metabit_mode {
		SetMetabit::DeferredAddresses => MetaBitChange::Set,
		_ => MetaBitChange::None,
            };
            let fetched = match mem.fetch(&physical, &meta_op) {
                Err(e) => {
		    return Err(Alarm::QSAL(
			self.regs.n,
			Unsigned36Bit::from(physical),
			format!("address {:#o} out of range while fetching deferred address: {}", &physical, e),
		    ));
                }
                Ok((word, _meta)) => {
		    // I think it's likely that the TX2 should perform
		    // indexation on deferred addreses.  This idea is
		    // based on the fact that the left subword of
		    // deferred addresses used in plugboard programs
		    // can be nonzero, and on the fact that the
		    // description of the SKM instruction notes "SKM
		    // is therefore non-indexable except through
		    // deferred addressing".
		    let (left, right) = subword::split_halves(word);
		    println!(
			"deferred addressing: fetched full word is {:o},,{:o}; using {:o} as the final address",
			&left, &right, &right);
		    Address::from(right)
                }
	    };

	    // We update the lower 18 bits (i.e. right half) of N with
	    // the value we just loaded from memory.
	    let unchanged_left = subword::left_half(Unsigned36Bit::from(self.regs.n));
	    self.update_n_register(subword::join_halves(unchanged_left, Unsigned18Bit::from(fetched)))?;
	}
	let physical_address = match self.regs.n.operand_address() {
	    // Cannot be a deferred address any more, as loop above
	    // loops until the address is not deferred.
	    OperandAddress::Deferred(_) => unreachable!(),
	    OperandAddress::Direct(physical_address) => physical_address,
	};
	// The defer bit in N is (now) not set.  Emulate a regular or
	// ultimate address cycle.  That is, add the index value to
	// the operand address.  While the index_address field in the
	// instruction is unsigned (following the conventions in the
	// assembly source), the indexation operation itself uses
	// signed arithmetic (see the explanation in the doc comment
	// for the IndexBy trait).
	let j = match initial_index_override {
	    None => self.regs.n.index_address(),
	    Some(overridden) => {
		let j = overridden;
		initial_index_override = None;
		j
	    },
	};
	let delta = self.regs.get_index_register(j); // this is Xj.

        // A number of things expect that the "most recent data (memory)
        // reference" is saved in register Q.  14JMP (a.k.a. JPQ) makes
        // use of this, for example.
        self.regs.q = physical_address.index_by(delta);

        // TODO: figure out if other parts of the system documentation
        // definitely expect the physical operand address to be
        // written back into the N register (in a
        // programmer-detectable way).
        Ok(self.regs.q)
    }

    fn dismiss_unless_held(&mut self) {
	if !self.regs.n.is_held() {
            if let Some(current_seq) = self.regs.k {
                self.regs.flags.lower(&current_seq);
		self.regs.current_sequence_is_runnable = false;
            }
	}
    }

}
