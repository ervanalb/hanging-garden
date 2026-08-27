use oorandom::Rand32;

#[derive(Debug)]
pub struct TrickleParams {
    pub i_min_millis: u32,
    pub i_max_millis: u32,
    pub k: u32,
}

pub trait TrickleInstant: Copy + PartialOrd {
    fn plus_millis(self, millis: u32) -> Self;
    fn millis_until(self, other: Self) -> u32;
}

#[derive(Clone, Debug)]
pub struct TrickleState<'a, S, Instant> {
    params: &'a TrickleParams,
    rng: Rand32,
    state: S,
    interval_millis: u32,
    counter: u32,
    t_expiry: Instant,
    interval_expiry: Instant,
    after_t: bool,
}

pub enum TricklePollResult {
    /// Data should be broadcast out immediately
    Send,
    /// Timeout in milliseconds with how long until poll should be called again
    /// (max time to wait--can be called sooner.)
    Wait(u32),
}

impl<'a, S: Default + Clone + TrickleOrd, Instant: TrickleInstant> TrickleState<'a, S, Instant> {
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

        self.t_expiry = now.plus_millis(
            self.rng
                .rand_range(self.interval_millis / 2..self.interval_millis),
        );
        self.interval_expiry = now.plus_millis(self.interval_millis);
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
                    let timeout = now.millis_until(self.interval_expiry);
                    TricklePollResult::Wait(timeout)
                }
            } else {
                let timeout = now.millis_until(self.t_expiry);
                TricklePollResult::Wait(timeout)
            }
        } else {
            if now >= self.interval_expiry {
                self.double_interval();
                self.begin_interval(now);
                let timeout = now.millis_until(self.t_expiry);
                TricklePollResult::Wait(timeout)
            } else {
                let timeout = now.millis_until(self.interval_expiry);
                TricklePollResult::Wait(timeout)
            }
        }
    }

    /// Merge in a new state that we have received, and update the trickle algorithm accordingly.
    /// Returns whether the new state was accepted.
    pub fn receive_state(&mut self, now: Instant, new_state: &S) -> bool {
        match self.state.consider(new_state) {
            TrickleOrdering::Greater | TrickleOrdering::GreaterConsistent => {
                self.set_state(now, new_state);
                true
            }
            TrickleOrdering::Equal | TrickleOrdering::LessConsistent => {
                self.counter += 1;
                false
            }
            TrickleOrdering::Less => {
                // Receiving an outdated state that is not consistent
                // means we should broadcast out again soon,
                // to bring our neighbor up to date.
                self.reset_interval();
                self.begin_interval(now);
                false
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
    fn consider(&self, other: &Self) -> TrickleOrdering;
}

pub enum TrickleOrdering {
    Greater,           // State is greater (e.g. fresher)
    GreaterConsistent, // State is newer but consistent
    Equal,
    LessConsistent, // State is older but consistent
    Less,           // State is lesser (e.g. outdated)
}
