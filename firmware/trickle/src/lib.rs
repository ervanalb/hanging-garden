#![cfg_attr(not(feature = "impl-std"), no_std)]

#[cfg(any(feature = "impl-std", feature = "impl-embassy"))]
use oorandom::Rand32;

#[cfg(feature = "impl-std")]
use std::time::Duration;
#[cfg(feature = "impl-std")]
use std::time::Instant;

#[cfg(feature = "impl-embassy")]
use embassy_time::Duration;
#[cfg(feature = "impl-embassy")]
use embassy_time::Instant;

use core::cmp::Ordering;

#[derive(Debug)]
pub struct TrickleParams {
    pub i_min_micros: u64,
    pub i_max_micros: u64,
    pub k: u32,
}

#[cfg(any(feature = "impl-std", feature = "impl-embassy"))]
#[derive(Clone, Debug)]
pub struct TrickleState<'a> {
    params: &'a TrickleParams,
    rng: Rand32,
    interval_micros: u64,
    counter: u32,
    t_expiry: Instant,
    interval_expiry: Instant,
    after_t: bool,
}

#[cfg(any(feature = "impl-std", feature = "impl-embassy"))]
pub enum TricklePollResult {
    /// Data should be broadcast out immediately
    Send,
    /// Timeout in microseconds with how long until poll should be called again
    /// (max time to wait--can be called sooner.)
    Wait(u64),
}

#[cfg(any(feature = "impl-std", feature = "impl-embassy"))]
impl<'a> TrickleState<'a> {
    pub fn new(params: &'a TrickleParams, now: Instant, rng_seed: u64) -> Self {
        let mut result = TrickleState {
            params,
            rng: Rand32::new(rng_seed),
            interval_micros: params.i_min_micros,
            counter: 0,
            t_expiry: now,        // set by `begin_interval()`
            interval_expiry: now, // set by `begin_interval()`
            after_t: false,
        };
        result.begin_interval(now);
        result
    }

    fn begin_interval(&mut self, now: Instant) {
        self.counter = 0;

        // Divide down to approximately milliseconds to avoid having to keep around a 64-bit RNG.
        // All rounding is towards zero, so this may generate a t value which is
        // slightly below I / 2 (ok) but never above I (would be bad.)
        let interval_millis_ish = (self.interval_micros / 1024) as u32;
        let t_millis_ish =
            self.rng
                .rand_range(interval_millis_ish / 2..interval_millis_ish) as u64;
        self.t_expiry = now + Duration::from_micros(t_millis_ish as u64 * 1024);
        self.interval_expiry = now + Duration::from_micros(self.interval_micros);
        self.after_t = false;
    }

    fn double_interval(&mut self) {
        self.interval_micros = (self.interval_micros * 2).min(self.params.i_max_micros);
    }

    fn reset_interval(&mut self) {
        self.interval_micros = self.params.i_min_micros;
    }

    /// Advance the trickle algorithm.
    pub fn poll(&mut self, now: Instant) -> TricklePollResult {
        if !self.after_t {
            if now >= self.t_expiry {
                // Handle timer expiry
                self.after_t = true;

                if self.counter < self.params.k {
                    TricklePollResult::Send
                } else {
                    let timeout = (self.interval_expiry - now).as_micros() as u64;
                    TricklePollResult::Wait(timeout)
                }
            } else {
                let timeout = (self.t_expiry - now).as_micros() as u64;
                TricklePollResult::Wait(timeout)
            }
        } else {
            if now >= self.interval_expiry {
                self.double_interval();
                self.begin_interval(now);
                let timeout = (self.t_expiry - now).as_micros() as u64;
                TricklePollResult::Wait(timeout)
            } else {
                let timeout = (self.interval_expiry - now).as_micros() as u64;
                TricklePollResult::Wait(timeout)
            }
        }
    }

    /// Takes appropriate trickle actions given that we are assuming newer state.
    /// The polling loop should be woken.
    pub fn got_new_state(&mut self, now: Instant) {
        self.reset_interval();
        self.begin_interval(now);
    }

    /// Takes appropriate trickle actions given that we received a consistent (redundant) state.
    /// (The polling loop does not need to be woken.)
    pub fn got_consistent_state(&mut self) {
        self.counter += 1;
    }

    /// Takes appropriate trickle actions given that we received an outdated state.
    /// The polling loop should be woken.
    pub fn got_outdated_state(&mut self, now: Instant) {
        self.reset_interval();
        self.begin_interval(now);
    }
}

pub trait TrickleOrd {
    /// Note that comparison cannot necessarily be reversed.
    /// a.consider(b).reverse() is generally NOT the same as b.consider(a).
    fn consider(&self, other: &Self) -> TrickleOrdering;
}

#[repr(i8)]
pub enum TrickleOrdering {
    /// State is less and inconsistent (e.g. outdated)
    Less = -1,
    /// State is equal, or less but consistent
    Consistent = 0,
    /// State is greater (e.g. newer)
    Greater = 1,
}

impl TrickleOrdering {
    pub fn then(self, other: TrickleOrdering) -> TrickleOrdering {
        match self {
            Self::Less => Self::Less,
            Self::Consistent => other,
            Self::Greater => Self::Greater,
        }
    }

    pub fn then_with<F: FnOnce() -> TrickleOrdering>(self, f: F) -> TrickleOrdering {
        match self {
            Self::Less => Self::Less,
            Self::Consistent => f(),
            Self::Greater => Self::Greater,
        }
    }

    /*
    pub fn max(cmp: Ordering) -> TrickleOrdering {
        match cmp {
            Ordering::Less | Ordering::Equal => Self::Consistent,
            Ordering::Greater => Self::Greater,
        }
    }

    pub fn min(cmp: Ordering) -> TrickleOrdering {
        match cmp {
            Ordering::Greater | Ordering::Equal => Self::Consistent,
            Ordering::Less => Self::Greater,
        }
    }
    */
}

impl From<Ordering> for TrickleOrdering {
    fn from(value: Ordering) -> Self {
        match value {
            Ordering::Less => Self::Less,
            Ordering::Equal => Self::Consistent,
            Ordering::Greater => Self::Greater,
        }
    }
}
