//! `--profile`'s window marks and the guest images' identity
//! (`stela_boot_core::profile_window`).
//!
//! A mark is Control+Option+Shift+M on a host keyboard ([`filter`]): the
//! M stays from the guest, `PROFILE MARK n` is logged at once, and the
//! heartbeat ends a beat at its next boundary (`run`), so every per-beat
//! line splits at the mark and window `n - 1`'s tables are said. Window 0
//! is the boot up to the first mark.
//!
//! [`GuestImages`] finds the dyld shared cache's header and the kernel
//! collection's from the boot PE's state between slices, a bounded number
//! of reads each time, and logs their UUIDs and slides once.
use super::parallel::Board;
use crate::output;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};
use stela_boot_core::profile_window::{
    self as window, DyldHeader, KeyAction, MACH_HEADER_BYTES, MH_FILESET, MarkKeys,
};
use stela_drivers::input::Input;
use stela_engine::machine::{Host, Machine, system::Register};

/// `--profile` is on: marks are looked for.
static ENABLED: AtomicBool = AtomicBool::new(false);
/// Marks pressed since the boot.
static MARKS: AtomicU32 = AtomicU32::new(0);
/// Marks the heartbeat has ended a beat for.
static TAKEN: AtomicU32 = AtomicU32::new(0);

/// Under `--profile`: look for marks from now on.
pub(super) fn enable() {
    ENABLED.store(true, Relaxed);
}

/// Keep each mark's M from the guest, and count the mark
/// ([`stela_boot_core::profile_window::MarkKeys`]).
pub(super) fn filter(keys: &mut MarkKeys, inputs: &mut Vec<Input>) {
    if !ENABLED.load(Relaxed) {
        return;
    }
    inputs.retain(|input| {
        let Input::Key { usage, pressed } = *input else {
            return true;
        };
        match keys.key(usage, pressed) {
            KeyAction::Deliver => true,
            KeyAction::Swallow => false,
            KeyAction::Mark => {
                let mark = MARKS.fetch_add(1, Relaxed) + 1;
                output::arguments(format_args!(
                    "PROFILE MARK {mark}: Control+Option+Shift+M; window {} ends and window \
                     {mark} begins at the next beat\n",
                    mark - 1
                ));
                false
            }
        }
    });
}

/// The next mark the heartbeat has not ended a beat for, taking it.
pub(super) fn take_mark() -> Option<u32> {
    let taken = TAKEN.load(Relaxed);
    (MARKS.load(Relaxed) > taken).then(|| {
        TAKEN.store(taken + 1, Relaxed);
        taken + 1
    })
}

/// The dyld shared region's base on arm64 macOS (`SHARED_REGION_BASE_ARM64`)
/// and the span searched above it for the cache's first header.
const DYLD_BASE: u64 = 0x1_8000_0000;
const DYLD_SPAN: u64 = 0x1_0000_0000;
/// Kernel collections lie this far below a kernel PC at most.
const KERNEL_SPAN: u64 = 0x8000_0000;
/// The guest's page: headers start on one.
const GUEST_PAGE: u64 = 0x4000;
/// The engine's fetch page: a read lies in one.
const FETCH_PAGE: u64 = 0x1000;
/// Header reads per [`GuestImages::step`].
const PROBES: u32 = 256;
/// Searches begun before giving up on a missing image.
const ATTEMPTS: u32 = 8;
/// Load commands read at most.
const COMMANDS_MOST: u32 = 256 << 10;

#[derive(Debug, Default)]
struct Search {
    /// The next page to probe and the last, when a search runs.
    at: Option<(u64, u64)>,
    attempts: u32,
    done: bool,
}

/// The guest's dyld shared cache and kernel collection headers, searched
/// for while the boot PE runs at EL0 and in the kernel.
#[derive(Debug, Default)]
pub(super) struct GuestImages {
    dyld: Search,
    kernel: Search,
}

/// Read `bytes.len()` bytes at `address`, a fetch page at a time.
fn read<H: Host>(machine: &mut Machine<H, Board>, address: u64, bytes: &mut [u8]) -> bool {
    let mut done = 0usize;
    while done < bytes.len() {
        let at = address.wrapping_add(done as u64);
        let room = (FETCH_PAGE - (at & (FETCH_PAGE - 1))) as usize;
        let take = room.min(bytes.len() - done);
        if !machine.read_virtual(at, &mut bytes[done..done + take]) {
            return false;
        }
        done += take;
    }
    true
}

impl GuestImages {
    /// Probe a bounded number of pages for the image the boot PE's level
    /// can see; say each image once found.
    pub(super) fn step<H: Host>(&mut self, machine: &mut Machine<H, Board>) {
        if self.dyld.done && self.kernel.done {
            return;
        }
        let (el, guarded, pc) = (
            machine.cpu.pstate.el,
            machine.cpu.pstate.guarded,
            machine.cpu.pc,
        );
        if el == 0 && !self.dyld.done {
            self.step_dyld(machine);
        } else if el != 0 && !guarded && pc >> 63 == 1 && !self.kernel.done {
            self.step_kernel(machine, pc);
        }
    }

    fn step_dyld<H: Host>(&mut self, machine: &mut Machine<H, Board>) {
        let search = &mut self.dyld;
        let (mut at, end) = *search.at.get_or_insert((DYLD_BASE, DYLD_BASE + DYLD_SPAN));
        for _ in 0..PROBES {
            let mut magic = [0u8; 8];
            if read(machine, at, &mut magic) && magic.starts_with(window::DYLD_MAGIC) {
                let mut header = [0u8; 0x1000];
                if read(machine, at, &mut header)
                    && let Some(header) = window::dyld_header(&header)
                {
                    say_dyld(at, &header);
                    search.done = true;
                    return;
                }
            }
            at += GUEST_PAGE;
            if at >= end {
                search.at = None;
                search.attempts += 1;
                if search.attempts >= ATTEMPTS {
                    search.done = true;
                    output::arguments(format_args!(
                        "PROFILE DYLD CACHE not found: no dyld_v1 header on a 16 KiB page \
                         of {DYLD_BASE:#x}..{:#x} in {ATTEMPTS} searches\n",
                        DYLD_BASE + DYLD_SPAN
                    ));
                }
                return;
            }
        }
        search.at = Some((at, end));
    }

    fn step_kernel<H: Host>(&mut self, machine: &mut Machine<H, Board>, pc: u64) {
        let search = &mut self.kernel;
        let start = pc & !(GUEST_PAGE - 1);
        let (mut at, end) = *search
            .at
            .get_or_insert((start, start.saturating_sub(KERNEL_SPAN)));
        for _ in 0..PROBES {
            let mut bytes = [0u8; MACH_HEADER_BYTES];
            if read(machine, at, &mut bytes)
                && let Some(header) = window::mach_header(&bytes)
                && header.filetype == MH_FILESET
            {
                say_kernel(machine, at, header);
                search.done = true;
                return;
            }
            at = at.wrapping_sub(GUEST_PAGE);
            if at <= end || at >> 63 == 0 {
                // A PC in SPTM, TXM or an image below none: another PC
                // starts the next search.
                search.at = None;
                search.attempts += 1;
                if search.attempts >= ATTEMPTS {
                    search.done = true;
                    output::arguments(format_args!(
                        "PROFILE KERNEL COLLECTION not found: no MH_FILESET header within \
                         {KERNEL_SPAN:#x} below {ATTEMPTS} kernel PCs\n"
                    ));
                }
                return;
            }
        }
        search.at = Some((at, end));
    }
}

fn say_dyld(at: u64, header: &DyldHeader) {
    output::arguments(format_args!(
        "PROFILE DYLD CACHE base={at:#x} unslid={:#x} slide={:#x} uuid={} magic={:?}: \
         EL0 PCs less the slide are the cache's own addresses\n",
        header.unslid,
        at.wrapping_sub(header.unslid),
        window::uuid_text(&header.uuid),
        window::magic_text(&header.magic),
    ));
}

/// The load commands of the image whose header is at `at`, at most
/// [`COMMANDS_MOST`] of their bytes.
fn commands<H: Host>(
    machine: &mut Machine<H, Board>,
    at: u64,
    header: window::MachHeader,
) -> Option<window::LoadInfo> {
    let size = header.sizeofcmds.min(COMMANDS_MOST) as usize;
    let mut bytes = alloc::vec![0u8; size];
    read(machine, at + MACH_HEADER_BYTES as u64, &mut bytes)
        .then(|| window::load_info(&bytes, header.ncmds))
}

fn say_kernel<H: Host>(machine: &mut Machine<H, Board>, at: u64, header: window::MachHeader) {
    let info = commands(machine, at, header).unwrap_or_default();
    let unslid = info.text.unwrap_or(0);
    let slide = at.wrapping_sub(unslid);
    // The kernel's own header, slid as the collection is, names its UUID.
    let kernel = info.kernel.map(|kernel| kernel.wrapping_add(slide));
    let kernel_uuid = kernel.and_then(|kernel| {
        let mut bytes = [0u8; MACH_HEADER_BYTES];
        read(machine, kernel, &mut bytes).then_some(())?;
        commands(machine, kernel, window::mach_header(&bytes)?)?.uuid
    });
    // The vector base the PC samples' `v+` offsets are from: EL2's under
    // VHE (`pc_sampling`); the search runs in the kernel only.
    let vbar = match machine.cpu.pstate.el == 2 {
        true => machine.cpu.read(Register::VBAR_EL2),
        false => machine.cpu.read(Register::VBAR_EL1),
    } & !0x7ff;
    let text = |uuid: Option<[u8; 16]>| uuid.map_or("?".into(), |uuid| window::uuid_text(&uuid));
    output::arguments(format_args!(
        "PROFILE KERNEL COLLECTION base={at:#x} unslid={unslid:#x} slide={slide:#x} uuid={} \
         kernel={} kernel_uuid={} vbar={vbar:#x} vbar_unslid={:#x}: a kernel or guarded sample's \
         v+offset is at vbar_unslid+offset in the collection\n",
        text(info.uuid),
        kernel.map_or("?".into(), |kernel| alloc::format!("{kernel:#x}")),
        text(kernel_uuid),
        vbar.wrapping_sub(slide),
    ));
}
