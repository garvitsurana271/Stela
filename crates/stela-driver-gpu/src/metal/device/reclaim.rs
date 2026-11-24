//! A device whose backend runs out of memory.
//!
//! The device keeps memory only to save the driver's kernel calls: destroyed
//! private textures for the next equal descriptor, free upload chunks for
//! the next upload, and released objects a bounded retirement left for the
//! next. A driver whose device memory is full spills to system memory, and
//! fails only when that is short too, so every allocation that fails for
//! want of memory frees these and is made once more before the call fails.
//! Each failure is counted, and the first and each power of two after it
//! leave a note for the platform's log: what failed, what was freed and
//! what the device still holds.
//!
//! The same notes carry the driver's own count of its heaps, and one is
//! left at the device's first call and at each step its holdings rise
//! ([`Device::note_memory_step`]), so a log shows how full the device memory
//! was before anything failed.
//!
//! What the GPU's work in flight holds back is freed only as that work
//! completes: released objects it may still read, and the upload chunks it
//! carries. So when freeing what the device keeps is not enough, the device
//! waits, at most [`RETIRE_WAIT_NS`], for the GPU to finish what it was
//! given, retires it and frees again before the allocation fails: a burst
//! of allocations a frame ahead of the GPU (Setup's Liquid Glass UI, which
//! blurs full-screen RGBA16F targets) then waits for the frames it replaces
//! rather than fails a WindowServer commit, which ends WindowServer. The
//! share's and a test build's caps still refuse what they would not hold.
//! An allocation the wait saved leaves a note of its own
//! ([`Device::note_saved`]).
//!
//! A test build may hold the device to a count of bytes
//! ([`VRAM_CAP`](crate::metal::vulkan::physical::VRAM_CAP)), at which its
//! allocations run out of memory as a full driver's would: a GPU with much
//! memory then takes the paths one with little takes.
//!
//! An allocation whose host kernel call the platform withdrew, no task
//! having taken it in time, fails the same way and is not one of these: the
//! service makes its call again where it may, and the device makes no
//! second allocation in its place.
use super::Device;
use crate::metal::{
    compiler::CompileLock,
    error::{Error, Result},
    shader::Compiler,
    vulkan::{
        backend::{Backend, BufferAllocation},
        object::{BufferInfo, Image, ImageInfo, Memory, MemoryRequest},
    },
};
use alloc::{collections::VecDeque, format, string::String, sync::Arc};

/// Notes the device keeps until a call takes them.
const NOTES: usize = 8;

/// The longest a device out of memory waits for the GPU to finish the work
/// in flight: about twice the slowest frame measured, 112 ms of GPU time for
/// one frame of Setup's Liquid Glass UI on a Vega 7 (gfx 9.3) with ten
/// command buffers in flight.
pub const RETIRE_WAIT_NS: u64 = 250_000_000;

/// What a device out of memory gave up to try again, in bytes.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Reclaimed {
    /// Destroyed private textures kept for reuse.
    pub recycled: u64,
    /// Free upload chunks.
    pub chunks: u64,
    /// Released objects a bounded retirement had left undestroyed.
    pub released: u64,
    /// The command buffers that finished while the device waited for the
    /// work in flight, and the bytes their retirement freed: released
    /// objects they held back (upload chunks they carried count in
    /// `chunks`).
    pub retired: usize,
    pub retired_bytes: u64,
}

impl Reclaimed {
    fn any(&self) -> bool {
        self.recycled != 0 || self.chunks != 0 || self.released != 0 || self.retired_bytes != 0
    }

    fn and(self, more: Self) -> Self {
        Self {
            recycled: self.recycled + more.recycled,
            chunks: self.chunks + more.chunks,
            released: self.released + more.released,
            retired: self.retired + more.retired,
            retired_bytes: self.retired_bytes + more.retired_bytes,
        }
    }
}

/// The least a device's holdings rise between two notes of them.
const STEP_BYTES: u64 = 64 << 20;

/// The device's out-of-memory count, the holdings its next note of them
/// waits for, and the notes it left.
#[derive(Debug, Default)]
pub(super) struct Shortage {
    count: u64,
    /// The allocations made only once the work in flight retired.
    saved: u64,
    /// The device polls to retire the work in flight
    /// ([`Device::retire_in_flight`]); an allocation that makes waits no
    /// more.
    retiring: bool,
    next_step: u64,
    notes: VecDeque<String>,
}

impl<B: Backend, C: Compiler> Device<B, C> {
    /// [`Backend::create_image`], made once more after the device frees
    /// what it keeps for reuse when memory runs out.
    pub(super) fn make_image(
        &mut self,
        info: &ImageInfo,
        request: MemoryRequest,
    ) -> Result<(Image, Memory, u64)> {
        // What the image's texels take, for the share's budget: four bytes
        // each, a third more for its levels, by its samples.
        let [width, height, depth] = info.extent;
        let texels = u64::from(width) * u64::from(height) * u64::from(depth);
        let bytes = texels
            .saturating_mul(4 * u64::from(info.array_layers.max(1)))
            .saturating_mul(u64::from(info.samples.bits().max(1)))
            .saturating_mul(if info.mip_levels > 1 { 4 } else { 3 })
            / 3;
        self.reclaiming(
            bytes,
            |backend| backend.create_image(info, request),
            || {
                let [width, height, depth] = info.extent;
                format!(
                    "a {width}x{height}x{depth} {:?} image of {} levels, {} layers and {:?} samples",
                    info.format, info.mip_levels, info.array_layers, info.samples
                )
            },
        )
    }

    /// [`Backend::create_buffer`], as [`Self::make_image`].
    pub(super) fn make_buffer(
        &mut self,
        info: &BufferInfo,
        request: MemoryRequest,
    ) -> Result<BufferAllocation> {
        self.reclaiming(
            info.size,
            |backend| backend.create_buffer(info, request),
            || {
                let place = if request.mapped { "mapped" } else { "device" };
                format!("a {place} buffer of {} bytes", info.size)
            },
        )
    }

    /// [`Backend::allocate_memory`], as [`Self::make_image`].
    pub(super) fn make_memory(
        &mut self,
        bytes: u64,
        request: MemoryRequest,
        memory_types: u32,
    ) -> Result<Memory> {
        self.reclaiming(
            bytes,
            |backend| backend.allocate_memory(bytes, request, memory_types),
            || format!("heap memory of {bytes} bytes"),
        )
    }

    fn reclaiming<T>(
        &mut self,
        bytes: u64,
        mut make: impl FnMut(&mut B) -> Result<T>,
        what: impl FnOnce() -> String,
    ) -> Result<T> {
        let first = if self.over_cap() || self.share_refuses(bytes) {
            Err(Error::OutOfMemory)
        } else {
            make(&mut self.backend)
        };
        if !matches!(first, Err(Error::OutOfMemory)) || self.host_call_withdrawn() {
            return first;
        }
        let mut reclaimed = self.reclaim()?;
        let mut made = if reclaimed.any() && self.would_hold(bytes) {
            make(&mut self.backend)
        } else {
            first
        };
        // Still short: what the work in flight holds back, once it is done.
        let mut saved = false;
        if matches!(made, Err(Error::OutOfMemory)) && !self.host_call_withdrawn() {
            let retired = self.retire_in_flight();
            if retired.retired != 0 {
                let more = retired.and(self.reclaim()?);
                reclaimed = reclaimed.and(more);
                if more.any() && self.would_hold(bytes) {
                    made = make(&mut self.backend);
                    saved = made.is_ok();
                }
            }
        }
        let what = what();
        match saved {
            true => self.note_saved(&what, reclaimed),
            false => self.note_shortage(&what, reclaimed, made.is_ok()),
        }
        made
    }

    /// Whether neither a test build's cap nor the share refuses an
    /// allocation of `bytes`.
    fn would_hold(&mut self, bytes: u64) -> bool {
        !self.over_cap() && !self.share_refuses(bytes)
    }

    /// Waits, at most [`RETIRE_WAIT_NS`], for the GPU to finish the work it
    /// was given whole, when that work holds back released objects or upload
    /// chunks ([`super::queue::Queue::retiring_serial`]), and retires what
    /// finished, a wait that timed out included: the command buffers retired
    /// and the bytes of released objects their retirement destroyed. Nothing
    /// when no such work runs, the device is lost or the wait fails, or
    /// within an allocation the retirement itself made.
    fn retire_in_flight(&mut self) -> Reclaimed {
        if self.lost || self.shortage.retiring {
            return Reclaimed::default();
        }
        let Some(serial) = self.queue.retiring_serial(!self.deferred.is_empty()) else {
            return Reclaimed::default();
        };
        let (flights, allocated) = (self.queue.in_flight_count(), self.allocated);
        if let Err(error) = self
            .backend
            .wait_semaphore(self.timeline, serial, RETIRE_WAIT_NS)
        {
            if error == Error::DeviceLost {
                self.mark_lost();
            }
            return Reclaimed::default();
        }
        self.shortage.retiring = true;
        let polled = self.poll();
        self.shortage.retiring = false;
        if polled.is_err() {
            return Reclaimed::default();
        }
        Reclaimed {
            retired: flights.saturating_sub(self.queue.in_flight_count()),
            retired_bytes: allocated.saturating_sub(self.allocated),
            ..Reclaimed::default()
        }
    }

    /// Whether a test build's cap leaves the device no memory: it holds as
    /// much as the cap in resources, recycled textures and upload chunks.
    fn over_cap(&self) -> bool {
        self.memory_cap.is_some_and(|cap| self.guest_bytes() >= cap)
    }

    /// The bytes the device holds: resources, recycled textures and upload
    /// chunks.
    fn guest_bytes(&self) -> u64 {
        self.allocated + self.recycled().bytes + self.upload_chunks().bytes
    }

    /// Whether an integrated GPU's share of RAM would not hold an
    /// allocation of `bytes` ([`super::share::refuses`]). The first asks
    /// the share's state is the base the device's growth starts from.
    fn share_refuses(&mut self, bytes: u64) -> bool {
        let guest = self.guest_bytes();
        let Some(budget) = &mut self.share else {
            return false;
        };
        let Some(state) = (budget.probe)() else {
            return false;
        };
        let base = *budget.base.get_or_insert(super::share::Base {
            held: state.held,
            guest,
        });
        super::share::refuses(state, base, guest, bytes)
    }

    /// The share's state for a note: Linux's pages of it and its size.
    fn share_note(&self) -> String {
        match self.share.and_then(|budget| (budget.probe)()) {
            Some(state) => format!(
                "; the GPU's share of RAM: Linux holds {}M of {}M",
                state.held >> 20,
                state.limit >> 20
            ),
            None => String::new(),
        }
    }

    /// Ties the device to a share with `probe`, as the platform's does.
    #[cfg(test)]
    pub(super) fn share_memory(&mut self, probe: super::share::Probe) {
        self.share = Some(super::share::Budget { probe, base: None });
    }

    /// Holds the device to `bytes`, as a test build's cap does.
    #[cfg(test)]
    pub(super) fn cap_memory(&mut self, bytes: u64) {
        self.memory_cap = Some(bytes);
    }

    /// Gives the device the service's lock, through which the platform
    /// says that it withdrew a host call of the call served now.
    pub fn watch_host_calls(&mut self, lock: &Arc<CompileLock>) {
        if self.host_calls.is_none() {
            self.host_calls = Some(lock.clone());
        }
    }

    /// Whether the platform withdrew a host call of the call served now,
    /// no task having taken it in time. The driver fails such an allocation
    /// as out of memory, which it is not: nothing is freed for it, and no
    /// second allocation is made, whose host call would hold the caller's
    /// processor as long again. The service makes the call again when it
    /// may.
    fn host_call_withdrawn(&self) -> bool {
        self.host_calls
            .as_ref()
            .is_some_and(|lock| lock.withdrawn())
    }

    /// Frees what the device keeps only for reuse. What submitted work may
    /// still read is freed when that work completes, as ever.
    fn reclaim(&mut self) -> Result<Reclaimed> {
        let recycled = self.recycled().bytes;
        self.trim_recycled(0, 0)?;
        let chunks = self.drop_free_chunks();
        let released = self.destroy_released();
        Ok(Reclaimed {
            recycled,
            chunks,
            released,
            ..Reclaimed::default()
        })
    }

    /// Destroys the released objects nothing in flight may read any longer,
    /// which a bounded retirement left; answers their bytes.
    fn destroy_released(&mut self) -> u64 {
        if self.lost || !self.destruction_left {
            return 0;
        }
        let mut bytes = 0;
        let mut kept = VecDeque::with_capacity(self.deferred.len());
        for (release, object, allocated) in core::mem::take(&mut self.deferred) {
            if self.frees_object(&release, object) {
                self.destroy_now(object);
                self.allocated -= allocated;
                bytes += allocated;
            } else {
                kept.push_back((release, object, allocated));
            }
        }
        self.deferred = kept;
        self.destruction_left = false;
        bytes
    }

    fn note_shortage(&mut self, what: &str, reclaimed: Reclaimed, made: bool) {
        self.shortage.count += 1;
        let count = self.shortage.count;
        if !count.is_power_of_two() || self.shortage.notes.len() >= NOTES {
            return;
        }
        let mib = |bytes: u64| bytes >> 20;
        let waited = match reclaimed.retired {
            0 => String::new(),
            retired => format!(
                " once the GPU finished {retired} command buffers, whose retirement freed {}M,",
                mib(reclaimed.retired_bytes)
            ),
        };
        let outcome = match (reclaimed.any(), made) {
            (false, _) => format!("it kept nothing to free{}", waited.trim_end_matches(',')),
            (true, made) => format!(
                "it freed recycled={}M chunks={}M released={}M{waited} and tried again, which {}",
                mib(reclaimed.recycled),
                mib(reclaimed.chunks),
                mib(reclaimed.released),
                if made { "succeeded" } else { "failed" }
            ),
        };
        let held = self.memory();
        let driver = self.driver_heaps();
        self.shortage.notes.push_back(format!(
            "OUT OF MEMORY {count}: {what} failed; {outcome}; the device holds allocated={}M \
             deferred={}M behind {} command buffers recycled={}M chunks={}M; {driver}{}",
            mib(held.allocated),
            mib(held.deferred_bytes),
            held.in_flight,
            mib(held.recycled.bytes),
            mib(held.chunks.bytes),
            self.share_note(),
        ));
    }

    /// Counts an allocation made only once the work in flight retired
    /// ([`Self::retire_in_flight`]), among the shortages too, and notes the
    /// first and each power of two after: what was made, what the wait and
    /// the freeing gave, and what the device holds now.
    fn note_saved(&mut self, what: &str, reclaimed: Reclaimed) {
        self.shortage.count += 1;
        self.shortage.saved += 1;
        let saved = self.shortage.saved;
        if !saved.is_power_of_two() || self.shortage.notes.len() >= NOTES {
            return;
        }
        let mib = |bytes: u64| bytes >> 20;
        let held = self.memory();
        self.shortage.notes.push_back(format!(
            "OUT OF MEMORY AVERTED {saved}: {what} was made once the GPU finished {} command \
             buffers, whose retirement freed {}M, and the device freed recycled={}M chunks={}M \
             released={}M; the device holds allocated={}M deferred={}M behind {} command \
             buffers{}",
            reclaimed.retired,
            mib(reclaimed.retired_bytes),
            mib(reclaimed.recycled),
            mib(reclaimed.chunks),
            mib(reclaimed.released),
            mib(held.allocated),
            mib(held.deferred_bytes),
            held.in_flight,
            self.share_note(),
        ));
    }

    /// The allocations made only once the work in flight retired.
    pub fn out_of_memory_averted(&self) -> u64 {
        self.shortage.saved
    }

    /// The driver's heaps as it counts them now, for a note. Not asked
    /// once the platform withdrew a host call of the call served now: the
    /// driver's count is a host call too, which would hold the caller's
    /// processor as long again.
    fn driver_heaps(&mut self) -> String {
        if self.host_call_withdrawn() {
            return String::from("the driver's heaps were not asked, as a host call waited");
        }
        let heaps = self.backend.memory_heaps();
        if heaps.is_empty() {
            return String::from("the driver does not say what its heaps hold");
        }
        let mut text = String::from("driver heaps use/budget/size:");
        for heap in heaps {
            let kind = if heap.device_local { "local" } else { "host" };
            text.push_str(&format!(
                " {}/{}/{}M/{kind}",
                heap.usage >> 20,
                heap.budget >> 20,
                heap.size >> 20
            ));
        }
        text
    }

    /// Notes what the device holds when the call served now is its first,
    /// and each time its holdings pass the next sixteenth of the memory the
    /// guest was told to work in (at least [`STEP_BYTES`]) above the last
    /// note's: a log then shows the device's memory as it filled, with the
    /// driver's own count of its heaps at each step.
    pub fn note_memory_step(&mut self) {
        let held = self.allocated + self.recycled().bytes;
        if held < self.shortage.next_step || self.shortage.notes.len() >= NOTES {
            return;
        }
        let budget = self.capabilities.recommended_max_working_set_size;
        let step = (budget / 16).max(STEP_BYTES);
        self.shortage.next_step = (held / step + 1) * step;
        let mib = |bytes: u64| bytes >> 20;
        let memory = self.memory();
        let driver = self.driver_heaps();
        // A capped test build says so in each note, so its log is never
        // read as a small GPU's.
        let cap = self.memory_cap.map_or_else(String::new, |cap| {
            format!("; a test build caps the device at {}M", mib(cap))
        });
        // An integrated GPU's memory is system memory: Linux's pages of its
        // share of RAM, whose state ends the note, or, with no share, the
        // pages of Stela's heap, which the note's reader weighs it against.
        let kind = match (self.capabilities.has_unified_memory, self.share.is_some()) {
            (true, true) => "SHARED MEMORY (the GPU's share of RAM)",
            (true, false) => "SHARED MEMORY (system memory from Stela's heap)",
            (false, _) => "VRAM",
        };
        self.shortage.notes.push_back(format!(
            "{kind} holds {}M of the {}M the guest works in: allocated={}M recycled={}M \
             chunks={}M blocks={}/{}M out_of_memory={}; {driver}{}{cap}",
            mib(held + memory.chunks.bytes),
            mib(budget),
            mib(memory.allocated),
            mib(memory.recycled.bytes),
            mib(memory.chunks.bytes),
            memory.blocks.blocks,
            mib(memory.blocks.block_bytes),
            memory.out_of_memory,
            self.share_note(),
        ));
    }

    /// How often an allocation of the backend's ran out of memory.
    pub fn out_of_memory(&self) -> u64 {
        self.shortage.count
    }

    /// The oldest note of the device's memory that no call has reported.
    pub fn take_memory_note(&mut self) -> Option<String> {
        self.shortage.notes.pop_front()
    }
}
