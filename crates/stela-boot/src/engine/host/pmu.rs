//! `--profile`'s hardware performance counters: the `PES PMU` heartbeat line.
//!
//! Each PE programs its core's counters once ([`start`]) to count in ring 0
//! and reads them with RDPMC at the step loop's phase charges
//! ([`stela_engine::machine::Host::profile_boundary`]): the counts since the
//! previous charge go to the charged phase, so the step loop's unattributed
//! time lands in the phase charged next. Shadow page-fault fills taken during
//! native runs are counted again apart (`fill`); `native` includes them.
//!
//! RDPMC may trap under a hypervisor. [`start`] times it, and reads take at
//! most 0.2% of a PE's time (`READ_SHARE`): every read of the set and every
//! reprogramming is timed with the TSC (which does not trap), and a core
//! reads again only while its measured reading time stays within
//! 1/`READ_SHARE` of the TSC ticks since its start; the line prints the
//! beat's measured share as `overhead=`. The bound holds whatever a read
//! costs at the time (a trapped read's cost varies with the hypervisor's
//! load), on every core, and covers the reads around shadow fills. A cheap
//! read reads the counters around one charged interval at most every
//! `2 * READ_SHARE` read costs (`method=interval:GKticks`); every interval's
//! TSC ticks are kept, and the line scales each phase's counts by its ticks
//! over its sampled ticks. A trapped read would bias short intervals (each
//! starts with the cache and predictor refill after the trap), so the core
//! instead reads at most once per window of 2 ms or more (`method=window`):
//! the whole PE takes every window, and a phase only the windows it held 90%
//! of. Counter MSRs are written only at [`start`] and when the event set
//! rotates, once a beat: with more events than counters the two fixed events
//! (cycles, instructions) stay and the others rotate in sets (`set=S/N`),
//! each beat reporting the set it ran.
//!
//! Events: AMD families 19h and 1Ah (Zen 3 to 5) from the PPR for family 1Ah
//! (PMCx076 cycles not halted, PMCx0C0 retired instructions, PMCx0C3 retired
//! mispredicted branches, PMCx18E umask 18h instruction cache misses,
//! PMCx084 L1 ITLB miss L2 hit, PMCx085 umask 0Fh L2 ITLB miss (walk),
//! PMCx045 umask 0Fh / F0h L1 DTLB miss L2 hit / L2 miss (walk), PMCx064
//! umask 09h L2 misses of instruction and data requests, PMCx044 umask 48h
//! data fills from DRAM or I/O, the core PMU's last-level miss proxy: L3
//! misses proper are only in the L3 PMU; PMCx1A0 umask 01h / 1Eh dispatch
//! slots left empty by the front end / by back-end stalls, PMCx0AA umask
//! 07h ops dispatched and PMCx0C1 ops retired, the top-down split of Zen 4
//! and 5: 6 or 8 slots per cycle, and dispatched less retired ops is bad
//! speculation). Other AMD families count cycles,
//! instructions and mispredicts. Intel counts the architectural events
//! CPUID leaf 0Ah reports (cycles, instructions, mispredicted branches, LLC
//! misses) and, on the Core models from Skylake on, the front end's empty
//! slots, the instruction cache's stalls and the code reads that missed it
//! (`FRONT_END_SLOTS_INTEL` and after), with the slots themselves from Ice
//! Lake on; a hybrid part's Atom cores count their own encodings of these,
//! kept apart in the window line. Other Intel models count the
//! architectural events only, said once.
//!
//! [`window_report`] says the counts of a marked window (`PROFILE MARK`,
//! `profile_window`) as ratios: IPC, the front end's and back end's shares
//! of the slots, instruction cache misses per thousand instructions.
use super::super::interrupts::Frame;
use crate::output;
use alloc::{boxed::Box, format, string::String, vec::Vec};
use core::arch::{
    asm, global_asm,
    x86_64::{__cpuid, __cpuid_count},
};
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering::Relaxed};
use stela_boot_core::host_samples::SAMPLED;
use stela_engine::machine::engine::step_timing::Phase;

/// Counters this module uses at most.
const MAX_COUNTERS: usize = 6;
/// The step loop's phases, then shadow fills.
const BUCKETS: usize = Phase::COUNT + 1;
const FILL: usize = Phase::COUNT;
const BUCKET_NAMES: [&str; BUCKETS] = ["sch", "dev", "fetch", "nat", "exit", "fill"];
/// Report order: the phases that matter most first.
const REPORT_ORDER: [usize; BUCKETS] = [3, 5, 2, 4, 0, 1];
const MAX_SETS: usize = 4;
const MAX_CORES: usize = 256;
/// Reading the counters may take at most 1/READ_SHARE of a PE's time
/// (0.2%), measured: a core reads the set only while its reads and
/// reprogrammings so far, plus the next read at its last measured cost, fit
/// in 1/READ_SHARE of the TSC ticks since its start ([`Own::affords`]). A
/// trapped read (about 100 us under KVM) then allows one read per 50 ms; a
/// native one (tens of cycles) dense sampling.
const READ_SHARE: u64 = 500;
/// A read of the set dearer than this (RDPMC trapped by a hypervisor)
/// switches the core to windows: reading around short intervals would
/// measure the refill after each trap, not the interval.
pub(super) const WINDOW_READ_TICKS: u64 = 2_000;
/// A window lasts at least this many ticks (about 2 ms at 3 GHz), and at
/// least READ_SHARE reads' cost. A tick count, not a time.
const WINDOW_TICKS: u64 = 6_000_000;
/// A window is credited to a bucket that holds at least this share of its
/// ticks, in tenths.
const DOMINANT_TENTHS: u64 = 9;

global_asm!(
    ".text",
    ".global stela_pmu_wrmsr",
    "stela_pmu_wrmsr:",
    "mov ecx, edi",
    "mov eax, esi",
    "mov rdx, rsi",
    "shr rdx, 32",
    ".global stela_pmu_wrmsr_instruction",
    "stela_pmu_wrmsr_instruction:",
    "wrmsr",
    "xor eax, eax",
    "ret",
    ".global stela_pmu_rdpmc",
    "stela_pmu_rdpmc:",
    "mov ecx, edi",
    ".global stela_pmu_rdpmc_instruction",
    "stela_pmu_rdpmc_instruction:",
    "rdpmc",
    "xor eax, eax",
    "ret",
    ".global stela_pmu_rdmsr",
    "stela_pmu_rdmsr:",
    "mov ecx, edi",
    ".global stela_pmu_rdmsr_instruction",
    "stela_pmu_rdmsr_instruction:",
    "rdmsr",
    "mov dword ptr [rsi], eax",
    "mov dword ptr [rsi + 4], edx",
    "xor eax, eax",
    "ret",
    ".global stela_pmu_refused",
    "stela_pmu_refused:",
    "mov eax, 1",
    "ret",
);

unsafe extern "sysv64" {
    fn stela_pmu_wrmsr(index: u32, value: u64) -> u32;
    fn stela_pmu_rdpmc(counter: u32) -> u32;
    fn stela_pmu_rdmsr(index: u32, value: *mut u64) -> u32;
    static stela_pmu_wrmsr_instruction: u8;
    static stela_pmu_rdpmc_instruction: u8;
    static stela_pmu_rdmsr_instruction: u8;
    static stela_pmu_refused: u8;
}

/// Resume a #GP of the probing WRMSR, RDPMC or RDMSR at its refusal: the
/// event, counter or register is unsupported.
pub(crate) fn fixup(frame: &mut Frame) -> bool {
    let rip = frame.rip;
    if frame.vector != 13
        || frame.cs & 3 != 0
        || (rip != (&raw const stela_pmu_wrmsr_instruction).addr() as u64
            && rip != (&raw const stela_pmu_rdpmc_instruction).addr() as u64
            && rip != (&raw const stela_pmu_rdmsr_instruction).addr() as u64)
    {
        return false;
    }
    frame.rip = (&raw const stela_pmu_refused).addr() as u64;
    true
}

/// WRMSR that answers a #GP with `false`.
pub(crate) fn write(index: u32, value: u64) -> bool {
    // SAFETY: ring 0; a refused write takes #GP, which `fixup` resumes.
    unsafe { stela_pmu_wrmsr(index, value) == 0 }
}

/// RDMSR that answers a #GP with `None`: the register is not implemented.
pub(crate) fn read(index: u32) -> Option<u64> {
    let mut value = 0;
    // SAFETY: ring 0; an unimplemented register takes #GP, which `fixup`
    // resumes; the value goes to this frame's `value`.
    (unsafe { stela_pmu_rdmsr(index, &mut value) } == 0).then_some(value)
}

/// This processor's performance registers for
/// [`stela_boot_core::host_performance`], whose model-specific ratio
/// registers are probed: a refused read is `None`, a refused write has no
/// effect. Needs this core's exception handler (`interrupts::interrupt`),
/// which resumes the probe.
pub(crate) struct PerformanceRegisters;

impl stela_boot_core::host_performance::Registers for PerformanceRegisters {
    unsafe fn read(&mut self, msr: stela_boot_core::msr::Msr) -> u64 {
        // SAFETY: the caller's contract.
        unsafe { msr.read() }
    }

    /// A write the processor refuses (#GP, as a hypervisor may answer a
    /// switch it does not model) leaves the register as it was, which the
    /// request reads back where it matters (turbo).
    unsafe fn write(&mut self, msr: stela_boot_core::msr::Msr, value: u64) {
        write(msr.index, value);
    }

    unsafe fn probe(&mut self, index: u32) -> Option<u64> {
        read(index)
    }
}

/// Where this host keeps its package energy counter: 0 before the probe,
/// 1 none, 2 Intel's model-specific RAPL registers, 3 AMD's.
static PACKAGE_ENERGY: AtomicU32 = AtomicU32::new(0);

/// This processor's package energy unit register and counter
/// (`stela_boot_core::host_clock::PackageEnergy`), where it has them: AMD's
/// where CPUID enumerates RAPL, Intel's where a probe of both registers
/// succeeds; found once for every processor, which share a vendor.
pub(crate) fn package_energy() -> Option<(u64, u32)> {
    use stela_boot_core::host_clock::{
        AMD_PACKAGE_ENERGY, AMD_RAPL_UNIT, INTEL_PACKAGE_ENERGY, INTEL_RAPL_UNIT,
    };
    let source = match PACKAGE_ENERGY.load(Relaxed) {
        0 => {
            let identity = __cpuid(0);
            // "GenuineIntel".
            let intel = (identity.ebx, identity.edx, identity.ecx)
                == (0x756e_6547, 0x4965_6e69, 0x6c65_746e);
            let source = if AMD_RAPL_UNIT.implemented() {
                3
            } else if intel
                && read(INTEL_RAPL_UNIT).is_some()
                && read(INTEL_PACKAGE_ENERGY).is_some()
            {
                2
            } else {
                1
            };
            PACKAGE_ENERGY.store(source, Relaxed);
            source
        }
        source => source,
    };
    match source {
        2 => Some((read(INTEL_RAPL_UNIT)?, read(INTEL_PACKAGE_ENERGY)? as u32)),
        // SAFETY: CPUID 8000_0007H:EDX[14] enumerates both registers, and
        // reading them has no effect.
        3 => unsafe { Some((AMD_RAPL_UNIT.read(), AMD_PACKAGE_ENERGY.read() as u32)) },
        _ => None,
    }
}

/// The SMI counter's register (`stela_boot_core::smi`), probed once for
/// every processor, which share a vendor: `u32::MAX` before the probe, 0
/// where there is none.
static SMI_COUNTER: AtomicU32 = AtomicU32::new(u32::MAX);

/// The SMIs this processor has taken, where it counts them.
pub(crate) fn smi_count() -> Option<u64> {
    let counter = match SMI_COUNTER.load(Relaxed) {
        u32::MAX => {
            let identity = __cpuid(0);
            // "GenuineIntel".
            let intel = (identity.ebx, identity.edx, identity.ecx)
                == (0x756e_6547, 0x4965_6e69, 0x6c65_746e);
            let counter = stela_boot_core::smi::counter(intel)
                .filter(|&index| read(index).is_some())
                .unwrap_or(0);
            SMI_COUNTER.store(counter, Relaxed);
            counter
        }
        counter => counter,
    };
    match counter {
        0 => None,
        index => read(index),
    }
}

#[inline]
fn rdpmc(counter: u32) -> u64 {
    let (low, high): (u32, u32);
    // SAFETY: ring 0, a counter `start` probed.
    unsafe {
        asm!("rdpmc", in("ecx") counter, out("eax") low, out("edx") high,
            options(nomem, nostack, preserves_flags));
    }
    u64::from(high) << 32 | u64::from(low)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Event {
    name: &'static str,
    code: u16,
    umask: u8,
    /// Its encoding on an Intel hybrid part's Atom cores, where that
    /// differs; `None` there leaves the counter off on them.
    atom: Option<(u16, u8)>,
}

const fn event(name: &'static str, code: u16, umask: u8) -> Event {
    Event {
        name,
        code,
        umask,
        atom: Some((code, umask)),
    }
}

/// An Intel event with another encoding on Atom cores, or none there.
const fn intel(name: &'static str, code: u16, umask: u8, atom: Option<(u16, u8)>) -> Event {
    Event {
        name,
        code,
        umask,
        atom,
    }
}

const CYCLES_AMD: Event = event("cyc", 0x076, 0);
const INSTRUCTIONS_AMD: Event = event("ins", 0x0c0, 0);
const ZEN: [Event; 12] = [
    event("brmisp", 0x0c3, 0),
    event("icmiss", 0x18e, 0x18),
    event("itlbh", 0x084, 0),
    event("itlbw", 0x085, 0x0f),
    event("dtlbh", 0x045, 0x0f),
    event("dtlbw", 0x045, 0xf0),
    event("l2miss", 0x064, 0x09),
    event("dram", 0x044, 0x48),
    event("feslot", 0x1a0, 0x01),
    event("beslot", 0x1a0, 0x1e),
    event("disp", 0x0aa, 0x07),
    event("retops", 0x0c1, 0),
];
const CYCLES_INTEL: Event = event("cyc", 0x3c, 0);
const INSTRUCTIONS_INTEL: Event = event("ins", 0xc0, 0);
const MISPREDICTS_INTEL: Event = event("brmisp", 0xc5, 0);
const LLC_MISSES_INTEL: Event = event("llcmiss", 0x2e, 0x41);
/// The front end on the Core microarchitectures from Skylake on (Intel's
/// perfmon event files for SKL, ICL, TGL, ADL/RPL/MTL Golden Cove and
/// Redwood Cove, SPR): `IDQ_UOPS_NOT_DELIVERED.CORE` (9Ch/01h, slots the
/// front end left empty while the back end could take them; Gracemont's
/// `TOPDOWN_FE_BOUND.ALL` 71h/00h), `ICACHE_DATA.STALLS` (80h/04h, cycles
/// fetch waited for the instruction cache; Skylake's
/// `ICACHE_16B.IFDATA_STALL`, the same encoding), and `L2_RQSTS.ALL_CODE_RD`
/// (24h/E4h, code reads that missed the L1i and went to L2, prefetches
/// included; Gracemont's `ICACHE.MISSES` 80h/02h). The topdown slots there
/// are the core's 4 (Skylake) or 6 issue slots a cycle, Gracemont's 5.
const FRONT_END_SLOTS_INTEL: Event = intel("feslot", 0x9c, 0x01, Some((0x71, 0x00)));
const ICACHE_STALLS_INTEL: Event = intel("icstall", 0x80, 0x04, None);
const ICACHE_MISSES_INTEL: Event = intel("icmiss", 0x24, 0xe4, Some((0x80, 0x02)));
/// `TOPDOWN.SLOTS_P` (A4h/01h, Ice Lake on): the slots themselves on a
/// general counter; none on Skylake or Atom cores, where the width times
/// cycles stands for them.
const SLOTS_INTEL: Event = intel("slots", 0xa4, 0x01, None);
/// Family 6 models of the Skylake-derived Core cores (client and server).
const SKYLAKE_MODELS: [u8; 7] = [0x4e, 0x5e, 0x8e, 0x9e, 0xa5, 0xa6, 0x55];
/// Family 6 models from Ice Lake on whose Core cores count `TOPDOWN.SLOTS_P`:
/// Ice Lake, Tiger Lake, Rocket Lake, Sapphire and Emerald Rapids, and the
/// hybrid Alder, Raptor and Meteor Lake.
const SLOTS_MODELS: [u8; 13] = [
    0x7d, 0x7e, 0x6a, 0x6c, 0x8c, 0x8d, 0xa7, 0x8f, 0xcf, 0x97, 0x9a, 0xb7, 0xba,
];
/// Raptor Lake's other model and Meteor Lake, also with `TOPDOWN.SLOTS_P`.
const SLOTS_MODELS_MORE: [u8; 3] = [0xbf, 0xaa, 0xac];
/// The issue slots a cycle of an Intel Core core with `TOPDOWN.SLOTS_P`
/// (Sunny Cove on: 5 on Ice Lake and Tiger Lake, 6 on Golden Cove; the
/// slots event counts them itself, so this is only for the line's word),
/// of Skylake, and of Gracemont.
const SKYLAKE_WIDTH: u64 = 4;
const ATOM_WIDTH: u64 = 5;

/// `(family, model)` as CPUID leaf 1 gives them, the extended fields folded.
fn family_model() -> (u32, u8) {
    let eax = __cpuid(1).eax;
    let base = (eax >> 8) & 0xf;
    let model = ((eax >> 4) & 0xf) as u8;
    match base {
        0x6 | 0xf => (family(), model | (((eax >> 16) & 0xf) as u8) << 4),
        _ => (base, model),
    }
}

/// Whether this core is an Intel hybrid part's Atom core (CPUID leaf 1Ah
/// core type 20h).
fn atom_core() -> bool {
    __cpuid(0).eax >= 0x1a
        && __cpuid_count(7, 0).edx & (1 << 15) != 0
        && __cpuid_count(0x1a, 0).eax >> 24 == 0x20
}

/// How this core's counters are reached, and what they count.
struct Layout {
    counters: usize,
    /// Event select of counter `i`: `select + i * stride`.
    select: u32,
    stride: u32,
    /// The global enable register and its value, where there is one.
    global: Option<(u32, u64)>,
    amd: bool,
    fixed: Vec<Event>,
    rotating: Vec<Event>,
    described: String,
    /// Issue (dispatch) slots a cycle: what a slot event's share is of
    /// where no `slots` event counts them (0: one does); an Intel hybrid
    /// part's Atom cores' apart.
    width: u64,
    atom_width: u64,
    /// An Intel hybrid part with the front-end table: its Atom cores count
    /// their own encodings, and their counts are kept apart.
    hybrid: bool,
}

const AMD_VENDOR: [u32; 3] = [0x6874_7541, 0x6974_6e65, 0x444d_4163];
const INTEL_VENDOR: [u32; 3] = [0x756e_6547, 0x4965_6e69, 0x6c65_746e];

fn family() -> u32 {
    let eax = __cpuid(1).eax;
    let base = (eax >> 8) & 0xf;
    if base == 0xf {
        base + ((eax >> 20) & 0xff)
    } else {
        base
    }
}

fn layout() -> Option<Layout> {
    let vendor = __cpuid(0);
    let vendor = [vendor.ebx, vendor.edx, vendor.ecx];
    if vendor == AMD_VENDOR {
        let extended = __cpuid(0x8000_0000).eax;
        let core_ext = extended >= 0x8000_0001 && __cpuid(0x8000_0001).ecx & (1 << 23) != 0;
        let v2 = extended >= 0x8000_0022 && __cpuid(0x8000_0022).eax & 1 != 0;
        let (counters, select, stride) = if core_ext {
            let counters = if v2 {
                (__cpuid(0x8000_0022).ebx & 0xf) as usize
            } else {
                6
            };
            (counters, 0xc001_0200, 2)
        } else {
            (4, 0xc001_0000, 1)
        };
        let counters = counters.min(MAX_COUNTERS);
        let family = family();
        let zen = matches!(family, 0x19 | 0x1a);
        let rotating = if zen { ZEN.to_vec() } else { ZEN[..1].to_vec() };
        Some(Layout {
            counters,
            select,
            stride,
            global: v2.then(|| (0xc000_0301, (1u64 << counters) - 1)),
            amd: true,
            fixed: alloc::vec![CYCLES_AMD, INSTRUCTIONS_AMD],
            rotating,
            // Zen 3 and 4 dispatch 6 ops a cycle, Zen 5 8 (PMCx1A0's slots).
            width: if family == 0x1a { 8 } else { 6 },
            atom_width: 0,
            hybrid: false,
            described: format!(
                "AMD family {family:#x} counters={counters} core_ext={core_ext} perfmon_v2={v2}{}",
                if zen {
                    ""
                } else {
                    " (no Zen event table: cycles, instructions, mispredicts only)"
                }
            ),
        })
    } else if vendor == INTEL_VENDOR && __cpuid(0).eax >= 0xa {
        let leaf = __cpuid_count(0xa, 0);
        let version = leaf.eax & 0xff;
        let counters = (((leaf.eax >> 8) & 0xff) as usize).min(MAX_COUNTERS);
        let known = ((leaf.eax >> 24) & 0xff) as usize;
        if version == 0 || counters == 0 {
            return None;
        }
        // EBX bit set: the architectural event is unavailable.
        let available = |bit: usize| bit < known && leaf.ebx & (1 << bit) == 0;
        let mut fixed = Vec::new();
        for (bit, event) in [(0, CYCLES_INTEL), (1, INSTRUCTIONS_INTEL)] {
            if available(bit) {
                fixed.push(event);
            }
        }
        // The front-end events first, so a core with 4 counters runs them
        // in its first sets; the architectural ones after.
        let (family, model) = family_model();
        let slots =
            family == 6 && (SLOTS_MODELS.contains(&model) || SLOTS_MODELS_MORE.contains(&model));
        let core = family == 6 && (slots || SKYLAKE_MODELS.contains(&model));
        let hybrid = core && __cpuid_count(7, 0).edx & (1 << 15) != 0;
        let mut rotating = Vec::new();
        if core {
            rotating.push(FRONT_END_SLOTS_INTEL);
            if slots {
                rotating.push(SLOTS_INTEL);
            }
            rotating.extend([ICACHE_STALLS_INTEL, ICACHE_MISSES_INTEL]);
        }
        for (bit, event) in [(6, MISPREDICTS_INTEL), (4, LLC_MISSES_INTEL)] {
            if available(bit) {
                rotating.push(event);
            }
        }
        let table = match (core, slots) {
            (true, true) => {
                "; front end: feslot=9Ch/01h slots=A4h/01h icstall=80h/04h icmiss=24h/E4h (L2 code reads)"
            }
            (true, false) => {
                "; front end: feslot=9Ch/01h icstall=80h/04h icmiss=24h/E4h (L2 code reads), 4 slots a cycle"
            }
            _ => "; no front-end event table for this model: architectural events only",
        };
        Some(Layout {
            counters,
            select: 0x186,
            stride: 1,
            global: (version >= 2).then(|| (0x38f, (1u64 << counters) - 1)),
            amd: false,
            fixed,
            rotating,
            described: format!(
                "Intel family {family} model {model:#x} perfmon v{version} counters={counters}{table}{}",
                if hybrid {
                    "; hybrid: Atom cores count feslot=71h/00h icmiss=80h/02h (no slots, icstall), 5 slots a cycle, apart"
                } else {
                    ""
                }
            ),
            width: if slots { 0 } else { SKYLAKE_WIDTH },
            atom_width: ATOM_WIDTH,
            hybrid,
        })
    } else {
        None
    }
}

impl Layout {
    fn per_set(&self) -> usize {
        self.counters.saturating_sub(self.fixed.len()).max(1)
    }

    fn sets(&self) -> usize {
        self.rotating
            .len()
            .div_ceil(self.per_set())
            .clamp(1, MAX_SETS)
    }

    /// The events counter by counter while set `set` runs.
    fn events(&self, set: usize) -> Vec<Event> {
        let per = self.per_set();
        let mut events: Vec<Event> = self.fixed.clone();
        events.extend(self.rotating.iter().skip(set * per).take(per));
        events.truncate(self.counters);
        events
    }

    /// Counter `counter`'s count register.
    fn counter(&self, counter: usize) -> u32 {
        let counter = counter as u32;
        match (self.amd, self.stride) {
            (true, 2) => self.select + 1 + 2 * counter,
            (true, _) => 0xc001_0004 + counter,
            (false, _) => 0xc1 + counter,
        }
    }

    /// The register that clears counters' overflow status, where there is
    /// one: AMD's PerfCntrGlobalStatusClr, Intel's IA32_PERF_GLOBAL_OVF_CTRL.
    fn overflow_clear(&self) -> Option<u32> {
        self.global
            .map(|_| if self.amd { 0xc000_0302 } else { 0x390 })
    }

    /// Event select of `event` on this core, an Atom core of a hybrid part
    /// when `atom`: none where it has no encoding there.
    fn select_on(&self, event: Event, atom: bool) -> Option<u64> {
        match (atom && self.hybrid, event.atom) {
            (false, _) => Some(self.select(event)),
            (true, Some((code, umask))) => Some(self.select(Event {
                code,
                umask,
                ..event
            })),
            (true, None) => None,
        }
    }

    fn select(&self, event: Event) -> u64 {
        const OS: u64 = 1 << 17;
        const USR: u64 = 1 << 16;
        const ENABLE: u64 = 1 << 22;
        let code = u64::from(event.code);
        let high = if self.amd { (code >> 8 & 0xf) << 32 } else { 0 };
        (code & 0xff) | u64::from(event.umask) << 8 | OS | USR | ENABLE | high
    }
}

#[derive(Default)]
struct Bucket {
    counts: [AtomicU64; MAX_COUNTERS],
    ticks: AtomicU64,
    sampled: AtomicU64,
}

/// The owner core's own state: only its boundaries and fills touch it.
#[derive(Default)]
struct Own {
    set: usize,
    /// Counters read in the running set.
    read: usize,
    last: u64,
    snap: [u64; MAX_COUNTERS],
    sampling: bool,
    /// Where the sampled interval began, after its reads.
    sample_start: u64,
    /// The TSC before which the core does not start another sampled
    /// interval.
    next_sample: u64,
    /// Ticks between sampled interval starts at least: two reads each.
    gap: u64,
    /// The TSC at a shadow fill's start, and the counters then if the fill
    /// is sampled.
    fill: Option<(u64, Option<[u64; MAX_COUNTERS]>)>,
    /// The TSC at [`start`]: the core's reading budget grows from it.
    started: u64,
    /// TSC ticks spent reading and programming the counters since [`start`].
    spent: u64,
    /// What the last read of the set cost, in TSC ticks.
    cost: u64,
    /// TSC ticks a window lasts at least; 0 reads around sampled intervals.
    window: u64,
    window_start: u64,
    /// Each phase's ticks in the open window (`FILL` stays 0: fills are
    /// inside `nat`).
    window_ticks: [u64; BUCKETS],
}

struct Core {
    pe: usize,
    /// An Intel hybrid part's Atom core: its own encodings, its counts apart.
    atom: bool,
    own: UnsafeCell<Own>,
    buckets: [[Bucket; BUCKETS]; MAX_SETS],
    /// Window mode: every closed window's counts and ticks, per set.
    whole: [Bucket; MAX_SETS],
    /// [`Own::spent`], for the line's measured overhead.
    spent: AtomicU64,
}

// SAFETY: `own` is touched only by the owner core, from its boundaries and
// its page-fault handler, which never interleave; the rest are atomics.
unsafe impl Sync for Core {}

/// Each host processor's core record, once its PE started profiling.
static CORES: [AtomicPtr<Core>; MAX_CORES] =
    [const { AtomicPtr::new(core::ptr::null_mut()) }; MAX_CORES];
/// The event set every core should run.
static SET: AtomicU32 = AtomicU32::new(0);
static SETS: AtomicU32 = AtomicU32::new(1);
/// The longest interval-sampling gap any core uses, in TSC ticks.
static GAP: AtomicU64 = AtomicU64::new(0);
/// The longest window any core uses, in TSC ticks; 0 while every core
/// samples intervals.
static WINDOW: AtomicU64 = AtomicU64::new(0);
static DESCRIBED: AtomicBool = AtomicBool::new(false);
/// Counters whose event select some core refused, bit `set * MAX_COUNTERS +
/// counter`: their fields are left out of the line.
static REFUSED: AtomicU32 = AtomicU32::new(0);
static LAYOUT: AtomicPtr<Layout> = AtomicPtr::new(core::ptr::null_mut());

fn shared_layout() -> Option<&'static Layout> {
    let pointer = LAYOUT.load(Relaxed);
    // SAFETY: published once, leaked, never written after.
    (!pointer.is_null()).then(|| unsafe { &*pointer })
}

fn core() -> Option<&'static Core> {
    let pointer = CORES.get(super::local::processor() as usize)?.load(Relaxed);
    // SAFETY: leaked at `start`, never freed.
    (!pointer.is_null()).then(|| unsafe { &*pointer })
}

/// Program this core's counters for `set`. Says once per event what the
/// core refused.
fn program(layout: &Layout, set: usize, atom: bool) -> usize {
    let events = layout.events(set);
    // The sampled counters interrupt on overflow (`sampling`).
    const INTERRUPT: u64 = 1 << 20;
    for counter in 0..layout.counters {
        let index = layout.select + counter as u32 * layout.stride;
        let interrupt = match counter < SAMPLED && super::sampling::enabled() {
            true => INTERRUPT,
            false => 0,
        };
        let value = events
            .get(counter)
            .and_then(|event| layout.select_on(*event, atom))
            .map_or(0, |select| select | interrupt);
        if !write(index, value) && value != 0 {
            let bit = 1 << (set * MAX_COUNTERS + counter);
            if REFUSED.fetch_or(bit, Relaxed) & bit == 0 {
                output::arguments(format_args!(
                    "PMU event {} refused (#GP at {index:#x}): left out of PES PMU\n",
                    events[counter].name
                ));
            }
        }
    }
    if let Some((index, value)) = layout.global
        && !write(index, value)
    {
        output::arguments(format_args!(
            "PMU global enable {index:#x} refused (#GP); counters may stay off\n"
        ));
    }
    events.len()
}

/// The counters from the first that count cycles while selected: under a
/// hypervisor whose host keeps a counter for itself (a watchdog), the last
/// ones accept a select and stay 0.
fn live_counters(layout: &Layout) -> usize {
    let cycles = layout.select(layout.fixed[0]);
    let index = |counter: usize| layout.select + counter as u32 * layout.stride;
    for counter in 0..layout.counters {
        write(index(counter), cycles);
    }
    if let Some((global, value)) = layout.global {
        write(global, value);
    }
    let before: Vec<u64> = (0..layout.counters).map(|c| rdpmc(c as u32)).collect();
    let started = crate::apic::rdtsc();
    while crate::apic::rdtsc().wrapping_sub(started) < 100_000 {
        core::hint::spin_loop();
    }
    let live = (0..layout.counters)
        .take_while(|&counter| rdpmc(counter as u32) != before[counter])
        .count();
    for counter in 0..layout.counters {
        write(index(counter), 0);
    }
    live
}

/// Under `--profile`, program this host core's counters for PE `pe`.
pub(crate) fn start(pe: usize) {
    let slot = super::local::processor() as usize;
    if slot >= MAX_CORES {
        return;
    }
    let layout = match shared_layout() {
        Some(layout) => layout,
        None => {
            let Some(mut layout) = layout() else {
                if !DESCRIBED.swap(true, Relaxed) {
                    output::arguments(format_args!(
                        "PMU unavailable: no AMD core counters or Intel architectural perfmon; PES PMU left out\n"
                    ));
                }
                return;
            };
            // SAFETY: ring 0; an unknown counter takes #GP, which `fixup`
            // resumes.
            if unsafe { stela_pmu_rdpmc(0) } == 0 {
                let live = live_counters(&layout);
                if live < layout.counters && live > layout.fixed.len() {
                    output::arguments(format_args!(
                        "PMU counters {live}..{} take events but stay 0 (another owner holds them): {live} used\n",
                        layout.counters - 1
                    ));
                    layout.counters = live;
                }
            }
            let leaked = Box::into_raw(Box::new(layout));
            match LAYOUT.compare_exchange(core::ptr::null_mut(), leaked, Relaxed, Relaxed) {
                Ok(_) => {}
                // SAFETY: ours, never published.
                Err(_) => drop(unsafe { Box::from_raw(leaked) }),
            }
            shared_layout().expect("published")
        }
    };
    // SAFETY: ring 0; an unknown counter takes #GP, which `fixup` resumes.
    if unsafe { stela_pmu_rdpmc(0) } != 0 {
        output::arguments(format_args!(
            "PMU pe={pe}: RDPMC refused; PES PMU leaves this PE out\n"
        ));
        return;
    }
    super::sampling::enable(layout.fixed.len());
    let atom = atom_core();
    let read = program(layout, SET.load(Relaxed) as usize % layout.sets(), atom);
    // What reading the set costs: RDPMC may trap under a hypervisor.
    let before = crate::apic::rdtsc();
    for _ in 0..4 {
        for counter in 0..read {
            core::hint::black_box(rdpmc(counter as u32));
        }
    }
    let cost = crate::apic::rdtsc().wrapping_sub(before) / 4;
    let gap = 2 * cost * READ_SHARE;
    let window = if cost > WINDOW_READ_TICKS {
        WINDOW_TICKS.max(cost * READ_SHARE)
    } else {
        0
    };
    if window == 0 {
        GAP.fetch_max(gap, Relaxed);
    } else {
        WINDOW.fetch_max(window, Relaxed);
    }
    SETS.store(layout.sets() as u32, Relaxed);
    if !DESCRIBED.swap(true, Relaxed) {
        let mut sets = String::new();
        for set in 0..layout.sets() {
            let names: Vec<&str> = layout.events(set).iter().map(|event| event.name).collect();
            sets.push_str(&format!(" set{}={}", set + 1, names.join(",")));
        }
        let method = if window == 0 {
            format!("sampling a charged interval at most every {gap} ticks (reads under 0.2%)")
        } else {
            format!(
                "too dear to read around intervals: whole-PE windows of {window} ticks or more (reads under 0.2%), phases from windows one phase holds 90% of"
            )
        };
        output::arguments(format_args!(
            "PMU {}{sets}; one read of the set costs {cost} TSC ticks, {method}\n",
            layout.described
        ));
    }
    super::sampling::start(
        pe,
        [layout.counter(0), layout.counter(1)],
        layout.overflow_clear(),
        cost / read.max(1) as u64,
    );
    let started = crate::apic::rdtsc();
    let own = Own {
        set: SET.load(Relaxed) as usize % layout.sets(),
        read,
        last: started,
        gap,
        window,
        window_start: started,
        snap: read_all(read),
        started,
        // The probe's reads and the first snapshot count against the budget.
        spent: cost * 5,
        cost,
        ..Own::default()
    };
    let record = Box::into_raw(Box::new(Core {
        pe,
        atom,
        own: UnsafeCell::new(own),
        buckets: Default::default(),
        whole: Default::default(),
        spent: AtomicU64::new(cost * 5),
    }));
    CORES[slot].store(record, Relaxed);
}

/// Read the set.
fn read_all(read: usize) -> [u64; MAX_COUNTERS] {
    let mut values = [0; MAX_COUNTERS];
    for (counter, value) in values.iter_mut().enumerate().take(read) {
        *value = super::sampling::read(counter, rdpmc);
    }
    values
}

impl Own {
    /// Whether `reads` more reads of the set at the last measured cost keep
    /// the core's reading time within 1/READ_SHARE of its ticks since
    /// [`start`] at TSC `now`.
    fn affords(&self, now: u64, reads: u64) -> bool {
        let elapsed = now.wrapping_sub(self.started);
        self.spent
            .saturating_add(self.cost.saturating_mul(reads))
            .saturating_mul(READ_SHARE)
            <= elapsed
    }

    /// Charge `ticks` of reading or programming to the core's budget.
    fn charge(&mut self, core: &Core, ticks: u64) {
        self.spent = self.spent.wrapping_add(ticks);
        core.spent.store(self.spent, Relaxed);
    }
}

/// Read the set, timing the read: its cost is the core's next estimate and
/// counts against its budget.
fn read_timed(core: &Core, own: &mut Own) -> [u64; MAX_COUNTERS] {
    let before = crate::apic::rdtsc();
    let values = read_all(own.read);
    let cost = crate::apic::rdtsc().wrapping_sub(before);
    own.cost = cost;
    own.charge(core, cost);
    values
}

/// Program `set`, timing the counter MSR writes against the core's budget.
fn reprogram(core: &Core, own: &mut Own, layout: &Layout, set: usize) {
    let before = crate::apic::rdtsc();
    own.read = program(layout, set, core.atom);
    own.set = set;
    own.charge(core, crate::apic::rdtsc().wrapping_sub(before));
}

/// Add to a counter only its core writes.
#[inline]
fn add(counter: &AtomicU64, value: u64) {
    counter.store(counter.load(Relaxed).wrapping_add(value), Relaxed);
}

/// The counters' 48-bit delta.
fn delta(now: u64, then: u64) -> u64 {
    now.wrapping_sub(then) & ((1 << 48) - 1)
}

/// A charge of the interval ending at TSC `now` to `phase`.
#[inline]
pub(crate) fn boundary(phase: Phase, now: u64) {
    let Some(core) = core() else {
        return;
    };
    // SAFETY: the owner core, outside its fault handler.
    let own = unsafe { &mut *core.own.get() };
    let bucket = &core.buckets[own.set][phase as usize];
    let ticks = now.wrapping_sub(own.last);
    add(&bucket.ticks, ticks);
    if own.window != 0 {
        own.window_ticks[phase as usize] += ticks;
        own.last = now;
        let wanted = SET.load(Relaxed) as usize;
        if wanted != own.set
            && let Some(layout) = shared_layout()
        {
            // The open window counted the old set: drop it.
            reprogram(core, own, layout, wanted);
            open_window(core, own);
        } else if now.wrapping_sub(own.window_start) >= own.window && own.affords(now, 1) {
            // A window the budget cannot close yet stays open: it only
            // grows, and its counts and ticks stay matched.
            close_window(core, own);
        }
        return;
    }
    if own.sampling {
        let values = read_timed(core, own);
        for (counter, value) in values.iter().enumerate().take(own.read) {
            add(&bucket.counts[counter], delta(*value, own.snap[counter]));
        }
        add(&bucket.sampled, now.wrapping_sub(own.sample_start));
        own.sampling = false;
    }
    own.last = now;
    let wanted = SET.load(Relaxed) as usize;
    if wanted != own.set
        && let Some(layout) = shared_layout()
    {
        reprogram(core, own, layout, wanted);
    }
    // A sampled interval reads at both ends.
    if now.wrapping_sub(own.next_sample) < 1 << 63 && own.affords(now, 2) {
        own.next_sample = now.wrapping_add(own.gap);
        own.snap = read_timed(core, own);
        own.sampling = true;
        own.sample_start = crate::apic::rdtsc();
    }
}

fn open_window(core: &Core, own: &mut Own) {
    own.snap = read_timed(core, own);
    own.window_ticks = [0; BUCKETS];
    own.window_start = crate::apic::rdtsc();
}

/// Credit the window that ends now to the whole PE, and to the bucket that
/// holds most of its ticks if one does, then open the next.
fn close_window(core: &Core, own: &mut Own) {
    let values = read_timed(core, own);
    let ticks: u64 = own.window_ticks.iter().sum();
    if ticks == 0 {
        return;
    }
    let mut counts = [0; MAX_COUNTERS];
    for (counter, count) in counts.iter_mut().enumerate().take(own.read) {
        *count = delta(values[counter], own.snap[counter]);
    }
    let credit = |bucket: &Bucket| {
        for (counter, count) in counts.iter().enumerate().take(own.read) {
            add(&bucket.counts[counter], *count);
        }
        add(&bucket.sampled, ticks);
    };
    credit(&core.whole[own.set]);
    if let Some(dominant) = own
        .window_ticks
        .iter()
        .position(|phase| *phase * 10 >= ticks * DOMINANT_TENTHS)
    {
        credit(&core.buckets[own.set][dominant]);
    }
    own.snap = values;
    own.window_ticks = [0; BUCKETS];
    own.window_start = crate::apic::rdtsc();
}

/// Before a shadow page-fault fill: a mark while profiling.
#[inline]
pub(crate) fn fill_begin() {
    let Some(core) = core() else {
        return;
    };
    // SAFETY: the owner core; its boundary code is not running.
    let own = unsafe { &mut *core.own.get() };
    super::sampling::fill(true);
    let now = crate::apic::rdtsc();
    // A fill inside a sampled interval is read at both ends while the
    // budget allows; else the interval's `nat` sample still holds it.
    let values = (own.sampling && own.affords(now, 2)).then(|| read_timed(core, own));
    own.fill = Some((crate::apic::rdtsc(), values));
}

/// After the fill [`fill_begin`] marked.
#[inline]
pub(crate) fn fill_end() {
    let Some(core) = core() else {
        return;
    };
    // SAFETY: as `fill_begin`.
    let own = unsafe { &mut *core.own.get() };
    super::sampling::fill(false);
    let Some((then, before)) = own.fill.take() else {
        return;
    };
    let bucket = &core.buckets[own.set][FILL];
    let ticks = crate::apic::rdtsc().wrapping_sub(then);
    add(&bucket.ticks, ticks);
    if let Some(before) = before {
        let values = read_timed(core, own);
        for (counter, value) in values.iter().enumerate().take(own.read) {
            add(&bucket.counts[counter], delta(*value, before[counter]));
        }
        add(&bucket.sampled, ticks);
    }
}

/// What the line compares against: each core's bucket totals at the last
/// beat, per set.
pub(crate) struct Seen {
    totals: Vec<[[[u64; MAX_COUNTERS + 2]; BUCKETS + 1]; MAX_SETS]>,
    /// Each core's reading and programming ticks at the last beat.
    spent: Vec<u64>,
    /// The `PES WHERE` lines' ([`super::sampling::report`]).
    sampled: super::sampling::Seen,
    /// The open window's scaled counts ([`window_report`]), by core kind
    /// (0, an Intel hybrid part's Atom cores 1), set and bucket, and the
    /// beats each set ran in it.
    window: [[[[u64; MAX_COUNTERS]; BUCKETS + 1]; MAX_SETS]; 2],
    window_ran: [[[bool; BUCKETS + 1]; MAX_SETS]; 2],
    window_beats: [u32; MAX_SETS],
}

impl Seen {
    pub(crate) const fn new() -> Self {
        Self {
            totals: Vec::new(),
            spent: Vec::new(),
            sampled: super::sampling::Seen::new(),
            window: [[[[0; MAX_COUNTERS]; BUCKETS + 1]; MAX_SETS]; 2],
            window_ran: [[[false; BUCKETS + 1]; MAX_SETS]; 2],
            window_beats: [0; MAX_SETS],
        }
    }
}

/// `PES PMU WINDOW w=W beats=B1/B2/..`, then per core kind (`all`, or an
/// Intel hybrid part's `core` and `atom`) and set that ran in the window,
/// `setS:` and for the native code (`nat`) and the whole PE (`pe`, every
/// phase) its ratios: `ipc`, each slot event's share of the issue slots
/// (`feslot%`, `beslot%`: of the `slots` event where one counts, else of
/// the width times the cycles), `badspec%` (dispatched less retired ops,
/// AMD) and `retops%`, each stall event's share of the cycles
/// (`icstall%`), and every other event per thousand instructions
/// (`icmiss_pki`, `brmisp_pki`, ...): what the counters say of window `w`,
/// the beats since the last mark, with the sets rotating through it.
pub(crate) fn window_report(seen: &mut Seen, window: u32) {
    let Some(layout) = shared_layout() else {
        return;
    };
    let sets = layout.sets();
    let beats: Vec<String> = seen.window_beats[..sets]
        .iter()
        .map(|beats| format!("{beats}"))
        .collect();
    let mut line = format!("PES PMU WINDOW w={window} beats={}", beats.join("/"));
    let kinds: &[(usize, &str)] = match layout.hybrid {
        true => &[(0, "core"), (1, "atom")],
        false => &[(0, "all")],
    };
    for &(kind, kind_name) in kinds {
        line.push_str(&format!(" {kind_name}:"));
        let width = match kind {
            1 => layout.atom_width,
            _ => layout.width,
        };
        for set in 0..sets {
            if seen.window_beats[set] == 0 {
                continue;
            }
            let events = layout.events(set);
            line.push_str(&format!(" set{}:", set + 1));
            let phases: Vec<usize> = (0..Phase::COUNT).collect();
            // The whole PE: its window bucket, or every phase's.
            let whole = |counter: usize| match seen.window_ran[kind][set][BUCKETS] {
                true => seen.window[kind][set][BUCKETS][counter],
                false => phases
                    .iter()
                    .map(|&bucket| seen.window[kind][set][bucket][counter])
                    .sum(),
            };
            for (name, values) in [
                (
                    "nat",
                    core::array::from_fn::<u64, MAX_COUNTERS, _>(|counter| {
                        seen.window[kind][set][Phase::Native as usize][counter]
                    }),
                ),
                ("pe", core::array::from_fn(whole)),
            ] {
                line.push_str(&format!(" {name}="));
                line.push_str(&ratios(&events, &values, width));
            }
        }
    }
    line.push('\n');
    output::arguments(format_args!("{line}"));
    seen.window = [[[[0; MAX_COUNTERS]; BUCKETS + 1]; MAX_SETS]; 2];
    seen.window_ran = [[[false; BUCKETS + 1]; MAX_SETS]; 2];
    seen.window_beats = [0; MAX_SETS];
}

/// A window line's ratios of one bucket's counts ([`window_report`]),
/// comma-separated: `ipc=..,feslot%=..,...`, `-` without cycles.
fn ratios(events: &[Event], values: &[u64; MAX_COUNTERS], width: u64) -> String {
    let find = |name: &str| {
        events
            .iter()
            .position(|event| event.name == name)
            .map(|counter| values[counter])
    };
    let (Some(cycles), Some(instructions)) = (find("cyc"), find("ins")) else {
        return String::from("-");
    };
    if cycles == 0 {
        return String::from("-");
    }
    let slots = find("slots")
        .filter(|&slots| slots != 0)
        .unwrap_or(cycles.saturating_mul(width));
    let fixed = |value: u64, of: u64, scale: u64| {
        let tenths = (u128::from(value) * u128::from(scale) * 10)
            .checked_div(u128::from(of))
            .unwrap_or(0);
        format!("{}.{}", tenths / 10, tenths % 10)
    };
    let mut words = alloc::vec![format!(
        "ipc={}.{:02}",
        instructions / cycles,
        (u128::from(instructions % cycles) * 100 / u128::from(cycles))
    )];
    for (counter, event) in events.iter().enumerate() {
        let value = values[counter];
        match event.name {
            "cyc" | "ins" | "slots" | "disp" => {}
            "retops" => {
                words.push(format!("retops%={}", fixed(value, slots, 100)));
                if let Some(dispatched) = find("disp") {
                    words.push(format!(
                        "badspec%={}",
                        fixed(dispatched.saturating_sub(value), slots, 100)
                    ));
                }
            }
            name if name.ends_with("slot") => {
                words.push(format!("{name}%={}", fixed(value, slots, 100)));
            }
            name if name.ends_with("stall") => {
                words.push(format!("{name}%={}", fixed(value, cycles, 100)));
            }
            name => words.push(format!("{name}_pki={}", fixed(value, instructions, 1000))),
        }
    }
    words.join(",")
}

fn totals(core: &Core) -> [[[u64; MAX_COUNTERS + 2]; BUCKETS + 1]; MAX_SETS] {
    let mut all = [[[0; MAX_COUNTERS + 2]; BUCKETS + 1]; MAX_SETS];
    for (set, (buckets, whole)) in core.buckets.iter().zip(&core.whole).enumerate() {
        for (bucket, values) in buckets.iter().chain([whole]).enumerate() {
            let entry = &mut all[set][bucket];
            for (slot, count) in entry.iter_mut().zip(&values.counts) {
                *slot = count.load(Relaxed);
            }
            entry[MAX_COUNTERS] = values.ticks.load(Relaxed);
            entry[MAX_COUNTERS + 1] = values.sampled.load(Relaxed);
        }
    }
    all
}

/// The `PES PMU` line for the beat that ends now, then the next set.
///
/// `PES PMU set=S/N events=E1,E2,... method=M units=cyc,ins:M other:K`,
/// then for each PE `pe:` and for each bucket that ran (`nat`, `fill`,
/// `fetch`, `exit`, `sch`, `dev`) `name=v1/v2/...` in the order of
/// `events`, then `all:` the machine's sums. Counts are scaled by each
/// bucket's TSC ticks over its sampled ticks; cycles and instructions in
/// millions, the other events in thousands. `nat` includes `fill`. A
/// bucket with no sampled interval shows `-`.
///
/// `overhead=`: the TSC ticks the PEs spent reading and programming the
/// counters in the beat, each read and write timed, over their charged
/// ticks.
///
/// `method=interval:GKticks`: each bucket from the intervals read at both
/// ends. `method=window:NMticks` (RDPMC trapped): a leading `pe=` is the
/// whole PE from every window, unbiased; each phase bucket comes only from
/// the windows that phase held 90% of, so a phase that never dominates a
/// window shows `-` (`fill` always does). Where no window measured `nat`
/// alone, `nat~=C/I` is the whole PE's cycles and instructions times the
/// shares of each native code took in the PE's `PES WHERE` samples.
pub(crate) fn report(seen: &mut Seen) {
    let Some(layout) = shared_layout() else {
        return;
    };
    let sets = layout.sets();
    let set = SET.load(Relaxed) as usize % sets;
    let events = layout.events(set);
    let mut cores: Vec<&Core> = CORES
        .iter()
        .filter_map(|slot| {
            let pointer = slot.load(Relaxed);
            // SAFETY: leaked at `start`, never freed.
            (!pointer.is_null()).then(|| unsafe { &*pointer })
        })
        .collect();
    cores.sort_by_key(|core| core.pe);
    let pes = cores.last().map_or(0, |core| core.pe + 1);
    if seen.totals.len() < pes {
        seen.totals
            .resize(pes, [[[0; MAX_COUNTERS + 2]; BUCKETS + 1]; MAX_SETS]);
        seen.spent.resize(pes, 0);
    }
    // What the beat's reads cost, as timed, over every PE's charged ticks
    // (all sets; `fill` is inside `nat`).
    let (mut spent, mut ticks) = (0u64, 0u64);
    for core in &cores {
        let now = core.spent.load(Relaxed);
        spent += now.wrapping_sub(seen.spent[core.pe]);
        seen.spent[core.pe] = now;
        let before = &seen.totals[core.pe];
        for (set, buckets) in core.buckets.iter().enumerate() {
            for (bucket, values) in buckets.iter().enumerate().take(Phase::COUNT) {
                ticks += values
                    .ticks
                    .load(Relaxed)
                    .wrapping_sub(before[set][bucket][MAX_COUNTERS]);
            }
        }
    }
    let overhead = (u128::from(spent) * 10_000)
        .checked_div(u128::from(ticks))
        .unwrap_or(0) as u64;
    let refused = REFUSED.load(Relaxed) >> (set * MAX_COUNTERS);
    let kept = |counter: usize| refused & (1 << counter) == 0;
    let names: Vec<&str> = events
        .iter()
        .enumerate()
        .filter(|(counter, _)| kept(*counter))
        .map(|(_, event)| event.name)
        .collect();
    let window = WINDOW.load(Relaxed);
    let method = if window == 0 {
        format!("interval:{}Kticks", GAP.load(Relaxed).div_ceil(1_000))
    } else {
        format!("window:{}Mticks", window.div_ceil(1_000_000))
    };
    let mut line = format!(
        "PES PMU set={}/{sets} events={} method={method} overhead={}.{:02}% units=cyc,ins:M other:K",
        set + 1,
        names.join(","),
        overhead / 100,
        overhead % 100,
    );
    let mut machine = [[0u64; MAX_COUNTERS]; BUCKETS + 1];
    let mut ran = [false; BUCKETS + 1];
    let format_bucket = |line: &mut String, name: &str, values: &[u64]| {
        line.push_str(&format!(" {name}="));
        let mut first = true;
        for (counter, value) in values.iter().enumerate().take(events.len()) {
            if !kept(counter) {
                continue;
            }
            if !core::mem::take(&mut first) {
                line.push('/');
            }
            let unit = if counter < layout.fixed.len() {
                1_000_000
            } else {
                1_000
            };
            line.push_str(&format!("{}", value / unit));
        }
    };
    // Window mode leads with the whole PE (`pe`), read once a window.
    let order: Vec<usize> = (window != 0)
        .then_some(BUCKETS)
        .into_iter()
        .chain(REPORT_ORDER)
        .collect();
    let name = |bucket: usize| BUCKET_NAMES.get(bucket).copied().unwrap_or("pe");
    for core in &cores {
        let now = totals(core);
        let before = &seen.totals[core.pe];
        let elapsed = |bucket: usize| {
            now[set][bucket][MAX_COUNTERS].wrapping_sub(before[set][bucket][MAX_COUNTERS])
        };
        line.push_str(&format!(" {}:", core.pe));
        let mut whole: Option<[u64; MAX_COUNTERS]> = None;
        for &bucket in &order {
            // The whole PE ran every phase's ticks.
            let ticks = if bucket == BUCKETS {
                (0..Phase::COUNT).map(elapsed).sum()
            } else {
                elapsed(bucket)
            };
            let (now, before) = (&now[set][bucket], &before[set][bucket]);
            let sampled = now[MAX_COUNTERS + 1].wrapping_sub(before[MAX_COUNTERS + 1]);
            if ticks == 0 {
                continue;
            }
            if sampled == 0 {
                let estimate = (bucket == Phase::Native as usize)
                    .then_some(whole)
                    .flatten()
                    .zip(super::sampling::native_share(&seen.sampled, core.pe));
                match estimate {
                    Some((whole, share)) => {
                        let [cycles, instructions] = [0, 1].map(|counter: usize| {
                            (u128::from(whole[counter]) * u128::from(share[counter]) / 1_000_000)
                                as u64
                                / 1_000_000
                        });
                        line.push_str(&format!(" nat~={cycles}/{instructions}"));
                    }
                    None => line.push_str(&format!(" {}=-", name(bucket))),
                }
                continue;
            }
            let mut scaled = [0u64; MAX_COUNTERS];
            for counter in 0..events.len() {
                let count = now[counter].wrapping_sub(before[counter]);
                scaled[counter] =
                    (u128::from(count) * u128::from(ticks) / u128::from(sampled)) as u64;
                machine[bucket][counter] += scaled[counter];
                seen.window[usize::from(core.atom)][set][bucket][counter] += scaled[counter];
            }
            ran[bucket] = true;
            seen.window_ran[usize::from(core.atom)][set][bucket] = true;
            if bucket == BUCKETS {
                whole = Some(scaled);
            }
            format_bucket(&mut line, name(bucket), &scaled);
        }
        seen.totals[core.pe] = now;
    }
    seen.window_beats[set] += 1;
    line.push_str(" all:");
    for &bucket in &order {
        if ran[bucket] {
            format_bucket(&mut line, name(bucket), &machine[bucket]);
        }
    }
    line.push('\n');
    output::arguments(format_args!("{line}"));
    super::sampling::report(&mut seen.sampled);
    if sets > 1 {
        SET.store(((set + 1) % sets) as u32, Relaxed);
    }
}
