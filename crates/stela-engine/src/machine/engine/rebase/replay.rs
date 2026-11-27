//! A host replay of the board's kept forms under the Finder window of a
//! 16 GiB laptop's profile boot (8 PEs, a 1280 MiB window of 2.7 million
//! blocks, an 87 MiB bound): the guest runs the shared cache's hot pages at
//! two slides 0xdf88000 apart (1093 of the window's sampled pages at both,
//! 64% of its user samples), and the window holds less than both. Its PES
//! CODE lines a heartbeat: 430-550 thousand compiles, 194-233 thousand
//! installs at a page offset another PC held, 25-39 thousand relocated,
//! 182-340 thousand forms kept for blocks the ring took away.
//!
//! The real [`Ring`], ghosts ([`Churn`]) and [`Rebases`] take the installs,
//! scaled down a hundredfold, under the policy that kept the form of each
//! block the ring took away ([`Policy::Promoted`]) and the one that keeps the
//! forms of a page installed at two addresses ([`Policy::Shared`]). Neither
//! relocates more than a few installs in a hundred once the window turns
//! over: the bound is a fifteenth of the window and its forms leave within
//! a heartbeat, so what cuts compiles there is what the ring keeps
//! (`board/replay.rs`).
use super::super::FETCH_PAGE;
use super::super::board::{Advance, Grace, Ring, TENURED, YOUNG};
use super::super::churn::{self, Churn, ChurnCounts, Gone};
use super::{Found, Rebases};
use crate::translations::{Digest, Entry};
use alloc::{vec, vec::Vec};

const BLOCK_BYTES: usize = 64;
const PARTS: usize = 32;
const TICKS: u64 = 20;
const WARM_BEATS: u64 = 8;
const BEATS: u64 = 16;
/// Sampled heat: one run in this many counts its part.
const SAMPLE: u64 = 8;
/// Blocks a fetch page holds.
const PER_PAGE: usize = 32;
/// The two slides of the shared cache.
const SLIDES: [u64; 2] = [0x1_8f00_0000, 0x1_8f00_0000 - 0xdf8_8000];
/// Where the pages of code run once are mapped.
const ONCE_BASE: u64 = 0x1_0000_0000;
const FRAMES: u64 = 0x8_0000_0000;

/// The guest: `hot` blocks of shared-cache pages run at the first slide,
/// with odds a beat between `least` and 4 runs (log-uniform); `twice` of
/// their pages run at the second slide too, with the same odds; and `new`
/// blocks a beat, of pages of their own, run once.
#[derive(Clone, Copy, Debug)]
pub(super) struct Guest {
    pub(super) hot: usize,
    pub(super) least: f64,
    pub(super) twice: f64,
    pub(super) new: usize,
}

/// How a compile chooses to keep its form.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Policy {
    /// A block the ring took away within its tenure age keeps its form.
    Promoted,
    /// A block of a page installed at two addresses keeps its form.
    Shared,
}

/// Per beat after the warm-up.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Outcome {
    pub(super) installs: u64,
    pub(super) compiled: u64,
    pub(super) relocated: u64,
    /// Installs at a page offset the other slide's block held.
    pub(super) aliases: u64,
    pub(super) promoted: u64,
    pub(super) kept: u64,
}

/// Its instance's page, page offset and PC: hot instances are `2 * block
/// + slide`, those past `2 * hot` run once.
fn place_of(guest: &Guest, instance: usize) -> (u64, u16, u64) {
    let (page, slot, base) = if instance < 2 * guest.hot {
        let block = instance / 2;
        (block / PER_PAGE, block % PER_PAGE, SLIDES[instance % 2])
    } else {
        let block = instance - 2 * guest.hot;
        let page = guest.hot.div_ceil(PER_PAGE) + block / PER_PAGE;
        (page, block % PER_PAGE, ONCE_BASE)
    };
    let offset = (slot * (FETCH_PAGE as usize / PER_PAGE)) as u16;
    let page = page as u64 * FETCH_PAGE;
    (
        FRAMES + page + u64::from(offset),
        offset,
        base + page + u64::from(offset),
    )
}

/// The content key of the page at `physical`.
fn key(physical: u64) -> Digest {
    let hash = (physical / FETCH_PAGE).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    let mut bytes = [0; 32];
    bytes[..8].copy_from_slice(&hash.to_le_bytes());
    bytes[8..16].copy_from_slice(&(physical / FETCH_PAGE).to_le_bytes());
    Digest(bytes)
}

/// A form of the laptop's size: 61 code bytes for each of seven
/// instructions, as its installs averaged.
fn form(offset: u16) -> Entry {
    Entry {
        context: Digest::default(),
        offset,
        source_bytes: 28,
        cold: (0, 0),
        input: vec![0; 28],
        code: vec![0; 7 * 61],
        relocations: vec![0, 8, 16, 24],
        source_map: (0..7)
            .map(|at| (offset + 4 * at, u32::from(at) * 61))
            .collect(),
        exits: vec![],
        recovery: Default::default(),
    }
}

struct Model {
    guest: Guest,
    policy: Policy,
    ring: Ring,
    grace: Grace,
    churn: Churn,
    counts: ChurnCounts,
    rebases: Rebases,
    part_of: Vec<Option<u32>>,
    blocks_of: Vec<Vec<usize>>,
    outcome: Outcome,
    counting: bool,
}

impl Model {
    fn identity(&self, instance: usize) -> u64 {
        let (physical, _, pc) = place_of(&self.guest, instance);
        churn::identity(pc, physical, 0)
    }

    fn retire(&mut self, part: u32) {
        let generation = self.ring.generation();
        for instance in core::mem::take(&mut self.blocks_of[part as usize]) {
            self.part_of[instance] = None;
            let identity = self.identity(instance);
            self.churn
                .gone(&mut self.counts, identity, 0, generation, Gone::Ring);
        }
        self.ring.retired(part, self.grace.snapshot());
    }

    fn install(&mut self, instance: usize) {
        let (physical, offset, pc) = place_of(&self.guest, instance);
        let identity = churn::identity(pc, physical, 0);
        let promoted = self
            .churn
            .promotes(identity, self.ring.generation(), PARTS as u32);
        let keep = match self.policy {
            Policy::Promoted => promoted,
            Policy::Shared => self.rebases.shared(physical),
        };
        let (key, context) = (key(physical), Digest::default());
        let count = u64::from(self.counting);
        match self.rebases.look(&key, offset, &context, pc, keep) {
            Found::Entry(..) => self.outcome.relocated += count,
            Found::Keep => {
                self.outcome.compiled += count;
                self.outcome.kept += count;
                self.rebases
                    .keep(&key, offset, &context, Some(form(offset)));
            }
            Found::Compile => self.outcome.compiled += count,
        }
        let stream = if promoted { TENURED } else { YOUNG };
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
        // The other slide's block at the page offset, as the index finds.
        if instance < 2 * self.guest.hot && self.part_of[instance ^ 1].is_some() {
            self.outcome.aliases += count;
            self.rebases.note_shared(physical);
        }
        self.churn.installed(
            &mut self.counts,
            identity,
            0,
            self.ring.generation(),
            None,
            PARTS as u32,
        );
        self.ring.installed(place.part as usize);
        self.part_of[instance] = Some(place.part);
        self.blocks_of[place.part as usize].push(instance);
        self.outcome.installs += count;
        self.outcome.promoted += count * u64::from(promoted);
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

/// Replay `guest` on a window of `blocks` blocks and kept forms of `bound`
/// bytes under `policy`: per beat, after the warm-up.
pub(super) fn replay(guest: Guest, blocks: usize, bound: usize, policy: Policy) -> Outcome {
    let instances = 2 * guest.hot + guest.new * (WARM_BEATS + BEATS) as usize + 1;
    let mut model = Model {
        guest,
        policy,
        ring: Ring::new(PARTS, blocks / PARTS * BLOCK_BYTES),
        grace: Grace::new(1),
        churn: Churn::new(1 << 20),
        counts: ChurnCounts::default(),
        rebases: Rebases::new(bound),
        part_of: vec![None; instances],
        blocks_of: vec![Vec::new(); PARTS],
        outcome: Outcome::default(),
        counting: false,
    };
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    let odds: Vec<f64> = (0..guest.hot)
        .map(|_| (guest.least.ln() * (1.0 - uniform(&mut seed))).exp() * 4.0 / TICKS as f64)
        .collect();
    // The pages the second slide runs: a fixed share, spread over them.
    let twice: Vec<bool> = (0..guest.hot.div_ceil(PER_PAGE))
        .map(|_| uniform(&mut seed) < guest.twice)
        .collect();
    let mut fresh = 2 * guest.hot;
    let mut runs = Vec::new();
    for beat in 0..WARM_BEATS + BEATS {
        model.counting = beat >= WARM_BEATS;
        for _ in 0..TICKS {
            runs.clear();
            for (block, &odds) in odds.iter().enumerate() {
                if uniform(&mut seed) < odds {
                    runs.push(2 * block);
                }
                if twice[block / PER_PAGE] && uniform(&mut seed) < odds {
                    runs.push(2 * block + 1);
                }
            }
            for _ in 0..guest.new / TICKS as usize {
                runs.push(fresh);
                fresh += 1;
            }
            // The guest's runs interleave.
            for at in (1..runs.len()).rev() {
                runs.swap(at, next(&mut seed) as usize % (at + 1));
            }
            for &instance in &runs {
                match model.part_of[instance] {
                    Some(part) => {
                        if next(&mut seed).is_multiple_of(SAMPLE) {
                            model.ring.entered(part as usize);
                        }
                    }
                    None => model.install(instance),
                }
            }
        }
    }
    let beat = |count: u64| count / BEATS;
    let outcome = model.outcome;
    Outcome {
        installs: beat(outcome.installs),
        compiled: beat(outcome.compiled),
        relocated: beat(outcome.relocated),
        aliases: beat(outcome.aliases),
        promoted: beat(outcome.promoted),
        kept: beat(outcome.kept),
    }
}

/// The Finder window scaled a hundredfold: 27 thousand blocks of window,
/// 870 KiB of kept forms.
const WINDOW: usize = 27_000;
const BOUND: usize = (87 << 20) / 100;

/// The laptop's Finder window, a pool the window holds once but not at both
/// slides: the bound's forms come back for neither policy's blocks before
/// they turn over (a few installs in a hundred are relocated), and keeping
/// the forms of pages two slides run relocates more while it keeps a third
/// fewer forms than keeping those of blocks the ring took away. The ring's
/// installs are the same: a kept form changes what an install costs, not
/// whether it comes.
#[test]
fn a_page_two_slides_run_keeps_fewer_forms_and_relocates_more() {
    let guest = Guest {
        hot: 11_000,
        least: 0.25,
        twice: 0.5,
        new: 500,
    };
    let before = replay(guest, WINDOW, BOUND, Policy::Promoted);
    let after = replay(guest, WINDOW, BOUND, Policy::Shared);
    assert_eq!(before.installs, after.installs, "{before:?} {after:?}");
    assert!(before.relocated * 10 <= before.installs, "{before:?}");
    assert!(after.relocated >= before.relocated, "{before:?} {after:?}");
    assert!(after.kept * 4 <= before.kept * 3, "{before:?} {after:?}");
}

/// The table behind it: pools, shares at the second slide and bounds,
/// under both policies. Manual: `cargo test -p stela-engine --lib
/// kept_forms_replay -- --ignored --nocapture`.
#[test]
#[ignore = "manual: opt-in"]
fn kept_forms_replay_of_the_finder_window() {
    for hot in [11_000, 16_000, 22_000, 30_000] {
        for twice in [0.0, 0.25, 0.5, 0.9] {
            for new in [500, 1_500] {
                let guest = Guest {
                    hot,
                    least: 0.25,
                    twice,
                    new,
                };
                for bound in [BOUND / 2, BOUND, 2 * BOUND] {
                    let before = replay(guest, WINDOW, bound, Policy::Promoted);
                    let after = replay(guest, WINDOW, bound, Policy::Shared);
                    std::println!(
                        "REPLAY hot={hot}00 twice={twice} new={new}00 bound={}K \
                         installs={} aliases={} promoted={} | Promoted compiled={} \
                         relocated={} kept={} | Shared compiled={} relocated={} kept={} \
                         (per beat, x100 for the laptop)",
                        bound >> 10,
                        before.installs,
                        before.aliases,
                        before.promoted,
                        before.compiled,
                        before.relocated,
                        before.kept,
                        after.compiled,
                        after.relocated,
                        after.kept,
                    );
                }
            }
        }
    }
}
