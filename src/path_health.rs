//! Health of one shared upstream path (a region's game endpoint, or the Global SDK), kept apart
//! from account health so that an outage of the path does not cool every account down.
//!
//! Consecutive path faults form a streak. A streak becomes the path's fault ("attributed") once
//! it involves a second account or any anonymous call; SDK paths attribute every fault. Until
//! then its faults are charged to the one account that saw them. An attributed streak of
//! `account_pool.failure_threshold` faults opens the path: new logical calls are refused before
//! any upstream contact, and one probe per interval is let through. Any gRPC answer closes it.
use crate::accounts::{Charge, PoolPolicy};
use serde_json::{json, Value};
use std::{
    sync::{Mutex, MutexGuard},
    time::{Duration, Instant},
};

/// Longest wait between probes of an open path.
const MAX_PROBE_INTERVAL: Duration = Duration::from_secs(5);

/// What one sent attempt says about the path.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Outcome {
    /// The upstream answered with a gRPC status (any status except a bare 14).
    Healthy,
    /// No usable answer; holds the stable error code for logs.
    Fault(&'static str),
}
/// A state change worth an operator log line; logged after the lock is released.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) enum Transition {
    #[default]
    None,
    Opened,
    ProbeFailed,
    Recovered,
}
#[derive(Debug, Default)]
pub(crate) struct Change {
    pub transition: Transition,
    /// The streak that has just been attributed to the path: its account charges are withdrawn.
    pub attributed: Option<u64>,
}
#[derive(Default)]
struct State {
    failures: u32,
    /// Number of the current (or last) streak; the first streak is 1.
    epoch: u64,
    first_source: Option<String>,
    attributed: bool,
    last_attributed: u64,
    open_until: Option<Instant>,
    probing: bool,
}
pub(crate) struct PathHealth {
    distinct_sources: bool,
    threshold: u32,
    interval: Duration,
    state: Mutex<State>,
}
/// Admission of one logical call. A probe admission is released when the ticket is dropped.
pub(crate) struct Ticket<'a> {
    path: &'a PathHealth,
    probe: bool,
}
impl Drop for Ticket<'_> {
    fn drop(&mut self) {
        if self.probe {
            self.path.state().probing = false;
        }
    }
}
impl PathHealth {
    /// `distinct_sources`: faults of a single account stay that account's until a second source
    /// appears. Without it (SDK), every fault is the path's.
    pub(crate) fn new(policy: &PoolPolicy, distinct_sources: bool) -> Self {
        Self {
            distinct_sources,
            threshold: policy.failure_threshold,
            interval: Duration::from_secs(policy.cooldown_seconds).min(MAX_PROBE_INTERVAL),
            state: Mutex::new(State::default()),
        }
    }
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
    pub(crate) fn interval(&self) -> Duration {
        self.interval
    }
    /// Records one attempt. `source` is the account that sent it; None for anonymous calls.
    pub(crate) fn record(&self, source: Option<&str>, outcome: Outcome) -> Change {
        let mut s = self.state();
        let mut change = Change::default();
        match outcome {
            Outcome::Healthy => {
                s.failures = 0;
                s.attributed = false;
                s.first_source = None;
                if s.open_until.take().is_some() {
                    change.transition = Transition::Recovered;
                }
            }
            Outcome::Fault(_) => {
                if s.failures == 0 {
                    s.epoch += 1;
                    s.first_source = None;
                    s.attributed = false;
                }
                s.failures = s.failures.saturating_add(1);
                if !s.attributed {
                    let path = match source {
                        Some(name) if self.distinct_sources => match &s.first_source {
                            None => {
                                s.first_source = Some(name.to_owned());
                                false
                            }
                            Some(first) => first != name,
                        },
                        _ => true,
                    };
                    if path {
                        s.attributed = true;
                        s.last_attributed = s.epoch;
                        change.attributed = Some(s.epoch);
                    }
                }
                if s.attributed && s.failures >= self.threshold {
                    let open = s.open_until.is_some();
                    s.open_until = Some(Instant::now() + self.interval);
                    change.transition = match (open, s.probing) {
                        (false, _) => Transition::Opened,
                        (true, true) => Transition::ProbeFailed,
                        (true, false) => Transition::None,
                    };
                }
            }
        }
        change
    }
    /// Admits a logical call: always while closed, and one probe at a time once the open
    /// interval has passed.
    pub(crate) fn admit(&self) -> Option<Ticket<'_>> {
        let mut s = self.state();
        let probe = match s.open_until {
            None => false,
            Some(until) if until > Instant::now() || s.probing => return None,
            Some(_) => true,
        };
        s.probing |= probe;
        Some(Ticket { path: self, probe })
    }
    /// Who pays for a path-class fault reported now.
    pub(crate) fn charge(&self) -> Charge {
        let s = self.state();
        if s.attributed || s.open_until.is_some() {
            Charge::Path
        } else {
            Charge::Account {
                streak: (s.failures > 0).then_some(s.epoch),
            }
        }
    }
    /// Whether streak `epoch` has been attributed to the path.
    pub(crate) fn attributed(&self, epoch: u64) -> bool {
        self.state().last_attributed == epoch
    }
    /// Operator view; carries no account names or identifiers.
    pub(crate) fn status(&self) -> Value {
        let s = self.state();
        let state = match (s.open_until, s.probing) {
            (None, _) => "closed",
            (Some(_), true) => "probing",
            (Some(_), false) => "open",
        };
        let remaining = s.open_until.map_or(0, |until| {
            until.saturating_duration_since(Instant::now()).as_millis() as u64
        });
        json!({"state":state,"failures":s.failures,"attributed":s.attributed,"cooldown_remaining_ms":remaining})
    }
    /// Ends the current open interval so the next admission is a probe.
    #[cfg(test)]
    pub(crate) fn expire_for_test(&self) {
        if let Some(until) = self.state().open_until.as_mut() {
            *until = Instant::now();
        }
    }
}
