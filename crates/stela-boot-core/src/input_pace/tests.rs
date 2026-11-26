extern crate std;
use super::*;
use std::string::ToString;

/// A 24 MHz counter, as `stela-boot`'s corrected count.
const PER_MICRO: u64 = 24;

const fn us(micros: u64) -> u64 {
    micros * PER_MICRO
}

const fn ms(millis: u64) -> u64 {
    us(millis * 1000)
}

/// A device last polled at `looked` that last delivered at `active`, with
/// looks costing `cost` ticks.
fn device(looked: u64, active: u64, cost: u64) -> Device {
    Device {
        looked,
        active,
        cost,
        delivered: None,
    }
}

/// A time long after any delivery: the device is idle there.
const IDLE: u64 = ms(60_000);

#[test]
fn an_idle_device_is_polled_at_most_every_idle_millis() {
    let idle = device(IDLE, 0, us(2));
    assert_eq!(idle.interval(IDLE, PER_MICRO), ms(IDLE_MILLIS));
    let mut turn = Turn::new(WAIT_MICROS);
    let waits = Waits::default();
    assert_eq!(
        idle.plan(IDLE + ms(IDLE_MILLIS) - 1, PER_MICRO, &mut turn, &waits),
        Plan::Later
    );
    assert_eq!(
        idle.plan(IDLE + ms(IDLE_MILLIS), PER_MICRO, &mut turn, &waits),
        Plan::Poll {
            late: 0,
            wait_micros: 0
        }
    );
}

#[test]
fn costly_looks_space_polls_within_their_bounds() {
    // An empty look that costs a millisecond (a hypervisor's port read).
    let costly = device(IDLE, 0, ms(1));
    assert_eq!(costly.interval(IDLE, PER_MICRO), ms(COST_SHARE));
    let ruinous = device(IDLE, 0, ms(100));
    assert_eq!(ruinous.interval(IDLE, PER_MICRO), ms(SLOW_MILLIS));
    // Active, the same looks are spaced no more than ACTIVE_MILLIS apart,
    // and cheap ones not at all.
    let active = device(IDLE, IDLE, ms(100));
    assert_eq!(active.interval(IDLE, PER_MICRO), ms(ACTIVE_MILLIS));
    let cheap = device(IDLE, IDLE, us(2));
    assert_eq!(cheap.interval(IDLE, PER_MICRO), us(64));
}

#[test]
fn the_reads_that_delivered_input_do_not_slow_the_device() {
    // A touchpad behind an interrupt pin: a pin read costs 2 us, a report's
    // read 750 us. Counting the reports held it to one read every 24 ms.
    let mut pad = Device::default();
    let mut now = IDLE;
    for _ in 0..16 {
        pad.polled(now, us(2), false, PER_MICRO);
        now += ms(1);
    }
    for _ in 0..64 {
        pad.polled(now, us(750), true, PER_MICRO);
        now += ms(8);
    }
    assert!(pad.active(now, PER_MICRO));
    assert!(
        pad.interval(now, PER_MICRO) <= us(64),
        "{}",
        pad.interval(now, PER_MICRO)
    );
    // Empty looks that cost as much do slow it, as before.
    for _ in 0..64 {
        pad.polled(now, us(750), false, PER_MICRO);
        now += ms(1);
    }
    assert!(pad.interval(now, PER_MICRO) >= ms(20));
}

#[test]
fn a_due_poll_waits_only_once_late_and_once_a_turn() {
    let waits = Waits::default();
    let looked = ms(1000);
    let pad = device(looked, looked, 0);
    let other = device(looked, looked, 0);
    // Due but not yet late enough: only a try.
    let mut turn = Turn::new(WAIT_MICROS);
    let early = looked + us(LATE_WAIT_MICROS) - 1;
    assert_eq!(
        pad.plan(early, PER_MICRO, &mut turn, &waits),
        Plan::Poll {
            late: early - looked,
            wait_micros: 0
        }
    );
    // Late: the first device of the turn waits, the next only tries.
    let late = looked + us(LATE_WAIT_MICROS);
    let mut turn = Turn::new(WAIT_MICROS);
    assert_eq!(
        pad.plan(late, PER_MICRO, &mut turn, &waits),
        Plan::Poll {
            late: us(LATE_WAIT_MICROS),
            wait_micros: WAIT_MICROS
        }
    );
    assert_eq!(
        other.plan(late, PER_MICRO, &mut turn, &waits),
        Plan::Poll {
            late: us(LATE_WAIT_MICROS),
            wait_micros: 0
        }
    );
    // A step's turn waits less.
    let mut step = Turn::new(wait_micros(true));
    assert_eq!(
        pad.plan(late, PER_MICRO, &mut step, &waits),
        Plan::Poll {
            late: us(LATE_WAIT_MICROS),
            wait_micros: STEP_WAIT_MICROS
        }
    );
}

#[test]
fn a_wait_the_holder_outlasted_is_not_tried_again_for_the_backoff() {
    let looked = ms(1000);
    let pad = device(looked, looked, 0);
    let mut waits = Waits::default();
    let late = looked + ms(3);
    waits.outlasted(late, PER_MICRO);
    let mut turn = Turn::new(WAIT_MICROS);
    let within = late + us(WAIT_BACKOFF_MICROS) - 1;
    assert!(matches!(
        pad.plan(within, PER_MICRO, &mut turn, &waits),
        Plan::Poll { wait_micros: 0, .. }
    ));
    let after = late + us(WAIT_BACKOFF_MICROS);
    assert!(matches!(
        pad.plan(after, PER_MICRO, &mut turn, &waits),
        Plan::Poll {
            wait_micros: WAIT_MICROS,
            ..
        }
    ));
}

#[test]
fn the_backoff_holds_across_the_counters_wrap() {
    let mut waits = Waits::default();
    let at = u64::MAX - us(100);
    waits.outlasted(at, PER_MICRO);
    assert!(!waits.allowed(at.wrapping_add(us(WAIT_BACKOFF_MICROS) - 1)));
    assert!(waits.allowed(at.wrapping_add(us(WAIT_BACKOFF_MICROS))));
}

#[test]
fn gaps_count_only_within_a_motion() {
    let mut pad = Device::default();
    let start = ms(10_000);
    assert_eq!(pad.polled(start, 0, true, PER_MICRO), None);
    assert_eq!(pad.polled(start + ms(8), 0, true, PER_MICRO), Some(ms(8)));
    // An empty poll between keeps the gap from the last delivery.
    assert_eq!(pad.polled(start + ms(12), 0, false, PER_MICRO), None);
    assert_eq!(pad.polled(start + ms(16), 0, true, PER_MICRO), Some(ms(8)));
    // A pause between two motions is no gap.
    assert_eq!(
        pad.polled(start + ms(16 + GAP_MILLIS), 0, true, PER_MICRO),
        None
    );
}

#[test]
fn a_spread_reports_bucket_bounds_and_its_largest_time() {
    let mut spread = Spread::default();
    assert_eq!(spread.percentile(50), None);
    assert_eq!(spread.to_string(), "-");
    for _ in 0..90 {
        spread.add(700);
    }
    for _ in 0..9 {
        spread.add(3000);
    }
    spread.add(31_000);
    assert_eq!(spread.count(), 100);
    assert_eq!(spread.percentile(50), Some(1000));
    assert_eq!(spread.percentile(95), Some(4000));
    assert_eq!(spread.percentile(100), Some(31_000));
    assert_eq!(spread.to_string(), "1000+4000+31000");
    // A bound above every time is the largest time; the open bucket too.
    let mut one = Spread::default();
    one.add(9);
    assert_eq!(one.to_string(), "9+9+9");
    let mut huge = Spread::default();
    huge.add(500_000);
    assert_eq!(huge.percentile(50), Some(500_000));
}

/// A fake host-drivers lock: the GPUs' pump runs Linux in steps of
/// `step_us`, `steps` of them every `period_us`, and one step of `long_us`
/// at the start of every second. A waiter spinning for the lock is let in
/// at the end of the step in progress. `None` while free, else the end of
/// the step that holds it.
#[derive(Clone, Copy)]
struct Pump {
    period_us: u64,
    steps: u64,
    step_us: u64,
    long_us: u64,
}

impl Pump {
    fn holds(&self, now: u64) -> Option<u64> {
        let second = now - now % ms(1000);
        if now - second < us(self.long_us) {
            return Some(second + us(self.long_us));
        }
        let start = now - now % us(self.period_us);
        let into = now - start;
        (into < us(self.steps * self.step_us))
            .then(|| start + (into / us(self.step_us) + 1) * us(self.step_us))
    }
}

/// What a touchpad behind an interrupt pin gave the guest over a run.
struct Outcome {
    reports: u64,
    read: u64,
    /// From a report's readiness to its read.
    latency: Spread,
    gaps: Spread,
    spun_us: u64,
}

/// Run service turns every `turn_us` for `seconds` against `pump`, with a
/// touchpad that has a report ready every `report_us`: its pin read costs
/// 2 us and a report's read 750 us.
fn run(pump: Pump, turn_us: u64, report_us: u64, seconds: u64) -> Outcome {
    let mut pad = Device::default();
    let mut waits = Waits::default();
    let mut outcome = Outcome {
        reports: 0,
        read: 0,
        latency: Spread::default(),
        gaps: Spread::default(),
        spun_us: 0,
    };
    // The next report not yet read.
    let mut next_report = us(report_us);
    let end = ms(seconds * 1000);
    let mut now = us(turn_us);
    while now < end {
        let mut turn = Turn::new(WAIT_MICROS);
        if let Plan::Poll { wait_micros, .. } = pad.plan(now, PER_MICRO, &mut turn, &waits) {
            let entered = match pump.holds(now) {
                None => Some(now),
                Some(step_end) if wait_micros != 0 => {
                    if step_end - now <= us(wait_micros) {
                        outcome.spun_us += (step_end - now) / PER_MICRO;
                        Some(step_end)
                    } else {
                        outcome.spun_us += wait_micros;
                        waits.outlasted(now, PER_MICRO);
                        None
                    }
                }
                Some(_) => None,
            };
            if let Some(at) = entered {
                let mut delivered = false;
                while next_report <= at {
                    outcome.latency.add((at - next_report) / PER_MICRO);
                    outcome.read += 1;
                    next_report += us(report_us);
                    delivered = true;
                }
                let took = if delivered { us(750) } else { us(2) };
                if let Some(gap) = pad.polled(now, took, delivered, PER_MICRO) {
                    outcome.gaps.add(gap / PER_MICRO);
                }
            }
        }
        now += us(turn_us);
    }
    outcome.reports = end / us(report_us);
    outcome
}

const FREE: Pump = Pump {
    period_us: 1000,
    steps: 0,
    step_us: 1,
    long_us: 0,
};

#[test]
fn a_free_lock_reads_every_report_within_a_turn() {
    let outcome = run(FREE, 500, 8000, 10);
    assert!(outcome.read + 1 >= outcome.reports);
    assert!(outcome.latency.percentile(95).unwrap() <= 500);
    assert_eq!(outcome.gaps.percentile(50), Some(8000));
    assert_eq!(outcome.spun_us, 0);
}

#[test]
fn a_busy_pump_in_short_steps_leaves_reads_at_the_reports_rate() {
    // Half the time held, in Linux steps of 250 us, the pump's own mean.
    let pump = Pump {
        period_us: 2000,
        steps: 4,
        step_us: 250,
        long_us: 0,
    };
    let outcome = run(pump, 500, 8000, 10);
    assert!(outcome.read + 1 >= outcome.reports);
    assert!(outcome.latency.percentile(95).unwrap() <= 2000);
    assert!(outcome.gaps.percentile(95).unwrap() <= 16000);
    // Rate: at least 120 reads a second over the run.
    assert!(outcome.read >= 1200, "{}", outcome.read);
}

#[test]
fn a_long_linux_step_is_waited_for_a_few_times_not_spun_through() {
    // The pump held three fifths of the time in 250 us steps, as in the
    // busiest tenth of the logs' windows, and one 25 ms Linux step a second,
    // which nothing can shorten.
    let pump = Pump {
        period_us: 2500,
        steps: 6,
        step_us: 250,
        long_us: 25_000,
    };
    let outcome = run(pump, 500, 8000, 10);
    assert!(outcome.read + 1 >= outcome.reports);
    assert!(outcome.latency.percentile(50).unwrap() <= 1000);
    assert!(outcome.latency.percentile(95).unwrap() <= 2000);
    let short = run(Pump { long_us: 0, ..pump }, 500, 8000, 10);
    // The short steps cost the turns' waits little, and each long step
    // at most four outlasted waits of WAIT_MICROS, one per backoff.
    assert!(short.spun_us <= 10 * 20_000, "{}", short.spun_us);
    let long = outcome.spun_us - short.spun_us;
    assert!(long <= 10 * 4 * WAIT_MICROS, "{long}");
}

#[test]
fn a_lock_held_nine_tenths_still_gives_every_report_at_a_bounded_spin() {
    // 900 us steps with 100 us free between: tries mostly fail, and a late
    // poll waits out about half a step. Every report is still read within
    // a few milliseconds, at about a quarter of an idle PE's time.
    let pump = Pump {
        period_us: 1000,
        steps: 1,
        step_us: 900,
        long_us: 0,
    };
    let outcome = run(pump, 500, 8000, 10);
    assert!(outcome.read + 1 >= outcome.reports);
    assert!(outcome.latency.percentile(50).unwrap() <= 2000);
    assert!(outcome.latency.percentile(95).unwrap() <= 4000);
    assert!(outcome.spun_us <= 10 * 300_000, "{}", outcome.spun_us);
}
