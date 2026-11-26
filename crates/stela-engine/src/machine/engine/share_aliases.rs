//! One translation for every virtual alias of the same physical code
//! (`--share-aliases on`): a block compiled position independent
//! (`stela_compiler::Options::with_position_independent`) holds each guest
//! address as its offset from the page it runs at, which the state's page
//! base gives (`GuestState::page_base`), so a fetch at another address of
//! the same frame and variant takes a slot of its own, an alias, that runs
//! the first block's code: no window is compiled or installed again.
//!
//! The page base is the page of the block native code runs at every time
//! it runs: the engine sets it before each native call
//! ([`Machine::execute`]), every probe, jump cache and stamped tail hit and
//! every paired return sets it to its target's page
//! (`chain::lookup_dispatcher_for`), and a link within a page keeps it.
//! Code that is not position independent never reads it.
//!
//! An independent block (its code position independent, compiled in this
//! boot, with no native completion, HLE, tier 2 or device stub) is the
//! owner of its code. Its aliases share the code and so its links: only
//! the owner's record links, so an eviction finds every link of the code
//! among the blocks of the owner's page, as for any block. Its exits link
//! within the page only to blocks whose code is position independent too,
//! and out of it only through tails that read the page base
//! (`chain::relative_lookup_tail`): a cell or stamp holds one address's
//! target. Its dynamic exits take the lookups every block's do, which
//! compare the target in RAX at run time.
//!
//! A link names the block it enters by a constant, so a run of shared code
//! may end in a block of the page another alias linked from. The run's
//! block and the page base then name the alias that ran
//! ([`Machine::shared_block`]): the slot of that code at that page, made
//! now if the code has none there. Every alias joins its owner's arena part
//! list, so it goes with its code; its fault rows are the owner's moved to
//! its page.
use super::*;

/// What a block's slot is besides its code ([`Block::marks`]).
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(super) struct Marks(u8);

impl Marks {
    const DEVICE_STUB: u8 = 1;
    const INDEPENDENT: u8 = 2;
    const ALIAS: u8 = 4;

    /// A device site's stub, or an independent block (the owner of its
    /// position independent code), or neither.
    pub(super) const fn new(device_stub: bool, independent: bool) -> Self {
        Self(
            if device_stub { Self::DEVICE_STUB } else { 0 }
                | if independent { Self::INDEPENDENT } else { 0 },
        )
    }

    /// An alias of an independent block's code.
    pub(super) const ALIASED: Self = Self(Self::ALIAS);

    pub(super) const fn device_stub(self) -> bool {
        self.0 & Self::DEVICE_STUB != 0
    }

    /// The owner of position independent code.
    pub(super) const fn independent(self) -> bool {
        self.0 & Self::INDEPENDENT != 0
    }

    pub(super) const fn alias(self) -> bool {
        self.0 & Self::ALIAS != 0
    }

    /// Code that runs at every address of its page: an owner's or an
    /// alias's.
    pub(super) const fn shared(self) -> bool {
        self.0 & (Self::INDEPENDENT | Self::ALIAS) != 0
    }
}

/// The board's slots of shared code, by the code and the page each runs it
/// at, owners among them; read and written under its own lock, inside a
/// shard's when both are held. An entry may outlive its slot: each is
/// checked against the slot's block before it is used.
#[derive(Default)]
pub(super) struct Table {
    slots: BTreeMap<(usize, u64), u32>,
    owners: BTreeMap<usize, u32>,
}

/// Entries past which [`Table`] drops those whose slots went.
const TABLE_BOUND: usize = 1 << 16;

/// What sharing did, for the beat's log (`PES SHARE ALIASES`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[cfg_attr(
    all(feature = "std", feature = "native-profile"),
    derive(serde::Serialize)
)]
pub struct ShareAliasCounts {
    /// Native runs whose last block's page was the page base, and runs
    /// whose last block was shared code another alias reached, whose
    /// alias it named instead.
    pub checks: u64,
    pub mapped: u64,
    /// Runs that ended in a block whose code is not position independent
    /// at a page base not its own: a transfer that did not set it. Zero in
    /// a correct engine.
    pub mismatches: u64,
    /// Aliases made at a fetch's miss and at a run's end, and of those the
    /// ones the index held another block's place for, which are found only
    /// by their code.
    pub shared_fetches: u64,
    pub made: u64,
    pub unindexed: u64,
    /// Aliases that could not be made (no slot, the owner went): the fetch
    /// compiled its own block, or the run's exit stopped the engine.
    pub failed: u64,
    /// Blocks installed as owners, and those compiled position independent
    /// that are not (a native completion, a stored form).
    pub owners: u64,
    pub unshared: u64,
}

impl ShareAliasCounts {
    pub fn add(&mut self, other: &Self) {
        self.checks += other.checks;
        self.mapped += other.mapped;
        self.mismatches += other.mismatches;
        self.shared_fetches += other.shared_fetches;
        self.made += other.made;
        self.unindexed += other.unindexed;
        self.failed += other.failed;
        self.owners += other.owners;
        self.unshared += other.unshared;
    }

    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// Slots of [`Share::found`], a power of two.
const FOUND: usize = 256;

/// A memo of [`Share::found`]: the run's block and its incarnation, the
/// page base, then the alias and its incarnation.
type Found = (u32, u32, u64, u32, u32);

/// A PE's sharing ([`Machine::set_share_aliases`]).
pub(super) struct Share {
    pub(super) on: bool,
    /// The aliases runs' ends named last.
    found: Box<[Found]>,
}

impl Default for Share {
    fn default() -> Self {
        Self {
            on: false,
            found: vec![(u32::MAX, 0, 0, 0, 0); FOUND].into_boxed_slice(),
        }
    }
}

const PAGE: u64 = FETCH_PAGE - 1;

impl<H: Host, D: Devices> Machine<H, D> {
    /// Share one translation among the virtual aliases of the same
    /// physical code (`--share-aliases on`), before anything is compiled:
    /// blocks compile position independent where they can, and a fetch at
    /// another address of an independent block's frame takes an alias of
    /// it. Off leaves every block as before.
    pub fn set_share_aliases(&mut self, on: bool) {
        if self.share.on != on {
            self.share.on = on;
            // What blocks compile for, and the contexts the store keys
            // them by, change with it.
            self.preparations.clear();
            self.stored_contexts.clear();
            self.lookup_dispatchers = Default::default();
        }
    }

    /// Whether blocks share translations across aliases.
    pub fn shares_aliases(&self) -> bool {
        self.share.on
    }

    /// The block a native run that ended in block `index` at the page base
    /// `page` ended in: `index` itself in its own page, else the alias of
    /// its code at `page`, made now if it has none there. Checks that only
    /// shared code is reached at another page base ([`ShareAliasCounts`]).
    pub(super) fn shared_block(&mut self, index: usize, page: u64) -> Result<usize, Stop> {
        let Some(block) = self.code.blocks.get(index) else {
            return Ok(index);
        };
        if block.pc & !PAGE == page {
            self.statistics.share_aliases.checks += 1;
            return Ok(index);
        }
        if !block.marks.shared() {
            self.statistics.share_aliases.mismatches += 1;
            return Ok(index);
        }
        let incarnation = self.code.blocks.record(index).incarnation();
        let at = ((index as u64 ^ page >> 12).wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 56) as usize
            & (FOUND - 1);
        let (held, held_incarnation, held_page, alias, alias_incarnation) = self.share.found[at];
        if held as usize == index
            && held_incarnation == incarnation
            && held_page == page
            && self.code.blocks.record(alias as usize).incarnation() == alias_incarnation
            && self.code.blocks.record(alias as usize).live()
        {
            self.statistics.share_aliases.mapped += 1;
            return Ok(alias as usize);
        }
        let (code, pc) = (block.code, page | (block.pc & PAGE));
        let alias = match self.slot_of(code, page) {
            Some(alias) => alias,
            None => match self.make_alias(index, pc, false)? {
                Some(alias) => alias,
                None => {
                    self.statistics.share_aliases.failed += 1;
                    return Err(self.unsupported(RequirementKind::Native(
                        "shared code ran at a page whose alias could not be made",
                    )));
                }
            },
        };
        self.statistics.share_aliases.mapped += 1;
        let alias_incarnation = self.code.blocks.record(alias).incarnation();
        self.share.found[at] = (
            index as u32,
            incarnation,
            page,
            alias as u32,
            alias_incarnation,
        );
        Ok(alias)
    }

    /// The live slot that runs `code` at `page`, if the table holds one.
    fn slot_of(&self, code: NativeCode, page: u64) -> Option<usize> {
        let table = self
            .code
            .aliases
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let slot = *table.slots.get(&(code.0, page))? as usize;
        self.code
            .blocks
            .installed(slot)
            .is_some_and(|block| block.code == code && block.pc & !PAGE == page)
            .then_some(slot)
    }

    /// The owner of alias `index`'s code and the PC there of `pc`, a PC of
    /// the alias's page; `index` and `pc` themselves for any other block.
    /// `None` for an alias whose owner went: nothing links from it.
    pub(super) fn link_owner(&self, index: usize, pc: u64) -> Option<(usize, u64)> {
        let block = &self.code.blocks[index];
        if !block.marks.alias() {
            return Some((index, pc));
        }
        let owner = {
            let table = self
                .code
                .aliases
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            *table.owners.get(&block.code.0)? as usize
        };
        let held = self.code.blocks.installed(owner)?;
        if held.code != block.code || !held.marks.independent() {
            return None;
        }
        let delta = (held.pc & !PAGE).wrapping_sub(block.pc & !PAGE);
        Some((owner, pc.wrapping_add(delta)))
    }

    /// Note block `index`, just installed, as the owner of its code.
    pub(super) fn note_owner(&self, index: usize) {
        let block = &self.code.blocks[index];
        let mut table = self
            .code
            .aliases
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        table.owners.insert(block.code.0, index as u32);
        table
            .slots
            .insert((block.code.0, block.pc & !PAGE), index as u32);
    }

    /// At a fetch the index missed, an alias at `pc` of an independent
    /// block of the same frame offset (`physical`) and variant `id` at
    /// another PC: its code runs here without a compile or an install.
    pub(super) fn share_on_miss(
        &mut self,
        pc: u64,
        physical: u64,
        id: VariantId,
    ) -> Result<Option<usize>, Stop> {
        let owner = {
            let code = &self.code;
            let mut cache = code.page_at(physical, board::LockSite::Install);
            if let Some(held) = cache.index.get(&code.blocks, pc, physical, id) {
                return Ok(Some(held));
            }
            cache.index.at_offset(physical).find(|&slot| {
                code.blocks.installed(slot).is_some_and(|block| {
                    block.marks.independent()
                        && block.variant == id
                        && block.physical == physical
                        && block.pc != pc
                })
            })
        };
        let Some(owner) = owner else {
            return Ok(None);
        };
        let made = self.make_alias(owner, pc, true)?;
        match made {
            Some(_) => self.statistics.share_aliases.shared_fetches += 1,
            None => self.statistics.share_aliases.failed += 1,
        }
        Ok(made)
    }

    /// A new slot at `pc` for the code of block `source`, an owner or an
    /// alias, in its page's place: its successors and fault rows moved to
    /// `pc`'s page, in its owner's arena part list, indexed unless the
    /// index holds another block of its key there (for `fetch`, that block
    /// is returned instead). `None` when no slot is left or the code went.
    fn make_alias(&mut self, source: usize, pc: u64, fetch: bool) -> Result<Option<usize>, Stop> {
        let shared = Arc::clone(&self.code);
        let Some(block) = shared.blocks.installed(source) else {
            return Ok(None);
        };
        let (code, physical, variant) = (block.code, block.physical, block.variant);
        let delta = (pc & !PAGE).wrapping_sub(block.pc & !PAGE);
        let rows = shared
            .blocks
            .recovery()
            .get(&source)
            .and_then(|owner| owner.rebased(delta));
        let Some(rows) = rows else {
            return Ok(None);
        };
        let slot = match self.take_slot() {
            Some(slot) => slot,
            None => match self.retire_for_slot() {
                Some(slot) => slot,
                None => return Ok(None),
            },
        };
        let Some(block) = shared.blocks.installed(source) else {
            self.release_alias(slot);
            return Ok(None);
        };
        let mut successors = core::mem::take(&mut self.successor_scratch);
        successors.clear();
        successors.extend(block.successors.iter().map(|exit| Successor {
            target: GuestAddress(exit.target.0.wrapping_add(delta)),
            ..*exit
        }));
        let alias = Block {
            pc,
            physical,
            code,
            instructions: block.instructions,
            variant,
            successors: Box::default(),
            dynamic_site: block.dynamic_site,
            landing: block.landing,
            terminal: block.terminal,
            source_hash: block.source_hash,
            installer: self.current as u8,
            part: block.part,
            marks: Marks::ALIASED,
            hle: None,
            tier2: false,
        };
        if let Some(old) = shared.blocks.vacate(slot) {
            self.keep_successor_box(old.successors);
        }
        let boxed = self.successor_box(&successors);
        self.successor_scratch = successors;
        shared.blocks.fill(
            slot,
            Block {
                successors: boxed,
                ..alias
            },
        );
        shared.blocks.recovery().insert(slot, rows);
        let mut cache = shared.page_at(physical, board::LockSite::Install);
        let held = cache.index.get(&shared.blocks, pc, physical, variant);
        if fetch && let Some(held) = held {
            drop(cache);
            self.release_alias(slot);
            return Ok(Some(held));
        }
        // The code is still its source's, in a part a retirement has not
        // chosen: listed under the part lists' lock, as an install is.
        let listed = shared
            .blocks
            .installed(source)
            .is_some_and(|block| block.code == code)
            && {
                let mut code_slots = shared
                    .code_slots
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                let live = match &shared.arena {
                    Some(arena) => arena
                        .part_of(code.0)
                        .is_some_and(|part| arena.ring.part_live(part)),
                    None => true,
                };
                if live {
                    code_slots.record(code.0, slot, || {
                        shared
                            .arena
                            .as_ref()
                            .map(board::Arena::partitions)
                            .or_else(|| self.host.code_partitions())
                    });
                }
                live
            };
        if !listed {
            drop(cache);
            self.release_alias(slot);
            return Ok(None);
        }
        if held.is_none() {
            cache.index.insert(&shared.blocks, slot);
        } else {
            self.statistics.share_aliases.unindexed += 1;
        }
        {
            let mut table = shared
                .aliases
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if table.slots.len() > TABLE_BOUND {
                table
                    .slots
                    .retain(|_, slot| shared.blocks.installed(*slot as usize).is_some());
                table
                    .owners
                    .retain(|_, slot| shared.blocks.installed(*slot as usize).is_some());
            }
            table.slots.insert((code.0, pc & !PAGE), slot as u32);
        }
        drop(cache);
        self.statistics.share_aliases.made += 1;
        Ok(Some(slot))
    }

    /// Give back `slot`, filled for an alias that was never published.
    fn release_alias(&mut self, slot: usize) {
        let shared = Arc::clone(&self.code);
        if shared.blocks.record(slot).live() {
            shared.blocks.record(slot).mark_lost();
            shared.blocks.recovery().remove(&slot);
        }
        shared.blocks.retire(slot);
        let (grace, _) = self.grace();
        shared
            .blocks
            .release(alloc::vec![slot as u32], grace.snapshot());
    }

    /// The live aliases of the code of block `index` (an owner), which go
    /// with it when the index drops it alone.
    pub(super) fn aliases_of(&self, index: usize) -> Vec<u32> {
        let Some(block) = self.code.blocks.installed(index) else {
            return Vec::new();
        };
        let code = block.code;
        let table = self
            .code
            .aliases
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        table
            .slots
            .range((code.0, 0)..=(code.0, u64::MAX))
            .map(|(_, &slot)| slot)
            .filter(|&slot| {
                slot as usize != index
                    && self
                        .code
                        .blocks
                        .installed(slot as usize)
                        .is_some_and(|alias| alias.code == code && alias.marks.alias())
            })
            .collect()
    }
}
