//! Simulation of elapsed time in the simulated system.

use std::thread::sleep;
use std::time::{Duration, Instant};

use tracing::{event, Level};

/// Clock is a simulated system clock.  Its run rate may be real-time
/// (i.e. one simulated second per actual wall-clock second) or it may
/// run faster or slower than real-time.
///
/// The clock keeps track of how many of its cycles have been
/// "consumed" by callers.  Callers requiring more clock cycles will
/// find that their calls to [`Clock::consume`] block so that their
/// average consumption of cycles matches the simulated clock rate.
///
/// On the other hand, if the simulated clock produces ticks very
/// rapidly (for example because it is set up to run 1,000,000x "real"
/// time) then callers will never block and hence can proceed as fast
/// as they are able.
pub trait Clock {
    /// Retrieves the current (simulated) time.
    fn now(&self) -> Duration;

    /// The caller calls `consume` to simulate the passing of a
    /// duration `interval`.  The returned value is the interval
    /// after which it is next OK to call `consume`.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::thread::sleep;
    /// use std::time::Duration;
    /// use cpu::Clock;
    ///
    /// fn g<C: Clock>(clk: &mut C) {
    ///   // We just performed an action which would have taken
    ///   // one millisecond on the simulated machine.
    ///   clk.consume(&Duration::from_millis(1));
    /// }
    /// ```
    fn consume(&mut self, inteval: &Duration);
}

/// BasicClock provides a simulated clock.
///
/// # Examples
/// ```
/// use std::time::Duration;
/// use cpu::BasicClock;
/// use cpu::Clock;
/// let mut clk = BasicClock::new();
/// clk.consume(&Duration::from_micros(12));
/// ```
///
///
#[derive(Debug)]
pub struct BasicClock {
    /// The host time at which the clock started running.  At this
    /// time, the "real" time and the "simulated" time coincided.  We
    /// periodically move the origin in order to avoid subtracting
    /// pairs of nearly-equal large numbers (which risks loss of
    /// precision).
    origin: Instant,

    /// Elapsed time as measured by the simulated clock.
    simulator_elapsed: Duration,
}

impl BasicClock {
    pub fn new() -> BasicClock {
        BasicClock {
            origin: Instant::now(),
            simulator_elapsed: Duration::new(0, 0),
        }
    }
}

impl Default for BasicClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for BasicClock {
    fn now(&self) -> Duration {
        self.simulator_elapsed
    }

    fn consume(&mut self, interval: &Duration) {
        self.simulator_elapsed += *interval;
    }
}

#[derive(Debug)]
struct SignedDuration {
    negative: bool,
    magnitude: Duration,
}

impl SignedDuration {
    pub const ZERO: SignedDuration = SignedDuration {
        negative: false,
        magnitude: Duration::ZERO,
    };
}

impl From<Duration> for SignedDuration {
    fn from(magnitude: Duration) -> Self {
        Self {
            negative: false,
            magnitude,
        }
    }
}

impl SignedDuration {
    fn checked_sub(&self, d: Duration) -> Option<SignedDuration> {
        if self.negative {
            self.magnitude.checked_add(d).map(|result| SignedDuration {
                negative: true,
                magnitude: result,
            })
        } else {
            match self.magnitude.checked_sub(d) {
                Some(diff) => Some(SignedDuration {
                    negative: false,
                    magnitude: diff,
                }),
                None => match d.checked_sub(self.magnitude) {
                    Some(diff) => Some(SignedDuration {
                        negative: true,
                        magnitude: diff,
                    }),
                    None => {
                        panic!(
                            "checked_sub inconsistent for {:?} - {:?}",
                            self.magnitude, d
                        );
                    }
                },
            }
        }
    }

    fn checked_add(&self, d: Duration) -> Option<SignedDuration> {
        if self.negative {
            match d.checked_sub(self.magnitude) {
                Some(diff) => Some(SignedDuration {
                    negative: false,
                    magnitude: diff,
                }),
                None => match self.magnitude.checked_sub(d) {
                    Some(diff) => Some(SignedDuration {
                        negative: true,
                        magnitude: diff,
                    }),
                    None => {
                        panic!(
                            "checked_add inconsistent for {:?} - {:?}",
                            self.magnitude, d
                        );
                    }
                },
            }
        } else {
            self.magnitude.checked_add(d).map(|tot| SignedDuration {
                negative: false,
                magnitude: tot,
            })
        }
    }
}

/// MinimalSleeper provides a facility for periodically sleeping such
/// that on average we sleep for the requested amount of time, even
/// though we don't necessarily sleep on every call.  The idea is to
/// be efficient in the use of system calls.
///
/// # Examples
/// ```
/// use std::time::Duration;
/// use cpu::MinimalSleeper;
/// // `s` will try to sleep, on average, for the indicated amounts
/// // of time, but will never sleep for less than 1 millisecond.
/// let mut s = MinimalSleeper::new(Duration::from_millis(10));
/// ```
#[derive(Debug)]
pub struct MinimalSleeper {
    /// Minimum period for which we will try to sleep.
    min_sleep: Duration,

    sleep_owed: SignedDuration,

    total_cumulative_sleep: Duration,
}

impl MinimalSleeper {
    pub fn new(min_sleep: Duration) -> MinimalSleeper {
        MinimalSleeper {
            min_sleep,
            sleep_owed: SignedDuration::ZERO,
            total_cumulative_sleep: Duration::ZERO,
        }
    }

    fn really_sleep(&mut self) {
        match self.sleep_owed {
            SignedDuration {
                negative: false,
                magnitude,
            } => {
                let then = Instant::now();
                event!(Level::DEBUG, "Sleeping for {:?}...", self.sleep_owed);
                sleep(magnitude);
                self.total_cumulative_sleep += magnitude;
                let now = Instant::now();
                let slept_for = now - then;
                event!(
                    Level::TRACE,
                    "MinimalSleeper: owed sleep is {:?}, actually slept for {:?}",
                    self.sleep_owed,
                    slept_for
                );
                match self.sleep_owed.checked_sub(slept_for) {
                    Some(remainder) => {
                        self.sleep_owed = remainder;
                    }
                    None => {
                        panic!("MinimalSleeper::really_sleep: overflow in sleep_owed");
                    }
                }
            }
            _ => unreachable!(), // should not have been called.
        }
        event!(
            Level::DEBUG,
            "MinimalSleeper: updated sleep debt is {:?}...",
            self.sleep_owed
        );
    }

    pub fn sleep(&mut self, duration: &Duration) {
        match self.sleep_owed.checked_add(*duration) {
            Some(more) => {
                self.sleep_owed = more;
                match self.sleep_owed {
                    SignedDuration {
                        negative: false,
                        magnitude,
                    } if magnitude > self.min_sleep => self.really_sleep(),
                    _ => {
                        // We did not build up enough sleep debt to exceed the
                        // threshold, or our sleep debt is negative.  Wait for
                        // more calls to sleep to bring us over the threshold.
                    }
                }
            }
            None => {
                panic!("MinimalSleeper::really_sleep: overflow in sleep_owed");
            }
        }
    }
}

impl Drop for MinimalSleeper {
    fn drop(&mut self) {
        event!(
            Level::INFO,
            "MinimalSleeper: drop: total cumulative sleep is {:?}",
            self.total_cumulative_sleep
        );
    }
}
