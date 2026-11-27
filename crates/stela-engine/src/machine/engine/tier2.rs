//! Tier 2 (`--tier2`): hot blocks compiled again as superblocks.
//!
//! A block the engine finds hot is compiled again with
//! [`NativeSystem::traces`]: its region runs on past every branch it would
//! end at, to its first terminal that is not a branch, whether or not it
//! holds a loop. Its direct branches into itself jump there, its others
//! leave by side exits, NZCV is captured only where a path reads it, and
//! one run's locals stay in host registers across it. It is a block in
//! every contract the engine and the store know (one entry PC, a
//! contiguous source in one page, one source-map row per word, recovery
//! rows the compiler marks), so faults, exits, interrupts, invalidation and
//! retirement treat it as one; only its code differs.
//!
//! **Heat.** A run whose budget ran out was in its last block at a
//! uniformly chosen transfer, which counts [`EXHAUSTED_HEAT`]; the block a
//! run starts at counts [`ENTRY_HEAT`]. A block whose heat reaches the
//! threshold is promoted after the run, once its exit was completed.
//!
//! **Install.** The tier-2 block takes the tier-1 block's place in the
//! index: the tier-1 block is detached as any replaced block is (its links
//! in, table and cell entries go) and stays in its slot until its part
//! goes, for a PE that still runs it. The tier-2 block is never offered to
//! the translation store or kept for rebasing: the store and the pack hold
//! tier 1 alone, and tier 2 is made again each boot from heat. A block
//! tier 2 leaves as it was (no region forms, or the code is tier 1's) is
//! not promoted again; neither is one a check found wrong.
//!
//! **HLE** (`--hle`, [`super::hle`]). A composite block is never promoted,
//! and no tier-2 install takes its place. A superblock ends before any word
//! where an enabled signature's function starts
//! ([`Machine::hle_entry_after`]), so none of its branches lands on a
//! function entry's original words in place of its composite.
//!
//! **Kept contexts** (`--tlbi-keep on`, `tlbi_keep`). A tier-2 block is
//! indexed by PC, frame and variant as its tier-1 block was, and the ITLB
//! names the frame's block key, not a block: a kept context's verified
//! entry reaches it, a dropped one walks to whatever its frame is now. Its
//! stamps and jump cache entries go with the TLBI's drop epochs as every
//! block's do.
//!
//! **Check** (`--tier2-check`): `tier2_check`.
use super::{Machine, Variant, recovery, source_window};
use crate::machine::{Devices, Host};
use alloc::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};
use stela_compiler::x86_64::NativeSystem;

/// The heat a run adds to the block it ended in when its budget ran out.
pub(super) const EXHAUSTED_HEAT: u32 = 8;
/// The heat a run adds to the block it starts at.
pub(super) const ENTRY_HEAT: u32 = 1;
/// The heat that promotes a block, by default: about eight budget-spent
/// samples, or 64 entries from the engine.
pub const DEFAULT_THRESHOLD: u32 = 64;
/// The blocks whose heat a PE keeps at once; past it the heat starts over.
const HEAT_KEYS: usize = 16 * 1024;
/// The blocks a PE settles (does not promote again) at once.
const SETTLED_KEYS: usize = 64 * 1024;

/// How tier 2 runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tier2Options {
    /// `--tier2-check`: compare sampled entries against tier 1.
    pub check: bool,
    /// The heat that promotes a block.
    pub threshold: u32,
    /// Entries into tier-2 blocks per check.
    pub check_every: u32,
}

impl Tier2Options {
    /// `--tier2 on`, with `--tier2-check` or not.
    pub const fn on(check: bool) -> Self {
        Self {
            check,
            threshold: DEFAULT_THRESHOLD,
            check_every: super::tier2_check::CHECK_EVERY,
        }
    }
}

/// A block's name across its installs: PC, frame and variant.
type Key = (u64, u64, u32);

/// What tier 2 did on one PE, which the boot logs (`PES TIER2`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[cfg_attr(
    all(feature = "std", feature = "native-profile"),
    derive(serde::Serialize)
)]
pub struct Tier2Counts {
    /// Blocks whose heat reached the threshold.
    pub promoted: u64,
    /// Tier-2 blocks installed in their tier-1 blocks' places.
    pub installed: u64,
    /// Promoted blocks tier 2 compiled as tier 1 did, which stay.
    pub unchanged: u64,
    /// Promoted blocks tier 2 could not compile or install.
    pub refused: u64,
    /// Exits to the engine from tier-2 blocks, and of those the side exits
    /// (exits before the block's last instruction).
    pub exits: u64,
    pub exits_side: u64,
    /// Tier-2 blocks taken away after a check found them wrong.
    pub deopts: u64,
    /// Entries `--tier2-check` compared, found to differ from tier 1, and
    /// could not compare (the run faulted, or its block writes memory).
    pub checks: u64,
    pub check_mismatches: u64,
    pub check_skipped: u64,
    /// The entry PC of the last block a check found wrong.
    pub mismatch_pc: u64,
}

impl core::ops::AddAssign for Tier2Counts {
    fn add_assign(&mut self, other: Self) {
        let Self {
            promoted,
            installed,
            unchanged,
            refused,
            exits,
            exits_side,
            deopts,
            checks,
            check_mismatches,
            check_skipped,
            mismatch_pc,
        } = other;
        self.promoted += promoted;
        self.installed += installed;
        self.unchanged += unchanged;
        self.refused += refused;
        self.exits += exits;
        self.exits_side += exits_side;
        self.deopts += deopts;
        self.checks += checks;
        self.check_mismatches += check_mismatches;
        self.check_skipped += check_skipped;
        if mismatch_pc != 0 {
            self.mismatch_pc = mismatch_pc;
        }
    }
}

/// A PE's tier 2: its options, heat and settled blocks.
pub(super) struct Tier2 {
    /// `--tier2-check`.
    pub(super) check: bool,
    /// Entries into tier-2 blocks per check.
    pub(super) check_every: u32,
    /// The heat that promotes a block.
    threshold: u32,
    heat: BTreeMap<Key, u32>,
    settled: BTreeSet<Key>,
    /// The block slot the last run made hot, promoted after its exit.
    pub(super) pending: Option<usize>,
    /// Entries since the last check.
    pub(super) since_check: u32,
}

impl Tier2 {
    pub(super) fn new(options: Tier2Options) -> Self {
        Self {
            check: options.check,
            check_every: options.check_every.max(1),
            threshold: options.threshold.max(1),
            heat: BTreeMap::new(),
            settled: BTreeSet::new(),
            pending: None,
            since_check: 0,
        }
    }

    /// Add `heat` to `key`; whether it reached the threshold now.
    fn warm(&mut self, key: Key, heat: u32) -> bool {
        if self.settled.contains(&key) {
            return false;
        }
        if self.heat.len() >= HEAT_KEYS && !self.heat.contains_key(&key) {
            self.heat.clear();
        }
        let total = self.heat.entry(key).or_insert(0);
        let before = *total;
        *total = total.saturating_add(heat);
        before < self.threshold && *total >= self.threshold
    }

    /// Promote `key` no more.
    pub(super) fn settle(&mut self, key: Key) {
        self.heat.remove(&key);
        if self.settled.len() >= SETTLED_KEYS {
            self.settled.clear();
        }
        self.settled.insert(key);
    }

    /// Promote `key` again when it is hot: its tier-2 block went.
    fn cool(&mut self, key: Key) {
        self.heat.remove(&key);
    }
}

impl<H: Host, D: Devices> Machine<H, D> {
    /// Run tier 2 (`--tier2 on`) with `options`, or not (`None`, the
    /// default).
    pub fn set_tier2(&mut self, options: Option<Tier2Options>) {
        self.tier2 = options.map(|options| alloc::boxed::Box::new(Tier2::new(options)));
    }

    /// Whether tier 2 runs.
    pub fn tier2_enabled(&self) -> bool {
        self.tier2.is_some()
    }

    /// The name of block `slot` across installs.
    fn tier2_key(&self, slot: usize) -> Option<Key> {
        let block = self.code.blocks.get(slot)?;
        Some((block.pc, block.physical, block.variant.0))
    }

    /// Count the run that started at `entry` and ended in `last`, its budget
    /// spent or not.
    pub(super) fn tier2_heat(&mut self, entry: usize, last: usize, exhausted: bool) {
        if self.tier2.is_none() {
            return;
        }
        let mut hot = None;
        for (slot, heat) in [
            (last, if exhausted { EXHAUSTED_HEAT } else { 0 }),
            (entry, ENTRY_HEAT),
        ] {
            if heat == 0 {
                continue;
            }
            let Some(block) = self.code.blocks.get(slot) else {
                continue;
            };
            // An HLE composite is never promoted: it is a routine, not
            // translated code.
            // Nor is shared code: tier 2 holds absolute addresses.
            if block.tier2
                || block.marks.device_stub()
                || block.marks.shared()
                || block.hle.is_some()
            {
                continue;
            }
            let key = (block.pc, block.physical, block.variant.0);
            let tier2 = self.tier2.as_deref_mut().expect("tier 2 runs");
            if tier2.warm(key, heat) && hot.is_none() {
                hot = Some(slot);
            }
        }
        if let Some(slot) = hot {
            let tier2 = self.tier2.as_deref_mut().expect("tier 2 runs");
            tier2.pending = Some(slot);
            self.statistics.tier2.promoted += 1;
        }
    }

    /// Count an exit of block `last` with `skipped` instructions unrun.
    pub(super) fn tier2_exit(&mut self, last: usize, skipped: u16) {
        if self.tier2.is_some() && self.code.blocks.get(last).is_some_and(|block| block.tier2) {
            self.statistics.tier2.exits += 1;
            self.statistics.tier2.exits_side += u64::from(skipped != 0);
        }
    }

    /// Promote the block the last run made hot, if any.
    pub(super) fn tier2_after_run(&mut self) {
        let Some(slot) = self
            .tier2
            .as_deref_mut()
            .and_then(|tier2| tier2.pending.take())
        else {
            return;
        };
        let Some(key) = self.tier2_key(slot) else {
            return;
        };
        match self.promote(slot) {
            Promotion::Installed => {
                self.statistics.tier2.installed += 1;
                // Hot again only once its tier-2 block went.
                if let Some(tier2) = self.tier2.as_deref_mut() {
                    tier2.cool(key);
                }
            }
            Promotion::Unchanged => {
                self.statistics.tier2.unchanged += 1;
                self.tier2_settle(key);
            }
            Promotion::Refused => {
                self.statistics.tier2.refused += 1;
                self.tier2_settle(key);
            }
        }
    }

    fn tier2_settle(&mut self, key: Key) {
        if let Some(tier2) = self.tier2.as_deref_mut() {
            tier2.settle(key);
        }
    }

    /// The variant tier 2 compiles `variant`'s blocks for: its regions
    /// formed with or without a loop.
    fn traced(variant: Variant) -> Variant {
        Variant {
            system: NativeSystem {
                regions: true,
                traces: true,
                ..variant.system
            },
            ..variant
        }
    }

    /// Compile block `slot` for tier 2 and install it in the block's place.
    fn promote(&mut self, slot: usize) -> Promotion {
        let Some(block) = self.code.blocks.get(slot) else {
            return Promotion::Refused;
        };
        if block.tier2 || block.marks.device_stub() || block.marks.shared() || block.hle.is_some() {
            return Promotion::Refused;
        }
        let (pc, physical) = (block.pc, block.physical);
        let variant = self.variants[block.variant.0 as usize];
        // One-instruction blocks are for stepping and breakpoints; the
        // translated variant's pages are rare and keep tier 1's code.
        if variant.single || variant.translated || self.page_has_device_sites(physical) {
            return Promotion::Refused;
        }
        let window = source_window(pc, variant);
        // No block runs into a host breakpoint or trace point: one ends
        // before it, as a compile ends it.
        let limit = self
            .breakpoints
            .iter()
            .chain(&self.traces)
            .filter(|&&stop| stop > pc && stop < pc + window as u64)
            .min()
            .map_or(window, |&stop| (stop - pc) as usize);
        // Before its bytes are read, as a compile marks it.
        if let Some(hub) = &self.parallel {
            hub.compiling(physical);
        }
        let mut staging = [0; super::MAX_BLOCK_BYTES as usize];
        let source = &mut staging[..window];
        if !self.host.read_ram(physical, source) {
            return Promotion::Refused;
        }
        // Nor does one run into an HLE function's entry, where its
        // composite is (or will be) installed: a branch of it landing there
        // would run the original's words in place of the composite.
        let limit = self
            .hle_entry_after(pc, physical, source)
            .map_or(limit, |entry| entry.min(limit));
        let first = self.preparation(variant);
        first.admit(source);
        let preparation = self.preparation(Self::traced(variant));
        let Some(Ok(super::prepare::Prepared::Block {
            compiled, input, ..
        })) = preparation.prepare_window(source, pc, limit, true)
        else {
            return Promotion::Refused;
        };
        if !compiled.region {
            return Promotion::Unchanged;
        }
        // A region tier 1 formed too (one with a loop): the same code.
        if let Some(Ok(super::prepare::Prepared::Block {
            compiled: tier1, ..
        })) = first.prepare_window(source, pc, limit, true)
            && tier1.native.x86_64_code == compiled.native.x86_64_code
        {
            return Promotion::Unchanged;
        }
        let emission = Arc::new(super::stored::Emission::new(
            source[..input].to_vec(),
            compiled,
            preparation,
        ));
        let template = recovery::Template::compiled(emission);
        if template.portable_requires_unavailable_capture() || !template.matches_source(source) {
            return Promotion::Refused;
        }
        self.installing_tier2 = true;
        let installed = self.install(pc, physical, variant, source, template);
        self.installing_tier2 = false;
        match installed {
            Ok(super::Fetched::Block(index))
                if self.code.blocks.get(index).is_some_and(|block| block.tier2) =>
            {
                Promotion::Installed
            }
            _ => Promotion::Refused,
        }
    }

    /// Take tier-2 block `slot` away, and promote its block no more: what
    /// its page's blocks are compiled as next is tier 1's.
    pub(super) fn tier2_deopt(&mut self, slot: usize) {
        let Some(key) = self.tier2_key(slot) else {
            return;
        };
        self.tier2_settle(key);
        self.statistics.tier2.deopts += 1;
        let physical = key.1;
        self.invalidate_code_page(physical);
        self.post_code_page(physical);
    }
}

/// What a promotion did.
enum Promotion {
    Installed,
    Unchanged,
    Refused,
}
