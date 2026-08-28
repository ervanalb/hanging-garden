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
    pub i_min_millis: u32,
    pub i_max_millis: u32,
    pub k: u32,
}

#[cfg(any(feature = "impl-std", feature = "impl-embassy"))]
#[derive(Clone, Debug)]
pub struct TrickleState<'a, S> {
    params: &'a TrickleParams,
    rng: Rand32,
    state: S,
    interval_millis: u32,
    counter: u32,
    t_expiry: Instant,
    interval_expiry: Instant,
    after_t: bool,
}

#[cfg(any(feature = "impl-std", feature = "impl-embassy"))]
pub enum TricklePollResult {
    /// Data should be broadcast out immediately
    Send,
    /// Timeout in milliseconds with how long until poll should be called again
    /// (max time to wait--can be called sooner.)
    Wait(u32),
}

#[cfg(any(feature = "impl-std", feature = "impl-embassy"))]
impl<'a, S: Default + Clone + TrickleOrd> TrickleState<'a, S> {
    pub fn new(params: &'a TrickleParams, now: Instant, rng_seed: u64) -> Self {
        let mut result = TrickleState {
            params,
            rng: Rand32::new(rng_seed),
            state: S::default(),
            interval_millis: params.i_min_millis,
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

        self.t_expiry = now
            + Duration::from_millis(
                self.rng
                    .rand_range(self.interval_millis / 2..self.interval_millis)
                    as u64,
            );
        self.interval_expiry = now + Duration::from_millis(self.interval_millis as u64);
        self.after_t = false;
    }

    fn double_interval(&mut self) {
        self.interval_millis = (self.interval_millis * 2).min(self.params.i_max_millis);
    }

    fn reset_interval(&mut self) {
        self.interval_millis = self.params.i_min_millis;
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
                    let timeout = (self.interval_expiry - now).as_millis() as u32;
                    TricklePollResult::Wait(timeout)
                }
            } else {
                let timeout = (self.t_expiry - now).as_millis() as u32;
                TricklePollResult::Wait(timeout)
            }
        } else {
            if now >= self.interval_expiry {
                self.double_interval();
                self.begin_interval(now);
                let timeout = (self.t_expiry - now).as_millis() as u32;
                TricklePollResult::Wait(timeout)
            } else {
                let timeout = (self.interval_expiry - now).as_millis() as u32;
                TricklePollResult::Wait(timeout)
            }
        }
    }

    /// Merge in a new state that we have received, and update the trickle algorithm accordingly.
    /// Returns whether the trickle loop should be woken
    /// due to a potential change in timeout length.
    pub fn receive_state(&mut self, now: Instant, new_state: &S) -> bool {
        match self.state.consider(new_state) {
            TrickleOrdering::Greater => {
                self.set_state(now, new_state);
                true
            }
            TrickleOrdering::Consistent => {
                self.counter += 1;
                false
            }
            TrickleOrdering::Less => {
                // Receiving an outdated state that is not consistent
                // means we should broadcast out again soon,
                // to bring our neighbor up to date.
                self.reset_interval();
                self.begin_interval(now);
                true
            }
        }
    }

    /// Set a new state and update the trickle algorithm accordingly.
    pub fn set_state(&mut self, now: Instant, new_state: &S) {
        self.state = new_state.clone();
        self.reset_interval();
        self.begin_interval(now);
    }

    pub fn state(&self) -> &S {
        &self.state
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
