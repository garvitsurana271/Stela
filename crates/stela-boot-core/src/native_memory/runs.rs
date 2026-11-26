//! Linux page runs from arenas: blocks of [`ARENA_BYTES`] the heap gives
//! whole, split by a buddy allocator into runs of up to [`MAX_RUN_ORDER`],
//! each aligned to its size, with no header between them.
//!
//! Linux's page allocator returns `1 << order` pages aligned to their size,
//! and its catalogue checks that. From the heap itself every such run is a
//! block whose boundary tag sits just before the aligned payload, so two runs
//! of one size can never lie side by side: each strands almost its own size
//! before the next. An integrated GPU's TTM fills its buffers with 4 MiB runs
//! first, and on a ThinkPad T14 Gen 2 (Cezanne) 339 of them (1356 MiB) left
//! a 6400 MiB heap with 1.7 GiB free in no extent larger than 6 MiB, 270 s
//! into macOS's setup, when a 4 MiB allocation of the Metal device failed.
//! In an arena the runs tile, and a freed run joins its buddy again.
//!
//! An arena the heap cannot give falls back to the heap's own block, as
//! before; an arena whose runs are all free goes back to the heap, but for
//! one kept for the next.
use alloc::vec::Vec;
use core::{
    cell::UnsafeCell,
    ptr::NonNull,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

/// Pages in an arena: 32 MiB of 4 KiB pages.
pub const ARENA_ORDER: u32 = 13;
pub const ARENA_PAGES: usize = 1 << ARENA_ORDER;
/// Linux's `MAX_PAGE_ORDER`: runs of up to 4 MiB, and the arenas' alignment.
pub const MAX_RUN_ORDER: u32 = 10;
const LEVELS: usize = ARENA_ORDER as usize + 1;

/// The free runs of one arena, by order: a bit per run of each order, set
/// when that run is free and its buddy is not (they would have joined).
pub(crate) struct Buddy {
    bits: Vec<u64>,
    /// Each order's first word in `bits`.
    offsets: [usize; LEVELS],
    /// Each order's free runs.
    free: [usize; LEVELS],
}

impl Buddy {
    /// A wholly free arena, or `None` when its bitmaps cannot be allocated.
    pub(crate) fn new() -> Option<Self> {
        let mut offsets = [0; LEVELS];
        let mut words = 0;
        for (order, offset) in offsets.iter_mut().enumerate() {
            *offset = words;
            words += (ARENA_PAGES >> order).div_ceil(64);
        }
        let mut bits = Vec::new();
        bits.try_reserve_exact(words).ok()?;
        bits.resize(words, 0);
        let mut buddy = Self {
            bits,
            offsets,
            free: [0; LEVELS],
        };
        buddy.set(ARENA_ORDER as usize, 0, true);
        Some(buddy)
    }

    fn get(&self, order: usize, index: usize) -> bool {
        self.bits[self.offsets[order] + index / 64] & (1 << (index % 64)) != 0
    }

    fn set(&mut self, order: usize, index: usize, free: bool) {
        let word = &mut self.bits[self.offsets[order] + index / 64];
        let bit = 1 << (index % 64);
        if free {
            debug_assert!(*word & bit == 0);
            *word |= bit;
            self.free[order] += 1;
        } else {
            debug_assert!(*word & bit != 0);
            *word &= !bit;
            self.free[order] -= 1;
        }
    }

    fn first(&self, order: usize) -> Option<usize> {
        let words = (ARENA_PAGES >> order).div_ceil(64);
        let start = self.offsets[order];
        self.bits[start..start + words]
            .iter()
            .position(|word| *word != 0)
            .map(|word| word * 64 + self.bits[start + word].trailing_zeros() as usize)
    }

    /// Take a free run of `order`, splitting the smallest larger one when
    /// none is free: its first page.
    pub(crate) fn allocate(&mut self, order: u32) -> Option<usize> {
        let order = order as usize;
        let from = (order..LEVELS).find(|level| self.free[*level] != 0)?;
        let mut index = self.first(from)?;
        self.set(from, index, false);
        for level in (order..from).rev() {
            index *= 2;
            self.set(level, index + 1, true);
        }
        Some(index << order)
    }

    /// Free the run of `order` at `page`, joining it with its free buddies.
    pub(crate) fn free(&mut self, page: usize, order: u32) {
        let mut order = order as usize;
        let mut index = page >> order;
        debug_assert_eq!(index << order, page);
        while order < ARENA_ORDER as usize && self.get(order, index ^ 1) {
            self.set(order, index ^ 1, false);
            index /= 2;
            order += 1;
        }
        self.set(order, index, true);
    }

    /// Whether every page is free.
    pub(crate) fn empty(&self) -> bool {
        self.free[ARENA_ORDER as usize] == 1
    }

    /// Free pages.
    #[cfg(test)]
    pub(crate) fn free_pages(&self) -> usize {
        self.free
            .iter()
            .enumerate()
            .map(|(order, count)| count << order)
            .sum()
    }
}

pub(crate) struct Arena {
    pub(crate) base: NonNull<u8>,
    pub(crate) buddy: Buddy,
}

/// The arenas, behind a spin lock: Linux page requests come in process or
/// deferred context only (atomic ones are refused), and none nests.
pub(crate) struct Arenas {
    held: AtomicBool,
    arenas: UnsafeCell<Vec<Arena>>,
    /// Arenas, and the bytes in runs, for the heap report.
    pub(crate) count: AtomicUsize,
    pub(crate) used: AtomicUsize,
    /// Runs the arenas could not serve, which came from the heap.
    pub(crate) fallbacks: AtomicUsize,
}

// SAFETY: `arenas` is touched only under `held`.
unsafe impl Sync for Arenas {}

pub(crate) struct Locked<'a> {
    owner: &'a Arenas,
}

impl Drop for Locked<'_> {
    fn drop(&mut self) {
        self.owner.held.store(false, Ordering::Release);
    }
}

impl Locked<'_> {
    pub(crate) fn arenas(&mut self) -> &mut Vec<Arena> {
        // SAFETY: this guard holds the lock.
        unsafe { &mut *self.owner.arenas.get() }
    }
}

impl Arenas {
    pub(crate) const fn new() -> Self {
        Self {
            held: AtomicBool::new(false),
            arenas: UnsafeCell::new(Vec::new()),
            count: AtomicUsize::new(0),
            used: AtomicUsize::new(0),
            fallbacks: AtomicUsize::new(0),
        }
    }

    pub(crate) fn lock(&self) -> Locked<'_> {
        while self
            .held
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
        Locked { owner: self }
    }

    /// Adds `arena`, growing the list outside the lock: false when no room
    /// for it could be allocated.
    pub(crate) fn push(&self, arena: Arena) -> bool {
        loop {
            let wanted = {
                let mut locked = self.lock();
                let arenas = locked.arenas();
                if arenas.len() < arenas.capacity() {
                    arenas.push(arena);
                    return true;
                }
                arenas.capacity() * 2 + 4
            };
            let mut bigger = Vec::new();
            if bigger.try_reserve_exact(wanted).is_err() {
                return false;
            }
            let old = {
                let mut locked = self.lock();
                let arenas = locked.arenas();
                if arenas.capacity() >= bigger.capacity() {
                    // Another request grew it meanwhile.
                    None
                } else {
                    bigger.append(arenas);
                    Some(core::mem::replace(arenas, bigger))
                }
            };
            // The smaller list (or the unused one) is freed outside the lock.
            drop(old);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_of_one_order_tile_an_arena_and_join_again_when_freed() {
        let mut buddy = Buddy::new().unwrap();
        let order = MAX_RUN_ORDER;
        let runs: Vec<usize> = (0..ARENA_PAGES >> order)
            .map(|_| buddy.allocate(order).unwrap())
            .collect();
        // Every run aligned to its size, side by side.
        for (index, page) in runs.iter().enumerate() {
            assert_eq!(*page, index << order);
        }
        assert_eq!(buddy.allocate(0), None);
        assert_eq!(buddy.free_pages(), 0);
        for page in runs.iter().rev() {
            buddy.free(*page, order);
        }
        assert!(buddy.empty());
        assert_eq!(buddy.free_pages(), ARENA_PAGES);
    }

    #[test]
    fn four_mib_runs_tile_their_arenas_and_the_arenas_go_back_to_the_heap() {
        use crate::{
            heap::Heap,
            native_memory::{NativeMemory, PAGE},
        };
        use core::alloc::Layout;
        static HEAP: Heap = Heap::new();
        static SOURCE: NativeMemory = NativeMemory::new(&HEAP, |_, _, _, _| Ok(()));
        let arena = ARENA_PAGES * PAGE;
        let bytes = 3 * arena + (16 << 20);
        let layout = Layout::from_size_align(bytes, 4 << 20).unwrap();
        // SAFETY: test RAM kept for the whole test; its PA relation is a model.
        let ram = unsafe { std::alloc::alloc_zeroed(layout) };
        assert!(!ram.is_null());
        // SAFETY: one dedicated extent, no users before initialization.
        unsafe {
            HEAP.initialize(ram, bytes);
            SOURCE.initialize(ram as usize, 0x1_0000_0000, bytes, 0);
        }
        let runs_per_arena = ARENA_PAGES >> MAX_RUN_ORDER;
        let mut runs = Vec::new();
        for _ in 0..3 * runs_per_arena {
            runs.push(SOURCE.allocate_run(MAX_RUN_ORDER).unwrap());
        }
        let usage = SOURCE.run_usage();
        assert_eq!((usage.arenas, usage.fallbacks), (3, 0));
        assert_eq!(usage.used, 3 * arena);
        // The heap holds the three arenas and their bitmaps, no gap per run.
        assert!(
            HEAP.usage().live < 3 * arena + (64 << 10),
            "{}",
            HEAP.usage().live
        );
        let mut bases: Vec<usize> = runs
            .iter()
            .map(|run| {
                let mapping = run
                    .cpu_mapping(stela_drivers::bus::dma::memory::CacheAttribute::WriteBack)
                    .unwrap();
                mapping.address(0, 1).unwrap().as_ptr() as usize
            })
            .collect();
        bases.sort();
        for base in &bases {
            assert_eq!(base % (PAGE << MAX_RUN_ORDER), 0);
            let offset = (base - ram as usize) as u64;
            assert_eq!((0x1_0000_0000 + offset) % (PAGE << MAX_RUN_ORDER) as u64, 0);
        }
        bases.dedup();
        assert_eq!(bases.len(), runs.len());
        // The heap has no room for a fourth arena: a run comes from the heap.
        let fallback = SOURCE.allocate_run(0).unwrap();
        assert_eq!(SOURCE.run_usage().fallbacks, 1);
        drop(fallback);
        runs.clear();
        // One wholly free arena is kept, the rest go back.
        let usage = SOURCE.run_usage();
        assert_eq!((usage.arenas, usage.used), (1, 0));
        let small = SOURCE.allocate_run(0).unwrap();
        assert_eq!(SOURCE.run_usage().arenas, 1);
        drop(small);
    }

    #[test]
    fn mixed_orders_split_the_smallest_run_and_rejoin_in_any_order() {
        let mut buddy = Buddy::new().unwrap();
        let mut live = Vec::new();
        let mut seed = 0x2545_f491_u64;
        let mut taken = 0;
        for _ in 0..4000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            if live.is_empty() || !seed.is_multiple_of(3) {
                let order = (seed >> 8) as u32 % (MAX_RUN_ORDER + 1);
                if let Some(page) = buddy.allocate(order) {
                    assert_eq!(page % (1 << order), 0);
                    // No overlap with a live run.
                    for (other, other_order) in &live {
                        let (a, b) = (page, page + (1 << order));
                        let (c, d) = (*other, *other + (1usize << *other_order));
                        assert!(b <= c || d <= a);
                    }
                    taken += 1 << order;
                    live.push((page, order));
                }
            } else {
                let (page, order) = live.swap_remove((seed >> 20) as usize % live.len());
                taken -= 1 << order;
                buddy.free(page, order);
            }
            assert_eq!(buddy.free_pages(), ARENA_PAGES - taken);
        }
        for (page, order) in live.drain(..) {
            buddy.free(page, order);
        }
        assert!(buddy.empty());
    }
}
