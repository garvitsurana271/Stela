//! "Verify instead of forget" (`--tlbi-keep on`): a TLBI of everything
//! keeps the lookup contexts and their ITLB entries, marked unverified, and
//! the next fetch in each checks every entry of its context against the
//! tables before any lookup uses one again. Off by default.
//!
//! A TLB may hold any translation a walk would give now. A walk's result is
//! a function of the controls, which a context's key holds (all but TTBR0 and
//! the SPRR permissions in its frame, those two in the key itself), and of
//! the descriptors it reads: each read's address follows from the controls
//! and the values read before it. So a walk that reads the same addresses
//! and finds the same values gives the same result. Every ITLB fill records
//! its walk ([`WalkRecord`]): each descriptor address and value it read, over
//! both stages, in order (the leaf and every table on its path), the TTBR0
//! it walked from and what it gave. Verification ([`Machine::verify_kept`])
//! keeps an entry only where:
//!
//! - every recorded descriptor still holds its value;
//! - for a lower-half page, TTBR0 is the one the walk started from: the
//!   upper half's context, which holds the lower half's global pages
//!   (`itlb_context`), has no TTBR0 in its key, and XNU flushes everything
//!   precisely when the next process's shared region (global pages) differs;
//! - the host still permits the fetch (`Host::fetch_permitted`), which is
//!   host state and in no descriptor;
//! - the entry is what a fill would make of the walk now: the block key of
//!   the frame in the page's variant as it is now (`page_variant`, which
//!   reads host and board state about the frame) and the global mark.
//!
//! Anything else drops the entry (and grows its half's drop epoch, as every
//! drop does), and its page is walked again at the fetch. A context with an
//! entry no walk record covers, or met in another state than its key's, is
//! forgotten as a TLBI of everything forgets them all without the keep.
//!
//! Nothing reaches a kept context's entries before its verification:
//! - [`contexts::Contexts::find`] does not give a kept context's number, and
//!   every number native code holds comes from it: the fetch's contexts
//!   ([`Machine::contexts`]), the native TTBR0 switch table's entries
//!   (`keep_table_switches` takes `switch_contexts`) and the transition
//!   records' (`refresh_transition_contexts` takes `live_contexts`);
//! - the TLBI forgets the switch table and moves the transition records'
//!   generation ([`Machine::advance_translation_generation`]), as before;
//! - the chain's contexts become [`contexts::VOID`], which no entry has;
//! - both halves' ITLB drop epochs grow, so no stamp or jump cache entry made
//!   before the TLBI holds after it (a stamp may name a page whose ITLB
//!   entry an eviction took, which no verification sees);
//! - the filters TLBIs by VA reach entries through are cleared, as before,
//!   and verification records each entry it keeps in them again, as a walk
//!   does. A TLBI by VA between the TLBI of everything and a verification
//!   needs to reach no kept entry: verification reads the tables after it.
//!
//! Verification runs inside one fetch on this PE: no mail is read during it.
//! Another PE's table write and its TLBI by VA race it as they race a walk,
//! and as for a walk the entry is in the filters before it is used.
//!
//! `--tlbi-verify-check` walks every entry verification keeps in full as
//! well, and drops it, counted as a mismatch with a sample, where the walk
//! gives anything else: a check of the argument above on a real boot.
//!
//! **Cost.** A TLBI of everything marks the contexts (one store each) and
//! bumps two epochs. A verification reads each of the context's records
//! where it lies (`TlbiKeep::store`, indexed by context and page): a kept
//! entry moves nothing, and a dropped one leaves its own slot
//! (`LookupTable::set`), not every set its 16 KiB granule lists across all
//! contexts: with a thousand contexts sharing each VA and boot 27's drop
//! rate, that search was most of the cost in `tests/tlbi_keep_cost.rs`,
//! which times the keep against forgetting.

use super::*;

/// The descriptor reads a [`WalkRecord`] holds: four levels of stage 1 (five
/// with 52-bit addresses) and room to spare. A walk that reads more (stage 2
/// under stage 1) has no record, which makes its context unverifiable.
pub(super) const WALK_READS: usize = 8;

/// Entries of a verification's cache of descriptors read already: the
/// tables a context's pages share are read once.
const CONFIRMED: usize = 64;

/// The walk an ITLB entry was filled from: what it read, what it started
/// from and what it gave.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct WalkRecord {
    /// The page walked, and its frame: another page of the same leaf
    /// (`take_leaf_pages`) is at the same offset in the leaf's output.
    pub(super) page: u64,
    pub(super) physical: u64,
    /// The bytes the leaf maps, and its guarded and global bits.
    pub(super) size: u64,
    pub(super) guarded: bool,
    pub(super) global: bool,
    /// The TTBR0 value of the controls it walked under.
    pub(super) ttbr0: u64,
    reads: [(u64, u64); WALK_READS],
    len: u8,
}

impl WalkRecord {
    fn reads(&self) -> &[(u64, u64)] {
        &self.reads[..usize::from(self.len)]
    }

    /// Whether the page `page` is in this walk's leaf.
    fn covers(&self, page: u64) -> bool {
        self.size >= FETCH_PAGE
            && self.size.is_power_of_two()
            && (page ^ self.page) & !(self.size - 1) == 0
    }
}

/// The descriptor reads of one walk, in order ([`RecordingTables`]).
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct WalkReads {
    reads: [(u64, u64); WALK_READS],
    len: u8,
    /// More reads than fit, or a descriptor update: no record.
    spoilt: bool,
}

impl WalkReads {
    /// The address of the last descriptor the walk read, if it read any
    /// within the record's room.
    pub(super) fn last_address(&self) -> Option<u64> {
        self.reads[..usize::from(self.len)]
            .last()
            .map(|&(address, _)| address)
    }

    /// The record of this walk of `pc` under `ttbr0`, which gave
    /// `translation`.
    pub(super) fn record(
        &self,
        pc: u64,
        translation: &mmu::Translation,
        ttbr0: u64,
    ) -> Option<WalkRecord> {
        (!self.spoilt && self.len != 0).then_some(WalkRecord {
            page: pc & !(FETCH_PAGE - 1),
            physical: translation.physical & !(FETCH_PAGE - 1),
            size: translation.size,
            guarded: translation.guarded,
            global: translation.global,
            ttbr0,
            reads: self.reads,
            len: self.len,
        })
    }
}

/// [`RamTables`] noting every descriptor a walk reads ([`WalkReads`]).
pub(super) struct RecordingTables<'a, H: Host> {
    host: &'a mut H,
    pub(super) reads: WalkReads,
}

impl<'a, H: Host> RecordingTables<'a, H> {
    pub(super) fn new(host: &'a mut H) -> Self {
        Self {
            host,
            reads: WalkReads::default(),
        }
    }
}

impl<H: Host> mmu::TableMemory for RecordingTables<'_, H> {
    fn read_descriptor(&mut self, physical: u64) -> Option<u64> {
        let value = self.host.read_ram_descriptor(physical);
        let reads = &mut self.reads;
        match value {
            Some(value) if usize::from(reads.len) < WALK_READS => {
                reads.reads[usize::from(reads.len)] = (physical, value);
                reads.len += 1;
            }
            // A read that fails ends the walk in a fault, which no entry
            // records; one past the room spoils the record.
            Some(_) | None => reads.spoilt = true,
        }
        value
    }

    fn update_descriptor(&mut self, physical: u64, old: u64, new: u64) -> Option<bool> {
        // The walker makes none (no FEAT_HAFDBS); one would change what the
        // record read.
        self.reads.spoilt = true;
        RamTables(&mut *self.host).update_descriptor(physical, old, new)
    }
}

/// Variants a verification's cache keeps the numbers of.
const VARIANTS_SEEN: usize = 4;

/// A verification's cache of descriptors it read, by address, and of the
/// numbers of the variants its entries' frames took (`known_variant`
/// searches every variant; a context's pages take one or two).
struct Confirmed(
    [(u64, Option<u64>); CONFIRMED],
    [Option<(Variant, Option<VariantId>)>; VARIANTS_SEEN],
    usize,
);

impl Confirmed {
    fn new() -> Self {
        Self([(u64::MAX, None); CONFIRMED], [None; VARIANTS_SEEN], 0)
    }

    /// The number of `variant`, through the cache.
    fn variant<H: Host, D: Devices>(
        &mut self,
        machine: &Machine<H, D>,
        variant: Variant,
    ) -> Option<VariantId> {
        if let Some((_, id)) = self.1.iter().flatten().find(|(held, _)| *held == variant) {
            return *id;
        }
        let id = machine.known_variant(variant);
        self.1[self.2 % VARIANTS_SEEN] = Some((variant, id));
        self.2 += 1;
        id
    }

    fn slot(address: u64) -> usize {
        ((address >> 3).wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 58) as usize % CONFIRMED
    }
}

/// One PE's state for `--tlbi-keep on`.
#[derive(Default)]
pub(super) struct TlbiKeep {
    /// `--tlbi-keep on`: a TLBI of everything keeps the contexts.
    pub(super) on: bool,
    /// `--tlbi-verify-check`: every entry verification keeps is walked in
    /// full as well.
    pub(super) check: bool,
    /// The walk each ITLB entry was filled from, by context and page, as
    /// its place in `store`. An entry the ITLB evicted leaves its record
    /// until a verification or a prune finds it gone. The index holds
    /// places, not records: a verification reads a context's records where
    /// they are and moves none it keeps.
    records: BTreeMap<(u64, u64), u32>,
    /// The records by place, and the places of records that went.
    store: Vec<WalkRecord>,
    free: Vec<u32>,
    /// Contexts with an ITLB entry no record covers, which no verification
    /// can keep.
    incomplete: BTreeSet<u64>,
    /// The walk of each fetch cache entry, with the entry it made
    /// ([`Machine::fetch_record`]).
    fetch_walks: Vec<Option<(FetchEntry, WalkRecord)>>,
    /// Test only: kept contexts are confirmed without a check, the mutation
    /// the acceptance tests must catch.
    #[cfg(test)]
    pub(super) unverified: bool,
}

impl TlbiKeep {
    /// Record the walk of `context`'s entry of `page`, over the one before.
    fn put(&mut self, context: u64, page: u64, record: WalkRecord) {
        if let Some(&at) = self.records.get(&(context, page)) {
            self.store[at as usize] = record;
            return;
        }
        let at = match self.free.pop() {
            Some(at) => {
                self.store[at as usize] = record;
                at
            }
            None => {
                self.store.push(record);
                (self.store.len() - 1) as u32
            }
        };
        self.records.insert((context, page), at);
    }

    /// Drop the record of `context`'s entry of `page`, if it has one.
    fn take(&mut self, context: u64, page: u64) {
        if let Some(at) = self.records.remove(&(context, page)) {
            self.free.push(at);
        }
    }

    /// The pages `context` has records of, with their places.
    fn pages(&self, context: u64) -> Vec<(u64, u32)> {
        self.records
            .range((context, 0)..=(context, u64::MAX))
            .map(|(&(_, page), &at)| (page, at))
            .collect()
    }
}

impl<H: Host, D: Devices> Machine<H, D> {
    /// Keep the lookup contexts across a TLBI of everything and verify them
    /// before use (`keep`, `--tlbi-keep on`), walking every entry kept in
    /// full as well (`check`, `--tlbi-verify-check`). Contexts made before
    /// have entries without walk records: they go.
    pub fn set_tlbi_keep(&mut self, keep: bool, check: bool) {
        if keep && !self.tlbi_keep.on && self.spaces.iter().any(|spaces| spaces.len() != 0) {
            self.forget_spaces(0..=1, 3);
        }
        self.tlbi_keep = TlbiKeep {
            on: keep,
            check: keep && check,
            fetch_walks: if keep {
                vec![None; FETCH_CACHE]
            } else {
                Vec::new()
            },
            ..TlbiKeep::default()
        };
    }

    /// Whether this machine keeps its contexts across a TLBI of everything,
    /// and checks what it keeps.
    pub fn tlbi_keep(&self) -> (bool, bool) {
        (self.tlbi_keep.on, self.tlbi_keep.check)
    }

    /// The fetch cache's entry in `slot` came from a walk that read `reads`
    /// under `ttbr0` and gave `translation`.
    pub(super) fn note_fetch_walk(
        &mut self,
        slot: usize,
        entry: FetchEntry,
        reads: &WalkReads,
        translation: &mmu::Translation,
        ttbr0: u64,
    ) {
        let record = reads.record(entry.page, translation, ttbr0);
        if let Some(held) = self.tlbi_keep.fetch_walks.get_mut(slot) {
            *held = record.map(|record| (entry, record));
        }
    }

    /// The walk the fetch cache's current entry of the page of `pc` came
    /// from, when it was recorded.
    pub(super) fn fetch_record(&self, pc: u64) -> Option<WalkRecord> {
        if !self.tlbi_keep.on {
            return None;
        }
        let entry = self.cached_fetch(pc)?;
        let slot = (entry.page / FETCH_PAGE) as usize % FETCH_CACHE;
        match self.tlbi_keep.fetch_walks.get(slot)? {
            Some((held, record)) if *held == entry => Some(*record),
            _ => None,
        }
    }

    /// The ITLB holds the page `page` in `context`, from the walk `record`
    /// (none: unknown, so the context can no longer be verified).
    pub(super) fn note_kept_fill(&mut self, page: u64, context: u64, record: Option<WalkRecord>) {
        let limit = 2 * self.itlb.entries();
        if self.tlbi_keep.records.len() >= limit {
            self.prune_kept_records();
        }
        let keep = &mut self.tlbi_keep;
        match record.filter(|record| record.covers(page)) {
            Some(record) if keep.records.len() < limit => keep.put(context, page, record),
            _ => {
                keep.take(context, page);
                keep.incomplete.insert(context);
            }
        }
    }

    /// Drop the records of entries the ITLB no longer holds, and of
    /// contexts that went.
    fn prune_kept_records(&mut self) {
        let (itlb, spaces) = (&self.itlb, &self.spaces);
        let live = |number: u64| spaces.iter().any(|spaces| spaces.live(number).is_some());
        let TlbiKeep { records, free, .. } = &mut self.tlbi_keep;
        records.retain(|&(context, page), &mut at| {
            let held = itlb.find(itlb.page_slot(page, context), page, context);
            let stays = held.pc == page && held.key == context && live(context);
            if !stays {
                free.push(at);
            }
            stays
        });
        self.tlbi_keep.incomplete.retain(|&number| live(number));
    }

    /// The contexts of `half` go: their records with them.
    pub(super) fn forget_kept_records(&mut self, half: usize) {
        if !self.tlbi_keep.on {
            return;
        }
        let numbers: Vec<u64> = self.spaces[half].iter().map(|space| space.number).collect();
        for number in numbers {
            self.take_kept_records(number);
            self.tlbi_keep.incomplete.remove(&number);
        }
    }

    /// Take the records of `context`'s entries out: their pages.
    fn take_kept_records(&mut self, context: u64) -> Vec<u64> {
        let pages = self.tlbi_keep.pages(context);
        for &(page, _) in &pages {
            self.tlbi_keep.take(context, page);
        }
        pages.into_iter().map(|(page, _)| page).collect()
    }

    /// A TLBI of everything with `--tlbi-keep on`: every context stays,
    /// unverified, unreachable until its next fetch verifies it (the module
    /// documentation says why nothing reaches it before).
    ///
    /// The upper half is kept only while its keys leave out the ASID (no
    /// upper-half fetch was non-global since its contexts were last
    /// forgotten), as a forget would reset that: else it is forgotten as
    /// without the keep, so that native TTBR0 switches, which need it, come
    /// back.
    pub(super) fn keep_spaces(&mut self) {
        if self.private_fetches[1] {
            self.forget_contexts(1..=1, 1);
        }
        let kept = self.spaces[0].keep_all() + self.spaces[1].keep_all();
        self.statistics.tlbi_keep.tlbis += 1;
        self.statistics.tlbi_keep.contexts += kept as u64;
        for half in 0..2 {
            self.recent[half] = [None; RECENT];
            self.clear_fetched_leaves(half);
        }
        // A span of one granule in each half, which holds no entry: both
        // halves' drop epochs grow (`LookupTable::forget_range_where`), so
        // every stamp and jump cache entry this PE made goes.
        const UPPER_GRANULE: u64 = !0 << 14;
        self.itlb.forget_range_where(0..=0, |_| false);
        self.itlb
            .forget_range_where(UPPER_GRANULE..=UPPER_GRANULE, |_| false);
        self.chain.set_contexts([contexts::VOID; 2]);
        self.advance_translation_generation();
    }

    /// Verify the kept context `number` of `half` before a fetch at `el` in
    /// `variant` runs in it: whether it stays, every entry of it kept one a
    /// walk now gives ([`Self::kept_walk_holds`]) and in the filters, every
    /// other dropped and its page walked again at the fetch. One that cannot
    /// be verified is forgotten, its pages walked again.
    pub(super) fn verify_kept(
        &mut self,
        half: usize,
        el: u8,
        variant: Variant,
        number: u64,
    ) -> bool {
        let started = self.host.counter();
        let current = self.cpu.pstate.el | u8::from(self.cpu.pstate.guarded) << 4;
        let incomplete = self.tlbi_keep.incomplete.remove(&number);
        let verified = if el != current || incomplete {
            let pages = self.take_kept_records(number);
            self.refills.extend(pages);
            self.spaces[half].remove_number(number);
            self.statistics.tlbi_keep.forgotten += 1;
            false
        } else {
            self.verify_records(number, variant);
            self.spaces[half].confirm(number);
            self.statistics.tlbi_keep.verified += 1;
            true
        };
        self.statistics.tlbi_keep.ticks += self.host.counter().wrapping_sub(started);
        verified
    }

    /// [`Self::verify_kept`]'s check of each record of the context `number`.
    /// A kept entry's record stays where it is. A dropped entry leaves its
    /// slot, which grows its half's drop epoch as `forget_range_where`
    /// would (`LookupTable::set`), and its record goes: a drop costs one
    /// slot, not a search of every set its granule lists.
    fn verify_records(&mut self, number: u64, variant: Variant) {
        let controls = self.cpu.controls(self.cpu.regime());
        let requester = self.cpu.requester();
        let mut confirmed = Confirmed::new();
        for (page, at) in self.tlbi_keep.pages(number) {
            let record = self.tlbi_keep.store[at as usize];
            // The slot the entry is in: its own or its neighbour
            // (`LookupTable::find`).
            let slot = self.itlb.page_slot(page, number);
            let held_at = [slot, slot ^ 1].into_iter().find(|&at| {
                let held = self.itlb.entry(at);
                held.pc == page && held.key == number
            });
            // An entry the ITLB evicted: nothing to keep.
            let Some(held_at) = held_at else {
                self.tlbi_keep.take(number, page);
                continue;
            };
            let held = self.itlb.entry(held_at);
            #[cfg(test)]
            if self.tlbi_keep.unverified {
                continue;
            }
            let mut kept =
                self.kept_walk_holds(page, held, &record, &controls, variant, &mut confirmed);
            if kept && self.tlbi_keep.check {
                kept = self.check_kept(page, held, &record, &controls, requester, variant);
            }
            if kept {
                self.statistics.tlbi_keep.kept += 1;
            } else {
                self.itlb.set(held_at, chain::LookupEntry::default());
                self.tlbi_keep.take(number, page);
                self.statistics.tlbi_keep.dropped += 1;
                self.refills.push(page);
            }
        }
    }

    /// Whether the entry `held` of the page `page`, filled from `record`, is
    /// what a walk under `controls` gives now (the module documentation's
    /// conditions); one that is goes in the filters, as a walk's fetch does
    /// (`fetched_through`). `confirmed` caches this verification's reads.
    fn kept_walk_holds(
        &mut self,
        page: u64,
        held: chain::LookupEntry,
        record: &WalkRecord,
        controls: &mmu::Controls,
        variant: Variant,
        confirmed: &mut Confirmed,
    ) -> bool {
        let half = chain::half(page);
        if !record.covers(page)
            || half == 0 && record.ttbr0 != controls.ttbr0
            || !self.reads_hold(record, Some(confirmed))
            || !self.host.fetch_permitted(page)
        {
            return false;
        }
        let physical = record.physical.wrapping_add(page).wrapping_sub(record.page);
        if self.kept_entry(
            page,
            held.key,
            physical,
            record.guarded,
            record.global,
            variant,
            confirmed,
        ) != Some(held)
        {
            return false;
        }
        // A wider span may have let a broadcast pass the PE by while it read:
        // as a fetch walks again (`fetch_physical`), it reads again.
        if half == 0
            && record.global
            && self.widen_lower_globals(page, record.size)
            && !self.reads_hold(record, None)
        {
            return false;
        }
        // The upper half's first non-global fetch forgets its contexts;
        // a kept one has none past the forget (`keep_spaces`).
        if half == 1 && !record.global && !self.private_fetches[1] {
            return false;
        }
        self.fetched_through(half, page, record.size, record.global);
        true
    }

    /// Whether every descriptor `record` read holds its value still, reading
    /// through `confirmed` where given.
    fn reads_hold(&mut self, record: &WalkRecord, mut confirmed: Option<&mut Confirmed>) -> bool {
        record.reads().iter().all(|&(address, value)| {
            let now = match confirmed.as_deref_mut() {
                Some(cache) => {
                    let slot = &mut cache.0[Confirmed::slot(address)];
                    if slot.0 != address {
                        *slot = (address, self.host.read_ram_descriptor(address));
                    }
                    slot.1
                }
                None => self.host.read_ram_descriptor(address),
            };
            now == Some(value)
        })
    }

    /// The ITLB entry a fill in `context` would make of the page `page`
    /// translating to the frame of `physical`, `guarded` and `global`, for a
    /// fetch in `variant`: what [`Self::take_page`] holds of such a walk.
    /// `confirmed` caches the variants' numbers.
    #[allow(clippy::too_many_arguments)]
    fn kept_entry(
        &self,
        page: u64,
        context: u64,
        physical: u64,
        guarded: bool,
        global: bool,
        variant: Variant,
        confirmed: &mut Confirmed,
    ) -> Option<chain::LookupEntry> {
        let current = Variant {
            guarded_page: guarded,
            ..variant
        };
        let frame = physical & !(FETCH_PAGE - 1);
        let id = confirmed.variant(self, self.page_variant(current, frame))?;
        let key = self.block_key(frame, id);
        Some(match global {
            true => chain::LookupEntry::global_page(page, context, key),
            false => chain::LookupEntry::page(page, context, key),
        })
    }

    /// `--tlbi-verify-check`: whether a full walk of `page` under
    /// `controls` gives what verification kept of it (`held`, from
    /// `record`); a mismatch is counted, sampled and dropped.
    fn check_kept(
        &mut self,
        page: u64,
        held: chain::LookupEntry,
        record: &WalkRecord,
        controls: &mmu::Controls,
        requester: mmu::Requester,
        variant: Variant,
    ) -> bool {
        self.statistics.tlbi_keep.checked += 1;
        let walked = match mmu::translate(
            controls,
            page,
            Access::Execute,
            requester,
            &mut RamTables(&mut self.host),
        ) {
            Translated::Mapped(walked) if self.host.fetch_permitted(page) => Some(walked),
            _ => None,
        };
        let matches = walked.is_some_and(|walked| {
            walked.size == record.size
                && walked.global == record.global
                && walked.guarded == record.guarded
                && self.kept_entry(
                    page,
                    held.key,
                    walked.physical,
                    walked.guarded,
                    walked.global,
                    variant,
                    // Its own cache: the check shares nothing with what it
                    // checks.
                    &mut Confirmed::new(),
                ) == Some(held)
        });
        if !matches {
            let frame = walked.map_or(u64::MAX, |walked| walked.physical & !(FETCH_PAGE - 1));
            self.statistics
                .tlbi_keep
                .mismatch([page, held.key, held.entry, frame]);
        }
        matches
    }
}
