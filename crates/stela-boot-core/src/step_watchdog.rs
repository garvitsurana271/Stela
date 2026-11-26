//! The Linux task step watchdog's decisions. The host's Linux environment
//! runs driver work in bounded steps on the boot processor; a step that
//! never returns would leave the PC hanging with no stop record. While that
//! processor is the only one running (the GPU's start, before the other
//! processors start), a core cycle counter armed to overflow after
//! [`LIMIT_SECONDS`] raises an NMI there. Each step that returns, or the
//! dispatcher going idle, puts the counter back once a second at most
//! ([`next_rearm`]): the NMI comes only when nothing returned for the
//! whole limit, and its handler names the stuck step ([`attribute`],
//! [`message`]) with the interrupted instruction pointer.
//!
//! The counter counts unhalted core cycles, whose rate follows the core's
//! clock, so it does not time the limit itself: it overflows after
//! [`CHECK_SECONDS`] of cycles at the time-stamp counter's rate
//! ([`start_value`]), and an overflow that comes before the limit by the
//! time-stamp counter puts it back for what is left of the limit, its NMI
//! returning ([`check_in`]). The stop then comes within [`CHECK_FLOOR_MS`]
//! of the limit on a core at or above its nominal clock, and within one
//! check of it below, where 0.6.1 counted three times the limit's cycles at
//! once and stopped after 45 to 90 s while saying 30 s (65 s on an i5-10500).
//!
//! Once the other processors run, the counter is off (`--profile` may use
//! it) and they watch instead: each of them, between two slices of its
//! guest code, reads which step the boot processor runs ([`look`]). A step
//! that every look finds running for the whole limit, by the watcher's own
//! clock (the processors' time-stamp counters may differ by seconds), is
//! stuck: the watcher sends the boot processor an NMI, whose handler names
//! the step ([`caught`]) with the instruction pointer it interrupted. The
//! boot processor itself pays nothing more than the step's own stores: no
//! NMI goes to a processor that runs no Linux step, as a guest's or a
//! parked one does not.
use crate::first_stop::FirstStop;
use alloc::{format, string::String};

/// How long no step may return before the watchdog stops the boot: half of
/// the GPU start's whole budget (60 s), far past nouveau's longest wait
/// inside one step (its 2 s register and firmware polls), and far below
/// the five minutes a reporter waited at a black screen.
pub const LIMIT_SECONDS: u64 = 30;

/// How often at most a returned step puts the counter back.
pub const REARM_SECONDS: u64 = 1;

/// How many seconds of cycles at the time-stamp counter's rate the counter
/// counts before its NMI looks at the time ([`check_in`]): a core at a
/// turbo clock overflows sooner and one below its nominal clock later, so
/// the look, not the count, decides when the limit has passed.
pub const CHECK_SECONDS: u64 = 5;

/// The fewest cycles a check puts the counter back for, in milliseconds at
/// the time-stamp counter's rate: a core above its nominal clock overflows
/// before each rest of the limit runs out, and this bounds how many such
/// checks it takes and how late past the limit the last one comes.
pub const CHECK_FLOOR_MS: u64 = 500;

/// The value a `width`-bit up-counter starts from to overflow after
/// [`CHECK_SECONDS`] of cycles at `tsc_hz`; `None` when the count does not
/// fit or the rate is unknown.
pub fn start_value(tsc_hz: u64, width: u32) -> Option<u64> {
    counter_start(tsc_hz.checked_mul(CHECK_SECONDS)?, width)
}

/// The value a `width`-bit up-counter starts from to overflow after
/// `cycles`; `None` when they do not fit.
pub fn counter_start(cycles: u64, width: u32) -> Option<u64> {
    if !(32..=64).contains(&width) {
        return None;
    }
    let mask = u64::MAX >> (64 - width);
    if cycles == 0 || cycles > mask {
        return None;
    }
    Some((mask - cycles).wrapping_add(1) & mask)
}

/// What the counter's overflow NMI at `now` does, the counter last put back
/// by a returned step at `armed_at`, all by the time-stamp counter at
/// `tsc_hz`: before the limit, the cycles to count again (what is left of
/// the limit, at most [`CHECK_SECONDS`] and at least [`CHECK_FLOOR_MS`]),
/// the NMI then returning; `None` once nothing returned for the whole
/// limit, and the NMI is the watchdog's stop ([`attribute`]). A put-back
/// time after `now` counts as none passed.
pub fn check_in(armed_at: u64, now: u64, tsc_hz: u64) -> Option<u64> {
    let limit = tsc_hz.checked_mul(LIMIT_SECONDS)?;
    let quiet = now.saturating_sub(armed_at);
    if tsc_hz == 0 || quiet >= limit {
        return None;
    }
    let check = tsc_hz.saturating_mul(CHECK_SECONDS);
    let floor = tsc_hz.saturating_mul(CHECK_FLOOR_MS) / 1000;
    Some((limit - quiet).min(check).max(floor.max(1)))
}

/// When a step returning at `now` next puts the counter back.
pub fn next_rearm(now: u64, tsc_hz: u64) -> u64 {
    now.saturating_add(tsc_hz.saturating_mul(REARM_SECONDS).max(1))
}

/// What the NMI handler reads on the watched processor.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Observation {
    /// The counter is armed on this processor.
    pub armed: bool,
    /// When the counter was last put back.
    pub armed_at: u64,
    /// When the running step's switch began, 0 between steps.
    pub step_began: u64,
    pub task: u64,
    pub sequence: u64,
    pub now: u64,
    pub tsc_hz: u64,
}

/// The step the watchdog found running.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Step {
    pub task: u64,
    pub sequence: u64,
    pub running_ms: u64,
}

/// A watchdog stop: how long nothing returned, and the step running then,
/// if one was.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Stuck {
    pub quiet_ms: u64,
    pub step: Option<Step>,
}

fn millis(ticks: u64, tsc_hz: u64) -> u64 {
    (u128::from(ticks) * 1000 / u128::from(tsc_hz.max(1))) as u64
}

/// Whether an NMI is the watchdog's: the counter was armed and nothing put
/// it back for the whole limit, the soonest it can overflow. Any other NMI
/// (a platform's, or one before the limit) is not, and keeps the plain
/// exception report.
pub fn attribute(observation: &Observation) -> Option<Stuck> {
    let Observation {
        armed,
        armed_at,
        step_began,
        task,
        sequence,
        now,
        tsc_hz,
    } = *observation;
    let quiet = now.checked_sub(armed_at)?;
    if !armed || tsc_hz == 0 || quiet < tsc_hz.checked_mul(LIMIT_SECONDS)? {
        return None;
    }
    let step = (step_began != 0)
        .then(|| now.checked_sub(step_began))
        .flatten()
        .map(|running| Step {
            task,
            sequence,
            running_ms: millis(running, tsc_hz),
        });
    Some(Stuck {
        quiet_ms: millis(quiet, tsc_hz),
        step,
    })
}

/// What the watching processors last saw of the boot processor's step: the
/// step's own start, by the boot processor's counter, which identifies it,
/// and when a watcher first saw it, by the watchers' shared count.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Sighting {
    pub began: u64,
    pub seen_at: u64,
}

/// What one look finds ([`look`]).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Look {
    /// No step runs, or the count's rate is unknown.
    Idle,
    /// A step no look saw before: remember it.
    New(Sighting),
    /// The step the last look saw, still within the limit.
    Running,
    /// The step the last look saw, running for the whole limit since: send
    /// the NMI.
    Overdue { quiet_ms: u64 },
}

/// One watcher's look at the boot processor's step (`began`, 0 between
/// steps) at `now` of a count at `counter_hz`, given the `last` sighting.
/// Each step has its own start, so an unchanged one is the same step. Once
/// the boot stopped (`stop`) the watch ends: the boot processor stays
/// inside the step it stopped in, and an NMI would replace the stop's
/// reason with a timeout of that step.
pub fn look(
    stop: &FirstStop,
    last: Option<Sighting>,
    began: u64,
    now: u64,
    counter_hz: u64,
) -> Look {
    if began == 0 || counter_hz == 0 || stop.began() {
        return Look::Idle;
    }
    match last {
        Some(last) if last.began == began => {
            let quiet = now.saturating_sub(last.seen_at);
            if quiet >= counter_hz.saturating_mul(LIMIT_SECONDS) {
                Look::Overdue {
                    quiet_ms: millis(quiet, counter_hz),
                }
            } else {
                Look::Running
            }
        }
        _ => Look::New(Sighting {
            began,
            seen_at: now,
        }),
    }
}

/// What the boot processor's NMI handler finds once a watcher sent the NMI
/// for the step that began at `sent` ([`caught`]).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Caught {
    /// The step still runs: it never returned.
    Stuck(Stuck),
    /// The step returned between the watcher's look and the NMI, after
    /// running for the whole limit: the guest's processors waited for the
    /// boot processor all that time, so the boot stops all the same.
    Returned,
}

/// Whether an NMI on the boot processor is the watchers': one was sent
/// (`sent`, the step's start, 0 when none was), and what the step running
/// now (`observation`, read on the boot processor's own counter) says.
pub fn caught(sent: u64, observation: &Observation) -> Option<Caught> {
    if sent == 0 {
        return None;
    }
    let Observation {
        step_began,
        task,
        sequence,
        now,
        tsc_hz,
        ..
    } = *observation;
    if step_began != sent {
        return Some(Caught::Returned);
    }
    let running = millis(now.saturating_sub(step_began), tsc_hz);
    Some(Caught::Stuck(Stuck {
        quiet_ms: running,
        step: Some(Step {
            task,
            sequence,
            running_ms: running,
        }),
    }))
}

/// The stop's reason for a watchers' NMI ([`caught`]).
pub fn caught_message(stage: &str, caught: &Caught) -> String {
    match caught {
        Caught::Stuck(stuck) => message(stage, stuck),
        Caught::Returned => format!(
            "a Linux task step ran past {LIMIT_SECONDS} s ({stage}) and returned only as the \
             step watchdog's NMI came, the guest's processors waiting for this one all that \
             time; the step watchdog stopped it"
        ),
    }
}

/// The stop's reason, before the handler's instruction pointer and stack:
/// what the boot was doing (`stage`) and the step that never returned.
pub fn message(stage: &str, stuck: &Stuck) -> String {
    match stuck.step {
        Some(step) => format!(
            "a Linux task step did not return within {LIMIT_SECONDS} s ({stage}): task {} \
             job {} ran {} ms; the step watchdog stopped it",
            step.task, step.sequence, step.running_ms
        ),
        None => format!(
            "Linux's work did not return to its dispatcher within {LIMIT_SECONDS} s ({stage}), \
             outside any task step, for {} ms; the step watchdog stopped it",
            stuck.quiet_ms
        ),
    }
}

/// How long the boot processor's dispatcher may wait idle, while it runs
/// alone, with neither its timer interrupt nor a raised wake, in each of
/// the two windows [`IdleWatch`] counts: the timer's heartbeat fires every
/// millisecond, so a second without it is a lost interrupt, not a late one.
/// A halted processor counts no cycles, so the counter's limit never ends
/// such a wait; the dispatcher then polls the time-stamp counter instead of
/// halting, and stops the boot past both windows.
pub const IDLE_LIMIT_MS: u64 = 1000;

/// A polled idle wait's clock, by the time-stamp counter, from the wait's
/// entry with no timer interrupt or wake since. The boot stops only after
/// two whole [`IDLE_LIMIT_MS`] windows: the second starts where the first
/// ran out, so a counter that jumped forward sooner than the timer's
/// interrupt came (firmware that writes the counter, a hypervisor that
/// delivers the timer late) ends at most the first, and the timer armed at
/// the entry fires within the second. A counter that
/// steps back starts its window again instead of reading as overdue.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IdleWatch {
    /// Where the running window began.
    since: u64,
    /// The milliseconds the windows before it counted.
    counted: u64,
    /// The first window ran out.
    confirming: bool,
}

/// What an idle wait does after [`IdleWatch::check`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Idle {
    /// Keep polling with interrupts open.
    Wait,
    /// Stop the boot: no timer interrupt or wake for `waited_ms`.
    Stop { waited_ms: u64 },
}

impl IdleWatch {
    /// A wait entered when the counter read `entered`.
    pub fn new(entered: u64) -> Self {
        Self {
            since: entered,
            counted: 0,
            confirming: false,
        }
    }

    /// The wait at counter `now`, still with no interrupt or wake, at
    /// `tsc_hz`. Never stops with no known rate.
    pub fn check(&mut self, now: u64, tsc_hz: u64) -> Idle {
        if tsc_hz == 0 {
            return Idle::Wait;
        }
        let Some(ticks) = now.checked_sub(self.since) else {
            self.since = now;
            return Idle::Wait;
        };
        let elapsed = millis(ticks, tsc_hz);
        if elapsed < IDLE_LIMIT_MS {
            return Idle::Wait;
        }
        if !self.confirming {
            self.confirming = true;
            self.counted = elapsed;
            self.since = now;
            return Idle::Wait;
        }
        Idle::Stop {
            waited_ms: self.counted.saturating_add(elapsed),
        }
    }
}

/// The local APIC registers an idle stop names: the timer's entry and
/// counts, the task priority, and the in-service and requested vectors
/// (eight 32-bit registers each, vector 0 in bit 0 of the first).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IdleApic {
    pub lvt_timer: u32,
    pub initial: u32,
    pub current: u32,
    pub tpr: u32,
    pub in_service: [u32; 8],
    pub requested: [u32; 8],
}

/// The highest vector set in an ISR or IRR image.
fn highest(bits: &[u32; 8]) -> Option<u32> {
    bits.iter()
        .enumerate()
        .rev()
        .find(|(_, word)| **word != 0)
        .map(|(index, word)| index as u32 * 32 + 31 - word.leading_zeros())
}

fn vector(vector: Option<u32>) -> String {
    vector.map_or_else(|| String::from("none"), |vector| format!("{vector:#x}"))
}

/// The stop's reason when the dispatcher's idle wait outlived
/// [`IDLE_LIMIT_MS`] (`waited_ms`) during `stage`.
pub fn idle_message(stage: &str, waited_ms: u64, apic: &IdleApic) -> String {
    format!(
        "the boot processor's timer interrupt did not arrive for {waited_ms} ms while Linux's \
         work waited idle ({stage}): LVT timer {:#x}{}, count {} of {}, TPR {:#x}, highest \
         vector in service {}, highest requested {}",
        apic.lvt_timer,
        if apic.lvt_timer & 1 << 16 != 0 {
            " (masked)"
        } else {
            ""
        },
        apic.current,
        apic.initial,
        apic.tpr,
        vector(highest(&apic.in_service)),
        vector(highest(&apic.requested)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Polls `watch` from `from` to `to` in steps of `step`, as the idle
    /// loop does, returning the first stop.
    fn poll(watch: &mut IdleWatch, from: u64, to: u64, step: u64) -> Option<u64> {
        let mut now = from;
        while now <= to {
            if let Idle::Stop { waited_ms } = watch.check(now, HZ) {
                return Some(waited_ms);
            }
            now += step;
        }
        None
    }

    #[test]
    fn a_lost_timer_interrupt_stops_the_wait_after_two_windows() {
        let entered = 1 << 40;
        let mut watch = IdleWatch::new(entered);
        // The first window running out only starts the second.
        assert_eq!(poll(&mut watch, entered, entered + HZ, HZ / 1000), None);
        assert_eq!(
            poll(&mut watch, entered + HZ, entered + 3 * HZ, HZ / 1000),
            Some(2000)
        );
    }

    #[test]
    fn a_counter_that_jumps_forward_once_does_not_stop_the_wait() {
        // The counter reads 10 s past the entry at the first poll, before
        // the timer armed at the entry has fired: firmware that writes the
        // counter forward, or a hypervisor that delivers the timer after
        // the counter moved. The timer fires a millisecond later.
        let entered = 1 << 40;
        let mut watch = IdleWatch::new(entered);
        assert_eq!(watch.check(entered + 10 * HZ, HZ), Idle::Wait);
        let tick = entered + 10 * HZ + HZ / 1000;
        assert_eq!(poll(&mut watch, entered + 10 * HZ, tick, HZ / 10_000), None);
    }

    #[test]
    fn a_counter_that_steps_back_starts_its_window_again() {
        // Firmware that writes the counter back: a step back must not read
        // as a whole wrap of the counter, which would be overdue at once.
        let entered = 1 << 40;
        let mut watch = IdleWatch::new(entered);
        assert_eq!(watch.check(entered - 1, HZ), Idle::Wait);
        assert_eq!(
            poll(&mut watch, entered - 1, entered + HZ / 2, HZ / 1000),
            None
        );
        // Then a lost interrupt still stops it, two windows on.
        let mut watch = IdleWatch::new(entered);
        assert_eq!(watch.check(entered - HZ, HZ), Idle::Wait);
        assert_eq!(
            poll(&mut watch, entered - HZ, entered + 2 * HZ, HZ / 1000),
            Some(2000)
        );
    }

    #[test]
    fn an_idle_wait_with_no_known_rate_never_stops() {
        let mut watch = IdleWatch::new(0);
        assert_eq!(watch.check(u64::MAX / 2, 0), Idle::Wait);
        assert_eq!(watch.check(u64::MAX, 0), Idle::Wait);
    }

    #[test]
    fn an_idle_stop_names_the_timer_and_the_vectors_that_block_it() {
        let mut apic = IdleApic {
            lvt_timer: 0x20,
            initial: 2400,
            current: 0,
            tpr: 0,
            ..IdleApic::default()
        };
        apic.in_service[1] = 1 << 17;
        apic.requested[1] = 1;
        apic.requested[6] = 1 << 3;
        assert_eq!(
            idle_message("the GPU's probe", 1000, &apic),
            "the boot processor's timer interrupt did not arrive for 1000 ms while Linux's work \
             waited idle (the GPU's probe): LVT timer 0x20, count 0 of 2400, TPR 0x0, highest \
             vector in service 0x31, highest requested 0xc3"
        );
        apic.lvt_timer |= 1 << 16;
        apic.in_service = [0; 8];
        assert!(idle_message("Linux driver work", 1200, &apic).contains(
            "LVT timer 0x10020 (masked), count 0 of 2400, TPR 0x0, highest vector in service none"
        ));
    }

    const HZ: u64 = 2_400_000_000;

    #[test]
    fn the_counter_overflows_after_one_check_at_the_nominal_clock() {
        let start = start_value(HZ, 48).unwrap();
        assert_eq!((1u64 << 48) - start, HZ * CHECK_SECONDS);
        let start = start_value(HZ, 64).unwrap();
        assert_eq!(start.wrapping_neg(), HZ * CHECK_SECONDS);
        assert_eq!(counter_start(1000, 48), Some((1 << 48) - 1000));
        assert_eq!(counter_start(0, 48), None);
        // A 32-bit counter cannot hold 12 G cycles; no rate, no counter.
        assert_eq!(start_value(HZ, 32), None);
        assert_eq!(start_value(0, 48), None);
        assert_eq!(start_value(HZ, 16), None);
        assert_eq!(start_value(u64::MAX / 2, 64), None);
    }

    /// When the watchdog stops a step that never returns on a core whose
    /// clock is `ratio` (in hundredths) of the time-stamp counter's rate,
    /// in milliseconds by the time-stamp counter: each overflow comes
    /// after its cycles at that clock, and [`check_in`] decides.
    fn stopped_after_ms(ratio: u64) -> u64 {
        let armed_at = 1 << 40;
        let mut now = armed_at;
        let mut cycles = HZ * CHECK_SECONDS;
        let mut checks = 0;
        loop {
            now += cycles * 100 / ratio;
            match check_in(armed_at, now, HZ) {
                Some(next) => cycles = next,
                None => {
                    let observation = Observation {
                        armed: true,
                        armed_at,
                        step_began: armed_at,
                        task: 1,
                        sequence: 1,
                        now,
                        tsc_hz: HZ,
                    };
                    // The stop's NMI is the watchdog's, past the limit.
                    assert!(attribute(&observation).is_some());
                    return (now - armed_at) * 1000 / HZ;
                }
            }
            checks += 1;
            assert!(checks < 100, "the checks never end at {ratio}%");
        }
    }

    #[test]
    fn a_step_that_never_returns_stops_near_the_limit_at_any_clock() {
        let limit = LIMIT_SECONDS * 1000;
        // At or above the nominal clock (an i5-10500 turbos at 1.45 times
        // it, where 0.6.1's count of three limits stopped after 65 s):
        // within the floor of the limit.
        for ratio in [100, 145, 200, 300, 450] {
            let stopped = stopped_after_ms(ratio);
            assert!(
                (limit..limit + CHECK_FLOOR_MS).contains(&stopped),
                "{ratio}%: {stopped} ms"
            );
        }
        // Below it, within the slower clock's one check.
        for ratio in [50, 80] {
            let stopped = stopped_after_ms(ratio);
            let late = CHECK_SECONDS * 1000 * (100 - ratio) / ratio;
            assert!(
                (limit..=limit + late).contains(&stopped),
                "{ratio}%: {stopped} ms"
            );
        }
    }

    #[test]
    fn a_check_counts_what_is_left_of_the_limit() {
        let armed_at = 1 << 40;
        // Early on, one whole check; near the end, what is left; past the
        // limit, the stop.
        assert_eq!(
            check_in(armed_at, armed_at + HZ, HZ),
            Some(HZ * CHECK_SECONDS)
        );
        assert_eq!(check_in(armed_at, armed_at + 28 * HZ, HZ), Some(2 * HZ));
        assert_eq!(
            check_in(armed_at, armed_at + LIMIT_SECONDS * HZ - 1, HZ),
            Some(HZ * CHECK_FLOOR_MS / 1000)
        );
        assert_eq!(check_in(armed_at, armed_at + LIMIT_SECONDS * HZ, HZ), None);
        // A put-back time after the NMI's reading: none passed.
        assert_eq!(
            check_in(armed_at, armed_at - HZ, HZ),
            Some(HZ * CHECK_SECONDS)
        );
        assert_eq!(check_in(armed_at, armed_at, 0), None);
    }

    #[test]
    fn a_returned_step_puts_the_counter_back_once_a_second_at_most() {
        assert_eq!(next_rearm(10, HZ), 10 + HZ);
        assert_eq!(next_rearm(u64::MAX - 1, HZ), u64::MAX);
        assert_eq!(next_rearm(10, 0), 11);
    }

    fn observed(quiet_seconds: u64, step_seconds: Option<u64>) -> Observation {
        let now = 1000 * HZ;
        Observation {
            armed: true,
            armed_at: now - quiet_seconds * HZ,
            step_began: step_seconds.map_or(0, |seconds| now - seconds * HZ),
            task: 7,
            sequence: 42,
            now,
            tsc_hz: HZ,
        }
    }

    #[test]
    fn only_an_nmi_after_the_whole_limit_on_an_armed_counter_is_the_watchdogs() {
        // Before the limit, the counter cannot have overflowed: another NMI.
        assert_eq!(attribute(&observed(LIMIT_SECONDS - 1, Some(40))), None);
        let unarmed = Observation {
            armed: false,
            ..observed(LIMIT_SECONDS + 5, Some(40))
        };
        assert_eq!(attribute(&unarmed), None);
        let no_rate = Observation {
            tsc_hz: 0,
            ..observed(LIMIT_SECONDS + 5, Some(40))
        };
        assert_eq!(attribute(&no_rate), None);
        // A put-back time after the NMI's own reading is no overflow either.
        let ahead = Observation {
            armed_at: 1001 * HZ,
            ..observed(LIMIT_SECONDS, None)
        };
        assert_eq!(attribute(&ahead), None);
        assert_eq!(
            attribute(&observed(LIMIT_SECONDS, Some(40))),
            Some(Stuck {
                quiet_ms: LIMIT_SECONDS * 1000,
                step: Some(Step {
                    task: 7,
                    sequence: 42,
                    running_ms: 40_000
                }),
            })
        );
        assert_eq!(
            attribute(&observed(LIMIT_SECONDS + 3, None)),
            Some(Stuck {
                quiet_ms: (LIMIT_SECONDS + 3) * 1000,
                step: None,
            })
        );
    }

    #[test]
    fn another_processor_finds_a_step_stuck_once_it_saw_it_run_for_the_limit() {
        const COUNT_HZ: u64 = 1_000_000_000;
        let at = |seconds: u64| 5 * COUNT_HZ + seconds * COUNT_HZ;
        let watching = FirstStop::new();
        // Nothing runs: nothing to watch, whatever was seen before.
        assert_eq!(look(&watching, None, 0, at(0), COUNT_HZ), Look::Idle);
        let seen = Sighting {
            began: 77,
            seen_at: at(0),
        };
        assert_eq!(look(&watching, Some(seen), 0, at(40), COUNT_HZ), Look::Idle);
        assert_eq!(look(&watching, None, 77, at(0), 0), Look::Idle);
        // A step first seen is remembered, by the watcher's own count.
        assert_eq!(look(&watching, None, 77, at(0), COUNT_HZ), Look::New(seen));
        // The same step, within the limit, and past it.
        assert_eq!(
            look(&watching, Some(seen), 77, at(LIMIT_SECONDS) - 1, COUNT_HZ),
            Look::Running
        );
        assert_eq!(
            look(&watching, Some(seen), 77, at(LIMIT_SECONDS + 2), COUNT_HZ),
            Look::Overdue {
                quiet_ms: (LIMIT_SECONDS + 2) * 1000
            }
        );
        // Another step since, however long the watch: a new sighting.
        assert_eq!(
            look(&watching, Some(seen), 78, at(LIMIT_SECONDS + 2), COUNT_HZ),
            Look::New(Sighting {
                began: 78,
                seen_at: at(LIMIT_SECONDS + 2)
            })
        );
        // A count that went back (another watcher's earlier reading) is
        // no overdue.
        assert_eq!(
            look(&watching, Some(seen), 77, at(0) - COUNT_HZ, COUNT_HZ),
            Look::Running
        );
    }

    #[test]
    fn the_watch_ends_at_the_first_stop() {
        const COUNT_HZ: u64 = 1_000_000_000;
        let seen = Sighting {
            began: 77,
            seen_at: COUNT_HZ,
        };
        let overdue = COUNT_HZ * (LIMIT_SECONDS + 30);
        let stop = FirstStop::new();
        assert!(matches!(
            look(&stop, Some(seen), 77, overdue, COUNT_HZ),
            Look::Overdue { .. }
        ));
        // The boot processor stopped inside the step: its fatal stop never
        // returns, and the step never does either.
        assert_eq!(stop.claim(0), crate::first_stop::Claim::First);
        assert_eq!(look(&stop, Some(seen), 77, overdue, COUNT_HZ), Look::Idle);
        assert_eq!(look(&stop, None, 77, overdue, COUNT_HZ), Look::Idle);
        stop.finish(0);
        assert_eq!(look(&stop, Some(seen), 77, overdue, COUNT_HZ), Look::Idle);
    }

    #[test]
    fn only_an_nmi_a_watcher_sent_is_attributed_and_a_late_return_still_stops() {
        let observation = observed(0, Some(LIMIT_SECONDS + 1));
        // No NMI sent: a platform's, with the plain exception report.
        assert_eq!(caught(0, &observation), None);
        let stuck = caught(observation.step_began, &observation).unwrap();
        assert_eq!(
            stuck,
            Caught::Stuck(Stuck {
                quiet_ms: (LIMIT_SECONDS + 1) * 1000,
                step: Some(Step {
                    task: 7,
                    sequence: 42,
                    running_ms: (LIMIT_SECONDS + 1) * 1000
                }),
            })
        );
        let text = caught_message("Linux driver work", &stuck);
        assert!(text.contains("(Linux driver work)"), "{text}");
        assert!(text.contains("task 7 job 42 ran 31000 ms"), "{text}");
        // The step returned between the look and the NMI, or another runs.
        let returned = Observation {
            step_began: 0,
            ..observation
        };
        assert_eq!(
            caught(observation.step_began, &returned),
            Some(Caught::Returned)
        );
        let next = Observation {
            step_began: observation.step_began + 5,
            ..observation
        };
        assert_eq!(
            caught(observation.step_began, &next),
            Some(Caught::Returned)
        );
        assert!(caught_message("x", &Caught::Returned).contains("returned only as"));
    }

    #[test]
    fn the_stop_names_the_stage_and_the_step() {
        let stuck = attribute(&observed(LIMIT_SECONDS, Some(31))).unwrap();
        let text = message("the GPU's probe", &stuck);
        assert!(text.contains("(the GPU's probe)"), "{text}");
        assert!(text.contains("task 7 job 42 ran 31000 ms"), "{text}");
        let stuck = attribute(&observed(LIMIT_SECONDS, None)).unwrap();
        assert!(message("Linux's first tasks", &stuck).contains("outside any task step"));
    }
}
