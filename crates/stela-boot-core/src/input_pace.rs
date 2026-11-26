//! When `stela-boot`'s input service polls each host keyboard and pointer
//! (`engine::input`), how a due poll meets the host drivers' lock another
//! processor holds, and the spread of the polls' lateness and of the gaps
//! between the inputs they deliver.
//!
//! Every poll runs under the host drivers' lock, which the GPUs' pump holds
//! while Linux runs, a step at a time, and lets go after its step once a
//! core spins for it. A device is due once its interval has passed since
//! its last poll. A due poll that finds the lock taken tries again at the
//! next service turn; once it is [`LATE_WAIT_MICROS`] late it spins for
//! the lock instead, as a waiter the pump lets in after its step, for the
//! turn's [`wait_micros`] at most, once a turn. A wait the holder outlasted
//! (one Linux step can take tens of milliseconds) is not tried again for
//! [`WAIT_BACKOFF_MICROS`], so a poll never spins through a long step.
//!
//! A device's interval is its looks' cost: a poll that delivered nothing
//! costs the boot PE what it took, and polls are spaced [`COST_SHARE`] times
//! that recent cost apart, between [`IDLE_MILLIS`] and [`SLOW_MILLIS`] when
//! idle and at most [`ACTIVE_MILLIS`] for [`ACTIVE_SECONDS`] after its last
//! input. A poll that delivered input is not counted: its time is the
//! input's own transfer, which an I2C touchpad behind an interrupt pin pays
//! only when its pin says a report is ready (about 0.75 ms for a report at
//! 400 kHz), so counting it held a moving finger to one read every 32 ms.

/// A device that gave no input for [`ACTIVE_SECONDS`] is polled this often
/// at most. A poll can be a port or register access a hypervisor takes to
/// its device model; USB low-speed keyboards and mice are polled as rarely.
pub const IDLE_MILLIS: u64 = 8;
/// How long a device is polled at every service turn after it last gave
/// input, so a pointer's motion keeps its full rate.
pub const ACTIVE_SECONDS: u64 = 5;
/// A device's looks may take at most one part in this of the boot PE: a
/// look costing more is spaced this many times its cost apart. A port read
/// that costs a microsecond on hardware can wait milliseconds for a
/// hypervisor's device-model lock; the boot PE's time is the guest's.
pub const COST_SHARE: u64 = 32;
/// The longest an idle device waits for its next poll however costly its
/// looks are, which its first input can wait.
pub const SLOW_MILLIS: u64 = 250;
/// The longest an active device waits for its next poll.
pub const ACTIVE_MILLIS: u64 = 32;
/// How late a due poll may run before it waits for a taken lock.
pub const LATE_WAIT_MICROS: u64 = 2000;
/// How long no poll waits for the lock after a wait the holder outlasted.
pub const WAIT_BACKOFF_MICROS: u64 = 8000;
/// The longest wait of a turn on a PE whose guest code waits for it (the
/// boot PE's step, which holds every PE's time), and of any other turn: a
/// waiting PE's or the host core's, whose guest is idle. The GPUs' pump's
/// Linux steps take about 0.2 ms on average.
pub const STEP_WAIT_MICROS: u64 = 250;
pub const WAIT_MICROS: u64 = 1000;
/// Gaps between a device's deliveries longer than this are pauses between
/// motions, not the rate of one, and are left out of its gap spread.
pub const GAP_MILLIS: u64 = 250;

/// The longest wait for the lock in a turn ([`STEP_WAIT_MICROS`]).
pub const fn wait_micros(step: bool) -> u64 {
    if step { STEP_WAIT_MICROS } else { WAIT_MICROS }
}

/// One device's pace, in counter ticks of `per_micro` a microsecond.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Device {
    /// The counter at its last poll.
    looked: u64,
    /// The counter at its last poll that delivered input.
    active: u64,
    /// Its looks' recent cost: an eighth of each poll that delivered
    /// nothing joins seven eighths of the last.
    cost: u64,
    /// The counter at its last delivery, for the gap spread.
    delivered: Option<u64>,
}

/// What a turn does with a device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Plan {
    /// Not due yet.
    Later,
    /// Due, `late` ticks past its interval: take the lock if it is free,
    /// else spin for it for `wait_micros` at most (none when zero).
    Poll { late: u64, wait_micros: u64 },
}

/// One turn's waits: at most one, of at most `wait_micros`.
#[derive(Clone, Copy, Debug)]
pub struct Turn {
    wait_micros: u64,
    waited: bool,
}

impl Turn {
    pub const fn new(wait_micros: u64) -> Self {
        Self {
            wait_micros,
            waited: false,
        }
    }
}

/// The waits across turns: after one the holder outlasted, none until the
/// counter passes `quiet_until`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Waits {
    quiet_until: Option<u64>,
}

impl Waits {
    fn allowed(&self, now: u64) -> bool {
        // The counter may wrap: the quiet time is over once `now` is no
        // more than half the counter's range past it.
        self.quiet_until
            .is_none_or(|until| now.wrapping_sub(until) < u64::MAX / 2)
    }

    /// A wait that began at `now` found the lock still held at its end.
    pub fn outlasted(&mut self, now: u64, per_micro: u64) {
        self.quiet_until = Some(now.wrapping_add(per_micro * WAIT_BACKOFF_MICROS));
    }
}

impl Device {
    /// Whether it delivered input within [`ACTIVE_SECONDS`] of `now`.
    pub fn active(&self, now: u64, per_micro: u64) -> bool {
        now.wrapping_sub(self.active) < per_micro * 1_000_000 * ACTIVE_SECONDS
    }

    /// The ticks between its polls at `now`.
    pub fn interval(&self, now: u64, per_micro: u64) -> u64 {
        let millis = per_micro * 1000;
        let spaced = self.cost.saturating_mul(COST_SHARE);
        if self.active(now, per_micro) {
            spaced.min(millis * ACTIVE_MILLIS)
        } else {
            spaced.clamp(millis * IDLE_MILLIS, millis * SLOW_MILLIS)
        }
    }

    /// What `turn` does with this device at `now`.
    pub fn plan(&self, now: u64, per_micro: u64, turn: &mut Turn, waits: &Waits) -> Plan {
        let since = now.wrapping_sub(self.looked);
        let interval = self.interval(now, per_micro);
        if since < interval {
            return Plan::Later;
        }
        let late = since - interval;
        let wait = late >= per_micro * LATE_WAIT_MICROS
            && !turn.waited
            && turn.wait_micros != 0
            && waits.allowed(now);
        if wait {
            turn.waited = true;
        }
        Plan::Poll {
            late,
            wait_micros: if wait { turn.wait_micros } else { 0 },
        }
    }

    /// A poll at `now` took `took` ticks inside the lock and `delivered`
    /// input or not: the gap since its last delivery, when it delivered
    /// within [`GAP_MILLIS`] of it.
    pub fn polled(&mut self, now: u64, took: u64, delivered: bool, per_micro: u64) -> Option<u64> {
        self.looked = now;
        if !delivered {
            self.cost = self.cost - self.cost / 8 + took / 8;
            return None;
        }
        self.active = now;
        let gap = self
            .delivered
            .map(|at| now.wrapping_sub(at))
            .filter(|gap| *gap < per_micro * 1000 * GAP_MILLIS);
        self.delivered = Some(now);
        gap
    }
}

/// The upper bounds of [`Spread`]'s buckets, in microseconds; the last
/// bucket is open.
const BOUNDS_US: [u64; 11] = [
    125, 250, 500, 1000, 2000, 4000, 8000, 16000, 32000, 64000, 128000,
];

/// How a set of times spread, kept as counts in doubling buckets.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Spread {
    counts: [u32; BOUNDS_US.len() + 1],
    max_us: u64,
}

impl Spread {
    pub fn add(&mut self, us: u64) {
        let bucket = BOUNDS_US
            .iter()
            .position(|bound| us <= *bound)
            .unwrap_or(BOUNDS_US.len());
        self.counts[bucket] = self.counts[bucket].saturating_add(1);
        self.max_us = self.max_us.max(us);
    }

    pub fn count(&self) -> u64 {
        self.counts.iter().map(|count| u64::from(*count)).sum()
    }

    /// The time at or below which `percent` of the times lie: the upper
    /// bound of the bucket that holds that rank, or the largest time when
    /// that is smaller or the bucket is the open one. `None` when empty.
    pub fn percentile(&self, percent: u64) -> Option<u64> {
        let count = self.count();
        if count == 0 {
            return None;
        }
        let rank = (count * percent.min(100)).div_ceil(100).max(1);
        let mut seen = 0;
        for (bucket, n) in self.counts.iter().enumerate() {
            seen += u64::from(*n);
            if seen >= rank {
                let bound = BOUNDS_US.get(bucket).copied().unwrap_or(u64::MAX);
                return Some(bound.min(self.max_us));
            }
        }
        Some(self.max_us)
    }
}

/// `p50+p95+max` in microseconds, `-` when empty.
impl core::fmt::Display for Spread {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match (self.percentile(50), self.percentile(95)) {
            (Some(median), Some(high)) => write!(f, "{median}+{high}+{}", self.max_us),
            _ => f.write_str("-"),
        }
    }
}

#[cfg(test)]
mod tests;
