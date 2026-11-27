//! A host replay of the ring under sign-in's working set: the real
//! [`Ring`] and ghosts ([`Churn`]) take the installs of a guest that keeps
//! running a hot pool of blocks larger than a 512 MiB window holds and
//! runs a stream of new blocks once each, as run 20261119-021855 showed
//! (0.6-1.1 million installs a heartbeat, 1.6 million new blocks over 18
//! heartbeats of sign-in, about 1.05 million blocks in the window). Scaled
//! down a hundredfold: a window of 512 MiB holds 10 500 blocks.
//!
//! A run of a block the ring holds counts its part's heat, one run in
//! eight as the sampled transfers do; a run of a block it does not hold
//! installs it, in the tenured generation when its ghost says the ring took
//! it within the tenure age ([`Churn::promotes`]).
//!
//! Beside the ring's own policy ([`Policy::Ring`]) it replays two others
//! on the same ring, and a block-by-block least-recently-used window
//! ([`least_recent`]) as the bound no policy of whole parts reaches.
use super::super::churn::{self, Churn, ChurnCounts, Gone};
use super::{Advance, Grace, Ring, TENURED, YOUNG};
use alloc::{vec, vec::Vec};

const BLOCK_BYTES: usize = 64;
const PARTS: usize = 32;
const TICKS: u64 = 20;
const WARM_BEATS: u64 = 6;
const BEATS: u64 = 18;
/// Sampled heat: one run in this many counts.
const SAMPLE: u64 = 8;

/// The guest: `hot` blocks run with odds per beat between `least` and 4
/// runs (log-uniform), and `new` blocks a beat that run once. Each beat
/// `turnover` of the hot blocks give way to new ones, which keep their
/// odds: the code of a launch, which the guest keeps running.
///
/// The first `paired` of the hot blocks are code of pages the guest maps
/// at two addresses (the shared cache at two slides): each of their runs
/// is at one address or the other, odds even. A board that installs a
/// block per address holds two blocks of each; one whose addresses share
/// one install (`shared`) holds one, as for any other block.
#[derive(Clone, Copy, Debug)]
pub(super) struct Guest {
    pub(super) hot: usize,
    pub(super) least: f64,
    pub(super) new: usize,
    pub(super) turnover: usize,
    pub(super) paired: usize,
    pub(super) shared: bool,
}

/// What a replay counted per beat after its warm-up.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Outcome {
    pub(super) installs: u64,
    /// Installs of blocks installed before.
    pub(super) again: u64,
    pub(super) promoted: u64,
    pub(super) retired: u64,
    /// Parts chosen to go by why, over the replay.
    pub(super) outs: [u64; 4],
    /// Blocks a retirement copied into tenured code ([`Policy::Survivors`]).
    pub(super) copied: u64,
    /// Runs of blocks of the hot pool, and those that found their block
    /// in the window.
    pub(super) hot_runs: u64,
    pub(super) hot_hits: u64,
    /// With the addresses sharing one install (`Guest::shared`): runs of
    /// paired blocks, which run position independent code, and the
    /// aliases made for runs at the second address, one each time the
    /// owner's install is new to it (`share_aliases`).
    pub(super) shared_runs: u64,
    pub(super) aliases: u64,
}

/// What a replay's ring does beyond its own choices.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Policy {
    /// The ring as the engine runs it.
    Ring,
    /// A block of the hot pool goes to tenured code at its first install,
    /// as if its install knew it would run again: a bound on any hint
    /// (the pack's history, the exit that reached it).
    TenureHot,
    /// A retirement copies the blocks a sampled run found since their
    /// install or last copy into tenured code: the part's survivors stay,
    /// and only its blocks that did not run again go.
    Survivors,
}

struct Model {
    ring: Ring,
    grace: Grace,
    churn: Churn,
    counts: ChurnCounts,
    part_of: Vec<Option<u32>>,
    blocks_of: Vec<Vec<usize>>,
    seen: Vec<bool>,
    outcome: Outcome,
    counting: bool,
    policy: Policy,
    ran: Vec<bool>,
    pending: Vec<usize>,
    draining: bool,
    /// The installed blocks whose second address has its alias.
    aliased: Vec<bool>,
}

/// The bit of a run's block that says it runs once.
const ONCE: usize = 1 << 62;
/// The bit of a run's block that says it runs at its second address,
/// sharing its first's install (`Guest::shared`).
const ALIAS: usize = 1 << 61;
/// The bit of a run's block that says it is a paired block's.
const PAIRED: usize = 1 << 60;

fn identity(block: usize) -> u64 {
    churn::identity(
        0x1_0000_0000 + 4 * block as u64,
        0x8000_0000 + 4 * block as u64,
        0,
    )
}

impl Model {
    fn retire(&mut self, part: u32) {
        let generation = self.ring.generation();
        for block in core::mem::take(&mut self.blocks_of[part as usize]) {
            self.part_of[block] = None;
            // An alias goes with its owner's code.
            self.aliased[block] = false;
            if self.policy == Policy::Survivors && core::mem::take(&mut self.ran[block]) {
                self.pending.push(block);
                continue;
            }
            self.churn
                .gone(&mut self.counts, identity(block), 0, generation, Gone::Ring);
        }
        self.ring.retired(part, self.grace.snapshot());
        self.outcome.retired += u64::from(self.counting);
    }

    fn install(&mut self, block: usize, hot: bool) {
        self.place(block, hot && self.policy == Policy::TenureHot, false);
        if !self.draining {
            self.draining = true;
            while let Some(block) = self.pending.pop() {
                if self.part_of[block].is_none() {
                    self.place(block, true, true);
                }
            }
            self.draining = false;
        }
    }

    fn place(&mut self, block: usize, tenure: bool, copy: bool) {
        let id = identity(block);
        let promoted = self
            .churn
            .promotes(id, self.ring.generation(), self.ring.parts);
        let stream = if promoted || tenure { TENURED } else { YOUNG };
        let place = loop {
            match self.ring.take(stream, BLOCK_BYTES, 16) {
                Ok(place) => break place,
                Err(full) => match self.ring.advance(stream, full, &self.grace, None) {
                    Advance::Moved => {}
                    Advance::Retire(part, _) => self.retire(part),
                    other => panic!("a ring nobody holds: {other:?}"),
                },
            }
        };
        self.ring.release(place);
        self.churn.installed(
            &mut self.counts,
            id,
            0,
            self.ring.generation(),
            None,
            self.ring.parts,
        );
        self.ring.installed(place.part as usize);
        self.part_of[block] = Some(place.part);
        self.blocks_of[place.part as usize].push(block);
        if copy {
            self.outcome.copied += u64::from(self.counting);
            return;
        }
        if self.counting {
            self.outcome.installs += 1;
            self.outcome.again += u64::from(self.seen[block]);
            self.outcome.promoted += u64::from(promoted);
        }
        self.seen[block] = true;
    }
}

fn next(seed: &mut u64) -> u64 {
    *seed ^= *seed << 13;
    *seed ^= *seed >> 7;
    *seed ^= *seed << 17;
    *seed
}

fn uniform(seed: &mut u64) -> f64 {
    (next(seed) >> 11) as f64 / (1u64 << 53) as f64
}

/// Run `guest`: `run` takes each block a run reaches, the replay's
/// random state, whether the warm-up is over, and whether the block is of
/// the hot pool (not one that runs once).
fn drive(guest: Guest, mut run: impl FnMut(usize, &mut u64, bool, bool)) {
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    let odds: Vec<f64> = (0..guest.hot)
        .map(|_| (guest.least.ln() * (1.0 - uniform(&mut seed))).exp() * 4.0 / TICKS as f64)
        .collect();
    let mut members: Vec<usize> = (0..guest.hot).collect();
    let mut fresh = guest.hot;
    let mut runs = Vec::new();
    for beat in 0..WARM_BEATS + BEATS {
        let counting = beat >= WARM_BEATS;
        for _ in 0..guest.turnover {
            let at = next(&mut seed) as usize % guest.hot;
            members[at] = fresh;
            fresh += 1;
        }
        for _ in 0..TICKS {
            runs.clear();
            for at in 0..guest.hot {
                if uniform(&mut seed) >= odds[at] {
                    continue;
                }
                // A block of a paired page at its second address is
                // another block, unless the addresses share one install.
                let second = at < guest.paired && next(&mut seed) & 1 == 1;
                let slide = second && !guest.shared;
                let alias = if second && guest.shared { ALIAS } else { 0 };
                let paired = if at < guest.paired { PAIRED } else { 0 };
                runs.push((2 * members[at] + usize::from(slide)) | alias | paired);
            }
            for _ in 0..guest.new / TICKS as usize {
                runs.push((2 * fresh) | ONCE);
                fresh += 1;
            }
            // Shuffle: the guest's runs interleave.
            for at in (1..runs.len()).rev() {
                runs.swap(at, next(&mut seed) as usize % (at + 1));
            }
            for &block in &runs {
                run(block & !ONCE, &mut seed, counting, block & ONCE == 0);
            }
        }
    }
}

/// The blocks `guest` ever runs: two numbers per block, its second
/// address's the odd one.
fn total(guest: Guest) -> usize {
    2 * (guest.hot + (guest.new + guest.turnover) * (WARM_BEATS + BEATS) as usize + 1)
}

fn per_beat(outcome: Outcome) -> Outcome {
    Outcome {
        installs: outcome.installs / BEATS,
        again: outcome.again / BEATS,
        promoted: outcome.promoted / BEATS,
        retired: outcome.retired / BEATS,
        copied: outcome.copied / BEATS,
        hot_runs: outcome.hot_runs / BEATS,
        hot_hits: outcome.hot_hits / BEATS,
        shared_runs: outcome.shared_runs / BEATS,
        aliases: outcome.aliases / BEATS,
        ..outcome
    }
}

/// Replay `guest` on a window of `blocks` blocks whose young code keeps a
/// `nursery`th of the parts, under `policy`: per beat, after the warm-up.
pub(super) fn replay(guest: Guest, blocks: usize, nursery: u32, policy: Policy) -> Outcome {
    replay_in(guest, blocks, PARTS, nursery, policy)
}

/// [`replay`] on a ring of `parts` parts.
pub(super) fn replay_in(
    guest: Guest,
    blocks: usize,
    parts: usize,
    nursery: u32,
    policy: Policy,
) -> Outcome {
    let per_part = blocks / parts;
    let total = total(guest);
    let mut model = Model {
        ring: Ring::with_nursery(parts, per_part * BLOCK_BYTES, nursery),
        grace: Grace::new(1),
        churn: Churn::new(1 << 20),
        counts: ChurnCounts::default(),
        part_of: vec![None; total],
        blocks_of: vec![Vec::new(); parts],
        seen: vec![false; total],
        outcome: Outcome::default(),
        counting: false,
        policy,
        ran: vec![false; total],
        pending: Vec::new(),
        draining: false,
        aliased: vec![false; total],
    };
    drive(guest, |block, seed, counting, hot| {
        model.counting = counting;
        let (alias, paired) = (block & ALIAS != 0, block & PAIRED != 0);
        let block = block & !(ALIAS | PAIRED);
        if counting && guest.shared && paired {
            model.outcome.shared_runs += 1;
        }
        if counting && hot {
            model.outcome.hot_runs += 1;
            model.outcome.hot_hits += u64::from(model.part_of[block].is_some());
        }
        match model.part_of[block] {
            Some(part) => {
                if next(seed).is_multiple_of(SAMPLE) {
                    model.ring.entered(part as usize);
                    model.ran[block] = true;
                }
            }
            None => model.install(block, hot),
        }
        // A run at the second address takes the alias of the install, made
        // once per install: a slot, no code.
        if alias && model.part_of[block].is_some() && !model.aliased[block] {
            model.aliased[block] = true;
            model.outcome.aliases += u64::from(counting);
        }
    });
    model.outcome.outs = model.ring.outs();
    per_beat(model.outcome)
}

/// What a window of `blocks` blocks that evicts the block run least
/// recently, one at a time, installs: a bound no part-sized policy
/// reaches, for how much of the churn is the policy's.
pub(super) fn least_recent(guest: Guest, blocks: usize) -> Outcome {
    let total = total(guest);
    let mut last = vec![None::<u64>; total];
    let mut seen = vec![false; total];
    let mut order = alloc::collections::BTreeSet::new();
    let mut clock = 0u64;
    let mut outcome = Outcome::default();
    drive(guest, |block, _, counting, _| {
        clock += 1;
        match last[block] {
            Some(at) => {
                order.remove(&(at, block));
            }
            None => {
                if order.len() == blocks
                    && let Some((at, gone)) = order.pop_first()
                {
                    debug_assert_eq!(last[gone], Some(at));
                    last[gone] = None;
                }
                if counting {
                    outcome.installs += 1;
                    outcome.again += u64::from(seen[block]);
                }
                seen[block] = true;
            }
        }
        last[block] = Some(clock);
        order.insert((clock, block));
    });
    per_beat(outcome)
}

/// Sign-in's hot pool (1.3 million blocks unscaled, 0.25-1 of 4 runs a
/// beat) and 90 thousand new blocks a beat: on a 512 MiB window with a
/// quarter of it young, as before, the ring installs about 1.1 million
/// blocks again a beat, as run 20261119-021855 did; on the 816 MiB window
/// a 12 GiB host now gets, with an eighth young, under a tenth of that.
#[test]
fn a_window_that_holds_sign_ins_hot_code_with_a_small_nursery_keeps_it() {
    let guest = Guest {
        hot: 13_000,
        least: 0.25,
        new: 900,
        turnover: 0,
        paired: 0,
        shared: false,
    };
    let before = replay(guest, 10_500, 4, Policy::Ring);
    let after = replay(guest, 16_700, super::ring::NURSERY_SHARE, Policy::Ring);
    assert!(before.again >= 8_000, "{before:?}");
    assert!(after.again * 10 <= before.again, "{before:?} {after:?}");
    // A larger window alone, its nursery a quarter, keeps less than half.
    let larger = replay(guest, 16_700, 4, Policy::Ring);
    assert!(larger.again > 2 * after.again, "{larger:?} {after:?}");
}

/// The table behind it: hot pools, launches and windows, under each
/// policy, beside the block-by-block bound. On run 20261119-115758's
/// 640 MiB window every reinstall followed a young part's going; here the
/// ring installs again 2.5 times what the bound does at 640 MiB and 1.3
/// million hot blocks, and 20 times at 816 MiB. Neither other policy
/// closes it: a first install that knew its block would run again helps
/// only where the hot pool fits the window and costs more where it does
/// not, and copying survivors trades a miss for a copy.
#[test]
#[ignore = "manual: opt-in"]
fn sign_in_replay_on_the_real_ring() {
    for hot in [9_500, 13_000] {
        for turnover in [0, 300, 1_000] {
            let guest = Guest {
                hot,
                least: 0.25,
                new: 900,
                turnover,
                paired: 0,
                shared: false,
            };
            for (window, blocks) in [
                (512, 10_500),
                (640, 13_100),
                (816, 16_700),
                (1200, 24_600),
                (1616, 33_100),
            ] {
                let bound = least_recent(guest, blocks);
                for policy in [Policy::Ring, Policy::TenureHot, Policy::Survivors] {
                    let outcome = replay(guest, blocks, super::ring::NURSERY_SHARE, policy);
                    std::println!(
                        "REPLAY hot={hot}00 launch={turnover}00 window={window}M {policy:?} \
                         installs={} again={} copied={} promoted={} retired={} outs={:?} \
                         lru_again={} (per beat, x100 for the VM)",
                        outcome.installs,
                        outcome.again,
                        outcome.copied,
                        outcome.promoted,
                        outcome.retired,
                        outcome.outs,
                        bound.again,
                    );
                }
            }
        }
    }
}

/// The ring at the Finder window of a 16 GiB laptop: hot code about the
/// window's size (both slides of the shared cache) and 120 thousand new
/// blocks a heartbeat, scaled a hundredfold. Parts of a 64th and young code
/// a 16th of them install at least 30% fewer blocks again than parts of a
/// 32nd with young code an eighth; a block-by-block window installs fewer
/// still, the room left to no policy of whole parts.
#[test]
fn finer_parts_and_a_smaller_nursery_keep_a_window_sized_pool() {
    let guest = Guest {
        hot: 26_000,
        least: 0.25,
        new: 1_200,
        turnover: 300,
        paired: 0,
        shared: false,
    };
    let before = replay_in(guest, 27_000, 32, 8, Policy::Ring);
    let after = replay_in(guest, 27_000, 64, super::ring::NURSERY_SHARE, Policy::Ring);
    let bound = least_recent(guest, 27_000);
    assert!(after.again * 10 <= before.again * 7, "{before:?} {after:?}");
    assert!(bound.again < after.again, "{bound:?} {after:?}");
    // A pool the window holds installs no more again either way.
    let fits = Guest {
        hot: 16_000,
        ..guest
    };
    let (before, after) = (
        replay_in(fits, 27_000, 32, 8, Policy::Ring),
        replay_in(fits, 27_000, 64, super::ring::NURSERY_SHARE, Policy::Ring),
    );
    assert!(after.again <= before.again, "{before:?} {after:?}");
}

/// The table behind it: pools about the window's size under part counts
/// and nursery shares, beside the block-by-block bound. Manual: `cargo test
/// -p stela-engine --release --lib finder_window_replay -- --ignored
/// --nocapture`.
#[test]
#[ignore = "manual: opt-in"]
fn finder_window_replay_by_parts_and_nursery() {
    for hot in [20_000, 23_000, 26_000, 29_000, 32_000] {
        for new in [600, 1_200, 2_000] {
            let guest = Guest {
                hot,
                least: 0.25,
                new,
                turnover: 300,
                paired: 0,
                shared: false,
            };
            let mut line = std::format!(
                "REPLAY hot={hot}00 new={new}00 lru_again={}",
                least_recent(guest, 27_000).again
            );
            for (parts, share) in [(32, 8), (32, 16), (64, 8), (64, 16), (64, 32)] {
                let outcome = replay_in(guest, 27_000, parts, share, Policy::Ring);
                line.push_str(&std::format!(
                    " parts={parts}/young=1/{share}:{}",
                    outcome.again
                ));
            }
            std::println!("{line} (again per beat, x100 for the laptop)");
        }
    }
}

/// The Finder window with its shared cache at two slides, as boot 35 of
/// the 0.6.1 profile ran it: 1093 sampled pages ran at both, holding 64%
/// of the user samples, and the hot code was about the window's size. Of
/// 26 thousand hot blocks (scaled a hundredfold) installed per address,
/// 64% are the two copies of 8 320 paired blocks: 17 680 blocks in all. A
/// board whose two addresses of a page share one install holds those
/// 17 680, which the window keeps, and installs a small part again of what
/// a board installing each address does.
#[test]
fn one_install_for_both_slides_keeps_a_window_sized_pool() {
    let guest = Guest {
        hot: 17_680,
        least: 0.25,
        new: 1_200,
        turnover: 300,
        paired: 8_320,
        shared: false,
    };
    let apart = replay_in(guest, 27_000, 64, super::ring::NURSERY_SHARE, Policy::Ring);
    let shared = replay_in(
        Guest {
            shared: true,
            ..guest
        },
        27_000,
        64,
        super::ring::NURSERY_SHARE,
        Policy::Ring,
    );
    assert!(shared.again * 3 <= apart.again, "{apart:?} {shared:?}");
    assert!(shared.installs < apart.installs, "{apart:?} {shared:?}");
    assert!(
        shared.hot_hits * apart.hot_runs > apart.hot_hits * shared.hot_runs,
        "{apart:?} {shared:?}"
    );
}

/// What sharing costs against what it saves, at the Finder window's 64%
/// pairs: an install of a block per address against one install and an
/// alias (a slot and its moved rows, no compile, no code) per address, and
/// the price of running position independent code. Assumed costs, in TSC
/// ticks: an install 30 000 (10 us, compiled or relocated, the code and
/// its links), an alias 3 000; a shared run pays a load from the frame per
/// guest address it uses (two, 1 tick each) and, for the third of its
/// transfers that leave the page, a probe (75 ticks) where a stamped tail
/// took 40. A replay run stands for every transfer a block made in its
/// tick, which the replay does not know: the table gives how many each may
/// stand for before sharing loses, against a laptop's tens.
#[test]
fn sharing_aliases_pays_for_its_runs_at_the_finder_window() {
    const INSTALL: f64 = 30_000.0;
    const ALIAS: f64 = 3_000.0;
    const RUN: f64 = 2.0 + (75.0 - 40.0) / 3.0;
    let guest = Guest {
        hot: 17_680,
        least: 0.25,
        new: 1_200,
        turnover: 300,
        paired: 8_320,
        shared: false,
    };
    let apart = replay_in(guest, 27_000, 64, super::ring::NURSERY_SHARE, Policy::Ring);
    let shared = replay_in(
        Guest {
            shared: true,
            ..guest
        },
        27_000,
        64,
        super::ring::NURSERY_SHARE,
        Policy::Ring,
    );
    let saved =
        (apart.installs as f64 - shared.installs as f64) * INSTALL - shared.aliases as f64 * ALIAS;
    let per_run = shared.shared_runs as f64 * RUN;
    let break_even = saved / per_run;
    std::println!(
        "SHARE REPLAY apart installs={} again={} | shared installs={} again={} aliases={} \
         shared_runs={} | saved={:.0} ticks a beat, {:.1} ticks a shared run: even at {:.0} \
         transfers a run",
        apart.installs,
        apart.again,
        shared.installs,
        shared.again,
        shared.aliases,
        shared.shared_runs,
        saved,
        RUN,
        break_even,
    );
    assert!(
        shared.installs + shared.aliases < apart.installs,
        "{apart:?} {shared:?}"
    );
    assert!(saved > 0.0, "{apart:?} {shared:?}");
}

/// The table behind it: shares of the hot pool at two slides and windows,
/// each address installed apart or both sharing one install. Manual:
/// `cargo test -p stela-engine --release --lib two_slides_replay --
/// --ignored --nocapture`.
#[test]
#[ignore = "manual: opt-in"]
fn two_slides_replay_apart_and_shared() {
    // Hot copies per address, of which a share are two copies of a paired
    // block: the 0.6.1 profile's Finder window is 26 000 and 64%.
    for copies in [20_000usize, 26_000, 32_000] {
        for percent in [0usize, 32, 50, 64, 80] {
            let paired = copies * percent / 200;
            let guest = Guest {
                hot: copies - paired,
                least: 0.25,
                new: 1_200,
                turnover: 300,
                paired,
                shared: false,
            };
            let mut line = std::format!("REPLAY copies={copies}00 paired={percent}%");
            for window in [21_000, 27_000, 33_000] {
                for shared in [false, true] {
                    let outcome = replay_in(
                        Guest { shared, ..guest },
                        window,
                        64,
                        super::ring::NURSERY_SHARE,
                        Policy::Ring,
                    );
                    line.push_str(&std::format!(
                        " w={window}/{}:installs={}/again={}/hit={:.3}",
                        if shared { "one" } else { "apart" },
                        outcome.installs,
                        outcome.again,
                        outcome.hot_hits as f64 / outcome.hot_runs.max(1) as f64,
                    ));
                }
            }
            std::println!("{line} (per beat, x100 for the laptop)");
        }
    }
}
