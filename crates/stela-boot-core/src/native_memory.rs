//! Native system-RAM pins backed by the mount's existing reusable heap.
//!
//! Allocation and the final release of any pin/CPU/DMA mapping must occur in
//! process or deferred-work context. Raw heap exclusion is maskable-IRQ safe;
//! that does not check pin/cache checks or mapping lifetime transitions in
//! an ISR. This service is not a VRAM allocator or an IOMMU.
use crate::heap::Heap;
use alloc::{boxed::Box, vec::Vec};
use core::{
    alloc::{GlobalAlloc, Layout},
    ptr::NonNull,
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
};
use stela_drivers::bus::dma::memory::{
    CacheAttribute, MemoryError, PageBacking, PageGeometry, PageSource, PinnedPages,
};

const PAGE_SHIFT: u8 = 12;
pub const PAGE: usize = 1 << PAGE_SHIFT;

pub mod cache;
#[path = "native_memory/runs.rs"]
pub mod runs;

/// Checks that `bytes` at CPU address `cpu` map physical `physical` with an
/// effective write-back type, reading page tables at `table_offset` past
/// their physical addresses. The mount reads CR3, PAT and the MTRRs.
pub type CacheCheck =
    fn(cpu: u64, physical: u64, bytes: usize, table_offset: u64) -> Result<(), MemoryError>;

pub struct NativeMemory {
    heap: &'static Heap,
    cache: CacheCheck,
    cpu: AtomicUsize,
    physical: AtomicU64,
    bytes: AtomicUsize,
    table_offset: AtomicU64,
    generation: AtomicU64,
    catalogs: AtomicUsize,
    runs: runs::Arenas,
}

/// The Linux page arenas' state, for the heap report.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RunUsage {
    pub arenas: usize,
    pub arena_bytes: usize,
    pub used: usize,
    pub fallbacks: usize,
}

/// The registered heap extent, as `initialize` recorded it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Registration {
    /// Zero until the first `initialize`; each registration increments it.
    pub generation: u64,
    pub cpu: usize,
    pub physical: u64,
    pub bytes: usize,
}

impl NativeMemory {
    /// Pages from `heap`, each allocation checked by `cache`.
    pub const fn new(heap: &'static Heap, cache: CacheCheck) -> Self {
        Self {
            heap,
            cache,
            cpu: AtomicUsize::new(0),
            physical: AtomicU64::new(0),
            bytes: AtomicUsize::new(0),
            table_offset: AtomicU64::new(0),
            generation: AtomicU64::new(0),
            catalogs: AtomicUsize::new(0),
            runs: runs::Arenas::new(),
        }
    }

    /// Register the heap's CPU/physical mapping once in each phase.
    ///
    /// # Safety
    /// The heap has just been initialized over exactly this firmware-reserved,
    /// exclusively owned, page-aligned RAM extent, excluded from every guest
    /// and free-memory inventory. Its writable WB mapping and all aliases stay
    /// valid and compatible across every root change until the last release.
    /// No allocation from a previous registration is alive and nothing uses
    /// this one concurrently; every page catalog has been retired. Only
    /// process or deferred-work context allocates or does the last release,
    /// including an `Arc` or lease drop. Adding `table_offset` to each
    /// CR3-reachable table's physical address gives a readable mapping of that
    /// page (zero for identity, the physmap offset in the engine). Root,
    /// migration and cache-policy changes are excluded during allocation.
    pub unsafe fn initialize(&self, cpu: usize, physical: u64, bytes: usize, table_offset: u64) {
        assert_eq!(self.catalogs.load(Ordering::Acquire), 0);
        let generation = self
            .generation
            .load(Ordering::Relaxed)
            .checked_add(1)
            .expect("native memory generation exhausted");
        assert!(cpu != 0 && cpu.is_multiple_of(PAGE));
        assert!(physical.is_multiple_of(PAGE as u64));
        assert!(bytes != 0 && bytes.is_multiple_of(PAGE));
        assert!(cpu.checked_add(bytes).is_some());
        assert!(physical.checked_add(bytes as u64).is_some());
        self.cpu.store(cpu, Ordering::Relaxed);
        self.physical.store(physical, Ordering::Relaxed);
        self.table_offset.store(table_offset, Ordering::Relaxed);
        self.bytes.store(bytes, Ordering::Release);
        self.generation.store(generation, Ordering::Release);
    }

    pub fn registration(&self) -> Registration {
        let generation = self.generation.load(Ordering::Acquire);
        Registration {
            generation,
            cpu: self.cpu.load(Ordering::Relaxed),
            physical: self.physical.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Acquire),
        }
    }

    pub fn table_offset(&self) -> u64 {
        self.table_offset.load(Ordering::Acquire)
    }

    /// Count a page catalog that refers to this registration, which
    /// `initialize` refuses to retarget until `release_catalog`.
    pub fn retain_catalog(&self) -> Result<(), MemoryError> {
        self.catalogs
            .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                count.checked_add(1)
            })
            .map(drop)
            .map_err(|_| MemoryError::OutOfMemory)
    }

    pub fn release_catalog(&self) {
        let previous = self.catalogs.fetch_sub(1, Ordering::AcqRel);
        assert!(previous != 0);
    }
}

impl NativeMemory {
    /// Exercise the registered mapping and allocator: refusals, page lists,
    /// zeroed tails and exact reclamation. Called by the mount's host check.
    pub fn verify(&self) -> Result<(), MemoryError> {
        let before = self.heap.usage().live;
        if !matches!(
            self.allocate(PAGE, PAGE, CacheAttribute::WriteCombining),
            Err(MemoryError::CacheAttributeUnavailable)
        ) || !matches!(
            self.allocate(PAGE, 3, CacheAttribute::WriteBack),
            Err(MemoryError::InvalidAlignment)
        ) {
            return Err(MemoryError::CacheAttributeUnavailable);
        }
        let pages = self.allocate(PAGE + 1, PAGE, CacheAttribute::WriteBack)?;
        if pages.logical_bytes() != PAGE + 1 || pages.bytes() != 2 * PAGE {
            return Err(MemoryError::InvalidSize);
        }
        let mapping = pages.cpu_mapping(CacheAttribute::WriteBack)?;
        if !matches!(
            pages.cpu_mapping(CacheAttribute::Uncached),
            Err(MemoryError::CacheAttributeUnavailable)
        ) || mapping.address(usize::MAX, 1) != Err(MemoryError::InvalidSize)
            || mapping.address(mapping.bytes(), 1) != Err(MemoryError::InvalidSize)
        {
            return Err(MemoryError::InvalidSize);
        }
        let pointer = mapping.address(0, mapping.bytes())?.as_ptr();
        let offset = (pointer as usize)
            .checked_sub(self.cpu.load(Ordering::Relaxed))
            .ok_or(MemoryError::OutsideReservation)?;
        let physical = self.physical.load(Ordering::Relaxed) + offset as u64;
        if pages.physical_pages() != [physical, physical + PAGE as u64] {
            return Err(MemoryError::InvalidPageList);
        }
        drop(pages);
        for index in 0..mapping.bytes() {
            // SAFETY: CPU-exclusive owned RAM with the mapping live.
            if unsafe { pointer.add(index).read_volatile() } != 0 {
                return Err(MemoryError::InvalidPageList);
            }
        }
        // SAFETY: this exclusive CPU mapping includes its full last-page tail.
        unsafe {
            pointer.add(2 * PAGE - 1).write_volatile(0x5a);
            if pointer.add(2 * PAGE - 1).read_volatile() != 0x5a {
                return Err(MemoryError::OutsideReservation);
            }
        }
        drop(mapping);
        if self.heap.usage().live != before {
            return Err(MemoryError::OutsideReservation);
        }
        Ok(())
    }
}

impl PageSource for NativeMemory {
    fn page_geometry(&self) -> Result<PageGeometry, MemoryError> {
        if self.bytes.load(Ordering::Acquire) == 0 {
            return Err(MemoryError::Unavailable);
        }
        // This allocator records PAGE_SHIFT for every real returned backing.
        // A large byte capacity/alignment does not establish a larger granule.
        PageGeometry::base_only(PAGE_SHIFT)
    }

    fn allocate(
        &self,
        bytes: usize,
        alignment: usize,
        attribute: CacheAttribute,
    ) -> Result<PinnedPages, MemoryError> {
        self.allocate_native(bytes, alignment, attribute, None)
    }

    fn allocate_below(
        &self,
        bytes: usize,
        alignment: usize,
        attribute: CacheAttribute,
        physical_end_exclusive: u64,
    ) -> Result<PinnedPages, MemoryError> {
        self.allocate_native(bytes, alignment, attribute, Some(physical_end_exclusive))
    }
}

impl NativeMemory {
    /// The Linux page arenas' state.
    pub fn run_usage(&self) -> RunUsage {
        let arenas = self.runs.count.load(Ordering::Relaxed);
        RunUsage {
            arenas,
            arena_bytes: arenas * runs::ARENA_PAGES * PAGE,
            used: self.runs.used.load(Ordering::Relaxed),
            fallbacks: self.runs.fallbacks.load(Ordering::Relaxed),
        }
    }

    /// A zeroed write-back run of `1 << order` pages aligned to its size, as
    /// Linux's page allocator gives one: from an arena ([`runs`]), or from
    /// the heap itself when no arena can be had.
    pub fn allocate_run(&'static self, order: u32) -> Result<PinnedPages, MemoryError> {
        if order > runs::MAX_RUN_ORDER {
            return Err(MemoryError::InvalidSize);
        }
        let bytes = PAGE << order;
        match self.arena_run(order) {
            Ok(Some(pages)) => Ok(pages),
            Ok(None) => {
                self.runs.fallbacks.fetch_add(1, Ordering::Relaxed);
                self.allocate(bytes, bytes, CacheAttribute::WriteBack)
            }
            Err(error) => Err(error),
        }
    }

    /// A run from an arena, a new one when none has room; `None` when the
    /// registration's CPU and physical bases differ in their arena residue
    /// or the heap has no arena to give.
    fn arena_run(&'static self, order: u32) -> Result<Option<PinnedPages>, MemoryError> {
        let capacity = self.bytes.load(Ordering::Acquire);
        if capacity == 0 {
            return Err(MemoryError::Unavailable);
        }
        let cpu = self.cpu.load(Ordering::Relaxed);
        let physical_base = self.physical.load(Ordering::Relaxed);
        let arena_align = PAGE << runs::MAX_RUN_ORDER;
        if ((cpu as u64) ^ physical_base) & (arena_align as u64 - 1) != 0 {
            return Ok(None);
        }
        let arena_bytes = runs::ARENA_PAGES * PAGE;
        let bytes = PAGE << order;
        // Nothing is allocated under the arenas' spin lock: an allocation may
        // end a GPU job's slice there (the heap's allocation hook), and every
        // other processor asking Linux for a page would spin on the lock until
        // the job ran again.
        let found = {
            let mut locked = self.runs.lock();
            locked
                .arenas()
                .iter_mut()
                .find_map(|arena| Some((arena.base, arena.buddy.allocate(order)?)))
        };
        let (base, page) = match found {
            Some(found) => found,
            None => {
                let Some(mut buddy) = runs::Buddy::new() else {
                    return Ok(None);
                };
                let layout = Layout::from_size_align(arena_bytes, arena_align)
                    .map_err(|_| MemoryError::InvalidAlignment)?;
                // SAFETY: a valid nonzero layout; the arena owns the block
                // until it is wholly free and given back with it.
                let Some(base) = NonNull::new(unsafe { self.heap.alloc(layout) }) else {
                    return Ok(None);
                };
                let in_heap = (base.as_ptr() as usize)
                    .checked_sub(cpu)
                    .is_some_and(|offset| offset + arena_bytes <= capacity);
                let page = buddy.allocate(order);
                let (true, Some(page)) = (in_heap, page) else {
                    // SAFETY: the block just allocated with this layout.
                    unsafe { self.heap.dealloc(base.as_ptr(), layout) };
                    return if in_heap {
                        Err(MemoryError::OutOfMemory)
                    } else {
                        Err(MemoryError::OutsideReservation)
                    };
                };
                if !self.runs.push(runs::Arena { base, buddy }) {
                    // SAFETY: as above; no run of it was handed out.
                    unsafe { self.heap.dealloc(base.as_ptr(), layout) };
                    return Ok(None);
                }
                self.runs.count.fetch_add(1, Ordering::Relaxed);
                (base, page)
            }
        };
        let mut run = RunAllocation {
            source: self,
            arena: base,
            page,
            order,
            pages: Vec::new(),
        };
        self.runs.used.fetch_add(bytes, Ordering::Relaxed);
        let start = run.cpu_base().as_ptr() as usize;
        let physical = physical_base + (start - cpu) as u64;
        (self.cache)(
            start as u64,
            physical,
            bytes,
            self.table_offset.load(Ordering::Relaxed),
        )?;
        run.pages
            .try_reserve_exact(bytes / PAGE)
            .map_err(|_| MemoryError::OutOfMemory)?;
        run.pages
            .extend((0..bytes / PAGE).map(|index| physical + (index * PAGE) as u64));
        // SAFETY: the run's pages, exclusively owned until it drops.
        unsafe { core::ptr::write_bytes(start as *mut u8, 0, bytes) };
        // SAFETY: a valid nonzero layout for the run's owner.
        let storage = NonNull::new(unsafe { alloc::alloc::alloc(Layout::new::<RunAllocation>()) })
            .ok_or(MemoryError::OutOfMemory)?
            .cast::<RunAllocation>();
        // SAFETY: fresh aligned storage, initialized once; the Box owns it.
        let run = unsafe {
            storage.as_ptr().write(run);
            Box::from_raw(storage.as_ptr())
        };
        PinnedPages::new(run, bytes, bytes).map(Some)
    }

    /// Give a run back to its arena, and the arena to the heap when it is
    /// wholly free and another wholly free arena is kept.
    fn free_run(&self, arena: NonNull<u8>, page: usize, order: u32) {
        self.runs.used.fetch_sub(PAGE << order, Ordering::Relaxed);
        let released = {
            let mut locked = self.runs.lock();
            let arenas = locked.arenas();
            let Some(index) = arenas.iter().position(|candidate| candidate.base == arena) else {
                panic!("a Linux page run outside every arena");
            };
            arenas[index].buddy.free(page, order);
            let spare = arenas
                .iter()
                .enumerate()
                .any(|(other, candidate)| other != index && candidate.buddy.empty());
            if arenas[index].buddy.empty() && spare {
                self.runs.count.fetch_sub(1, Ordering::Relaxed);
                Some(arenas.swap_remove(index))
            } else {
                None
            }
        };
        if let Some(arena) = released {
            let layout =
                Layout::from_size_align(runs::ARENA_PAGES * PAGE, PAGE << runs::MAX_RUN_ORDER)
                    .expect("the arena layout");
            // SAFETY: the arena's own block and layout, with no run left.
            unsafe { self.heap.dealloc(arena.base.as_ptr(), layout) };
        }
    }

    fn allocate_native(
        &self,
        bytes: usize,
        alignment: usize,
        attribute: CacheAttribute,
        physical_end_exclusive: Option<u64>,
    ) -> Result<PinnedPages, MemoryError> {
        if attribute != CacheAttribute::WriteBack {
            return Err(MemoryError::CacheAttributeUnavailable);
        }
        if !alignment.is_power_of_two() {
            return Err(MemoryError::InvalidAlignment);
        }
        if bytes == 0 {
            return Err(MemoryError::InvalidSize);
        }
        let capacity = self.bytes.load(Ordering::Acquire);
        if capacity == 0 {
            return Err(MemoryError::Unavailable);
        }
        let logical_bytes = bytes;
        let bytes = bytes
            .checked_next_multiple_of(PAGE)
            .ok_or(MemoryError::InvalidSize)?;
        let layout = Layout::from_size_align(bytes, alignment.max(PAGE))
            .map_err(|_| MemoryError::InvalidAlignment)?;
        let cpu = self.cpu.load(Ordering::Relaxed);
        let physical_base = self.physical.load(Ordering::Relaxed);
        // A linear heap cannot satisfy simultaneous CPU/PA alignment when
        // their registered bases have incompatible residues. Refuse it.
        if ((cpu as u64) ^ physical_base) & (layout.align() as u64 - 1) != 0 {
            return Err(MemoryError::InvalidAlignment);
        }
        // The physical ceiling is translated through this ACTUAL kept
        // linear mapping, not pointer identity. Keep only complete payloads;
        // the rounded native tail participates even for a one-byte request.
        let allowed = if let Some(end) = physical_end_exclusive {
            let available = end.saturating_sub(physical_base).min(capacity as u64);
            if available < bytes as u64 {
                return Err(MemoryError::OutOfMemory);
            }
            let available = usize::try_from(available).map_err(|_| MemoryError::InvalidSize)?;
            Some(
                cpu..cpu
                    .checked_add(available)
                    .ok_or(MemoryError::OutsideReservation)?,
            )
        } else {
            None
        };
        // SAFETY: initialize registered the same live heap and checked CPU/PA
        // relation. Pin/cache work remains process/deferred context. Both paths
        // use the canonical allocator and publish only zeroed exclusive pages.
        let pointer = NonNull::new(unsafe {
            if let Some(allowed) = allowed {
                self.heap.alloc_pages_zeroed_in(layout, allowed)
            } else {
                self.heap.alloc_zeroed(layout)
            }
        })
        .ok_or(MemoryError::OutOfMemory)?;
        // Own the payload before any validation or metadata allocation can fail.
        let mut backing = HeapAllocation {
            heap: self.heap,
            pointer,
            layout,
            pages: Vec::new(),
        };
        let start = pointer.as_ptr() as usize;
        let offset = start
            .checked_sub(cpu)
            .filter(|offset| offset.checked_add(bytes).is_some_and(|end| end <= capacity));
        let Some(offset) = offset else {
            return Err(MemoryError::OutsideReservation);
        };
        let physical = physical_base
            .checked_add(offset as u64)
            .ok_or(MemoryError::InvalidPageList)?;
        let physical_end = physical
            .checked_add(bytes as u64)
            .ok_or(MemoryError::InvalidPageList)?;
        if physical_end_exclusive.is_some_and(|end| physical_end > end) {
            return Err(MemoryError::OutsideReservation);
        }
        (self.cache)(
            start as u64,
            physical,
            bytes,
            self.table_offset.load(Ordering::Relaxed),
        )?;
        backing
            .pages
            .try_reserve_exact(bytes / PAGE)
            .map_err(|_| MemoryError::OutOfMemory)?;
        backing
            .pages
            .extend((0..bytes / PAGE).map(|index| physical + (index * PAGE) as u64));
        // The stable Box constructor aborts on OOM. Reserve its exact nonzero
        // storage fallibly before transferring this unique payload owner.
        // SAFETY: a valid nonzero layout for a fully initialized backing.
        let storage = NonNull::new(unsafe { alloc::alloc::alloc(Layout::new::<HeapAllocation>()) })
            .ok_or(MemoryError::OutOfMemory)?
            .cast::<HeapAllocation>();
        // SAFETY: fresh aligned global allocation, initialized once. Box takes
        // that exact allocation/layout and owns the backing on every later path.
        let backing = unsafe {
            storage.as_ptr().write(backing);
            Box::from_raw(storage.as_ptr())
        };
        PinnedPages::new(backing, logical_bytes, layout.align())
    }
}

struct HeapAllocation {
    heap: &'static Heap,
    pointer: NonNull<u8>,
    layout: Layout,
    pages: Vec<u64>,
}

// SAFETY: the pointer is an owned, immovable allocation. No safe CPU data
// references are exposed. Consumers synchronize raw CPU/device accesses and
// never do final release in hard IRQ context; metadata is immutable.
unsafe impl Send for HeapAllocation {}
// SAFETY: shared access exposes immutable metadata and raw pointers only.
unsafe impl Sync for HeapAllocation {}

// SAFETY: initialize proves the native heap's contiguous PA mapping;
// allocate keeps a full aligned/zeroed extent and records every real page.
// Drop returns it to the same allocator only after the last Arc-backed lease.
unsafe impl PageBacking for HeapAllocation {
    fn cpu_base(&self) -> NonNull<u8> {
        self.pointer
    }

    fn physical_pages(&self) -> &[u64] {
        &self.pages
    }

    fn page_shift(&self) -> u8 {
        PAGE_SHIFT
    }

    fn cache_attribute(&self) -> CacheAttribute {
        CacheAttribute::WriteBack
    }
}

/// A run of Linux pages in an arena ([`NativeMemory::allocate_run`]).
struct RunAllocation {
    source: &'static NativeMemory,
    arena: NonNull<u8>,
    page: usize,
    order: u32,
    pages: Vec<u64>,
}

// SAFETY: as HeapAllocation's: an owned, immovable run; the arenas' lock
// orders its release.
unsafe impl Send for RunAllocation {}
// SAFETY: shared access exposes immutable metadata and raw pointers only.
unsafe impl Sync for RunAllocation {}

// SAFETY: the arena lies in the registered heap, whose PA relation
// initialize proved; the run is aligned to its size, zeroed, and every page
// recorded. Drop returns it only after the last lease.
unsafe impl PageBacking for RunAllocation {
    fn cpu_base(&self) -> NonNull<u8> {
        // SAFETY: the run lies inside its arena's block.
        unsafe { self.arena.add(self.page * PAGE) }
    }

    fn physical_pages(&self) -> &[u64] {
        &self.pages
    }

    fn page_shift(&self) -> u8 {
        PAGE_SHIFT
    }

    fn cache_attribute(&self) -> CacheAttribute {
        CacheAttribute::WriteBack
    }
}

impl Drop for RunAllocation {
    fn drop(&mut self) {
        self.source.free_run(self.arena, self.page, self.order);
    }
}

impl Drop for HeapAllocation {
    fn drop(&mut self) {
        // SAFETY: this owner is released only after every pin/mapping ends,
        // using the exact allocation and layout, in process/deferred context.
        unsafe { self.heap.dealloc(self.pointer.as_ptr(), self.layout) };
    }
}

#[cfg(test)]
pub(crate) mod tests;
