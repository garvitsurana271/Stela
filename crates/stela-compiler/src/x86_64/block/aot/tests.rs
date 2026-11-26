//! Hosted runs of units against the JIT: a function is run from its entry
//! in a child process, once through JIT blocks and once through units, each
//! translation entered as the engine enters a block and every exit inside
//! the function compiled and entered next, until the function leaves (a
//! call, return, register branch or exit to elsewhere). Both runs start
//! from the same state over the same arena at a fixed address, and must
//! end in the same state: registers, NZCV, vectors, retired count, exit and
//! arena bytes. Under the x86 trap flag each run also counts the host
//! instructions its translations execute.
//!
//! `AOT_IN=<file of pc weight hex-bytes lines> cargo test -p stela-compiler
//! --release -- --ignored --exact x86_64::block::aot::tests::units_of_real_functions --nocapture`
//! runs the functions `stela-aot analyze --extract` wrote, weights them by
//! their samples and prints the totals.
use crate::{
    GuestAddress, GuestState, Options, TargetCpu,
    block::{BlockProfile, CompilationPlan, CompiledBlock, ExitKind, Features},
    x86_64::{MemoryChecks, NativeSystem, RangeCheck, chain, pointer},
};
use alloc::{boxed::Box, collections::BTreeMap, vec, vec::Vec};
use core::ffi::c_void;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

const SIGTRAP: i32 = 5;
const SA_SIGINFO: i32 = 4;
const GREGS: usize = 40;
const RIP: usize = 16;
const EFLAGS: usize = 17;
const TRAP_FLAG: u64 = 0x100;
/// The arena's fixed address and size: both runs see the same pointers.
const ARENA_ADDRESS: usize = 0x10_0000_0000;
const ARENA: usize = 8 << 20;
const CIPHER: u32 = 0x400;
/// Translations entered at most, and host instructions counted at most,
/// before a run gives up.
const MAX_RUNS: u32 = 4096;
const MAX_HOST: u64 = 1 << 24;
/// The host instructions of a link between two translations
/// (`chain::link_body`, unpolled): what the engine adds per transfer that
/// a hosted run, whose exits return, does not execute.
pub(super) const LINK_INSTRUCTIONS: u64 = 6;

#[repr(C)]
struct SigAction {
    handler: usize,
    mask: [u64; 16],
    flags: i32,
    restorer: usize,
}

unsafe extern "C" {
    fn sigaction(signal: i32, action: *const SigAction, old: *mut SigAction) -> i32;
    fn fork() -> i32;
    fn pipe(fds: *mut i32) -> i32;
    fn read(fd: i32, bytes: *mut c_void, count: usize) -> isize;
    fn write(fd: i32, bytes: *const c_void, count: usize) -> isize;
    fn close(fd: i32) -> i32;
    fn waitpid(pid: i32, status: *mut i32, options: i32) -> i32;
    fn alarm(seconds: u32) -> u32;
    fn _exit(status: i32) -> !;
    fn mmap(
        address: *mut c_void,
        bytes: usize,
        protection: i32,
        flags: i32,
        fd: i32,
        offset: i64,
    ) -> *mut c_void;
    fn mprotect(address: *mut c_void, bytes: usize, protection: i32) -> i32;
}

static BASE: AtomicUsize = AtomicUsize::new(0);
static LENGTH: AtomicUsize = AtomicUsize::new(0);
static COUNT: AtomicU64 = AtomicU64::new(0);
/// The child's end of its pipe, and the translation it is running, for
/// [`crashed`].
static PIPE: core::sync::atomic::AtomicI32 = core::sync::atomic::AtomicI32::new(-1);
static UNIT_PC: AtomicU64 = AtomicU64::new(0);
/// What a crashed child writes first in place of its outcome.
const CRASH_MARK: u64 = 0xdead_fa17_c0de_0001;

std::thread_local! {
    /// The last crash a run on this thread reported: the signal, the
    /// translation's entry PC and the host offset in its code.
    pub(super) static LAST_CRASH: core::cell::Cell<Option<(u64, u64, u64)>> =
        const { core::cell::Cell::new(None) };
}

/// A fault in the child: report where, and exit.
unsafe extern "C" fn crashed(signal: i32, _: *mut c_void, context: *mut c_void) {
    // SAFETY: Linux passes a ucontext_t; write and _exit are
    // async-signal-safe.
    unsafe {
        let gregs = context.cast::<u8>().add(GREGS).cast::<u64>();
        let rip = *gregs.add(RIP);
        let report = [
            CRASH_MARK,
            signal as u64,
            UNIT_PC.load(Ordering::Relaxed),
            rip.wrapping_sub(BASE.load(Ordering::Relaxed) as u64),
        ];
        write(PIPE.load(Ordering::Relaxed), report.as_ptr().cast(), 32);
        _exit(3);
    }
}

unsafe extern "C" fn step(_: i32, _: *mut c_void, context: *mut c_void) {
    // SAFETY: Linux passes a ucontext_t whose gregs hold the interrupted
    // RIP and EFLAGS; the child is single threaded.
    unsafe {
        let gregs = context.cast::<u8>().add(GREGS).cast::<u64>();
        let rip = *gregs.add(RIP) as usize;
        let base = BASE.load(Ordering::Relaxed);
        if (base..base + LENGTH.load(Ordering::Relaxed)).contains(&rip) {
            let count = COUNT.fetch_add(1, Ordering::Relaxed);
            if count >= MAX_HOST {
                *gregs.add(EFLAGS) &= !TRAP_FLAG;
            }
        }
    }
}

#[repr(C)]
struct Pair(u64, u64);

extern "sysv64" fn authenticated(_: *mut u8, _: u32, value: u64, _: u64) -> Pair {
    Pair(1, value)
}

/// The engine's system-mode options, as `executed_paths` has them.
pub(super) fn options() -> Options {
    let checks = MemoryChecks::system(true, false, true, false)
        .with_range(RangeCheck::four_level(4).one_sided(true));
    let system = NativeSystem {
        exclusive: true,
        pointer: Some(pointer::inline::Cells {
            tags: 0x40,
            strips: 0x80,
            routine: 0x1c0,
            cipher: TargetCpu::detect()
                .supports(TargetCpu::Aes)
                .then_some(CIPHER),
            mix: false,
        }),
        self_loops: true,
        regions: true,
        ..NativeSystem::NONE
    };
    Options::new(
        BlockProfile::new(Features::SYSTEM_MODE),
        TargetCpu::detect(),
    )
    .with_memory_checks(checks)
    .with_native_system(system)
}

fn map(address: usize, bytes: usize, fixed: bool) -> *mut u8 {
    // MAP_PRIVATE | MAP_ANONYMOUS, and MAP_FIXED_NOREPLACE for the arena.
    let flags = 0x22 | if fixed { 0x10_0000 } else { 0 };
    // SAFETY: private anonymous read-write memory.
    let page = unsafe { mmap(address as *mut c_void, bytes, 3, flags, -1, 0) };
    assert_ne!(page as usize, usize::MAX);
    if fixed {
        assert_eq!(page as usize, address, "the arena's address is free");
    }
    page.cast()
}

/// Registers and memory for a run, as `executed_paths` prepares them but
/// over an arena at [`ARENA_ADDRESS`].
fn prepare(state: &mut GuestState) {
    let arena = map(ARENA_ADDRESS, ARENA, true);
    let middle = arena as u64 + (ARENA as u64) / 2;
    // SAFETY: the arena is mapped read-write.
    unsafe {
        let words = arena.cast::<u64>();
        for index in 0..ARENA / 8 {
            *words.add(index) = middle + ((index as u64 * 8) & 0xffff);
        }
    }
    for (index, register) in state.x.iter_mut().enumerate() {
        *register = middle + 0x100 * index as u64;
    }
    state.sp = middle + 0x10_0000;
    state.fpsr = 0x10;
    let anchors = map(0, 8 << 16, false);
    let fault = map(0, 4096, false);
    state.unreachable = fault as u64;
    state.lower_limit = crate::Limit(u64::MAX);
    state.account = chain::Account::new(chain::MAX_BUDGET).0;
    let raw = (state as *mut GuestState).cast::<u8>();
    let anchor_offset = core::mem::offset_of!(GuestState, anchors);
    // SAFETY: the anchors are eight words in the state; each area is mapped.
    unsafe {
        for index in 0..8 {
            let area = anchors.add(index << 16);
            raw.add(anchor_offset + 8 * index)
                .cast::<u64>()
                .write(area as u64);
        }
        for key in 0..pointer::TAG_WORDS {
            anchors
                .add(3 << 16)
                .add(0x40 + 8 * key)
                .cast::<u64>()
                .write(pointer::tag(1, key as u8, 0));
        }
        let mut cipher = pointer::cipher::Cipher::default();
        cipher.set_ranges(
            [[false, true].map(|upper| pointer::cipher::Range::new(0, upper, false)); 2],
        );
        anchors
            .add(3 << 16)
            .add(CIPHER as usize)
            .cast::<pointer::cipher::Cipher>()
            .write(cipher);
        anchors.add(3 << 16).add(0x1c0).cast::<u64>().write(
            (authenticated as extern "sysv64" fn(*mut u8, u32, u64, u64) -> Pair) as usize as u64,
        );
    }
}

/// Which translator a run uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Mode {
    Jit,
    Aot,
}

fn compile(mode: Mode, source: &[u8], pc: u64, options: &Options) -> Option<CompiledBlock> {
    match mode {
        Mode::Jit => CompilationPlan::new(source, GuestAddress(pc), options)
            .ok()?
            .compile()
            .ok(),
        // Where no unit starts (its first word is the JIT's), the JIT's
        // block, as the engine would fall back.
        Mode::Aot => crate::block::aot::compile_unit(source, GuestAddress(pc), options)
            .ok()
            .or_else(|| compile(Mode::Jit, source, pc, options)),
    }
}

/// What a run of a function ended with. The first fields are its guest
/// state, which both translators must agree on; the last are its costs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
pub(super) struct Outcome {
    pub x: [u64; 31],
    pub sp: u64,
    pub nzcv: u64,
    pub v: [[u64; 2]; 32],
    pub retired: u64,
    pub exit: u64,
    pub kind: u64,
    pub memory: u64,
    /// It left the function before [`MAX_RUNS`] or [`MAX_HOST`].
    pub finished: u64,
    // Costs.
    pub host: u64,
    pub runs: u64,
    pub bytes: u64,
}

impl Outcome {
    /// The guest state alone.
    pub(super) fn guest(&self) -> Self {
        Self {
            host: 0,
            runs: 0,
            bytes: 0,
            ..*self
        }
    }
}

fn digest(bytes: &[u8]) -> u64 {
    // FNV-1a.
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3)
    })
}

/// Run the function of `source` at `entry` from its entry with `mode`'s
/// translations, in a child; `setup` changes the prepared state first.
pub(super) fn run(
    mode: Mode,
    source: &[u8],
    entry: u64,
    setup: impl FnOnce(&mut GuestState),
) -> Option<Outcome> {
    let mut fds = [0i32; 2];
    // SAFETY: plain POSIX calls; the child only runs translations and
    // writes its outcome.
    unsafe {
        assert_eq!(pipe(fds.as_mut_ptr()), 0);
        let pid = fork();
        if pid == 0 {
            close(fds[0]);
            alarm(20);
            let action = SigAction {
                handler: step as unsafe extern "C" fn(i32, *mut c_void, *mut c_void) as usize,
                mask: [0; 16],
                flags: SA_SIGINFO,
                restorer: 0,
            };
            sigaction(SIGTRAP, &action, core::ptr::null_mut());
            PIPE.store(fds[1], Ordering::Relaxed);
            let crash = SigAction {
                handler: crashed as unsafe extern "C" fn(i32, *mut c_void, *mut c_void) as usize,
                ..action
            };
            for signal in [4, 7, 8, 11] {
                sigaction(signal, &crash, core::ptr::null_mut());
            }
            let state = Box::leak(Box::new(GuestState::default()));
            prepare(state);
            setup(state);
            let outcome = child(mode, source, entry, state);
            write(
                fds[1],
                (&raw const outcome).cast(),
                core::mem::size_of::<Outcome>(),
            );
            _exit(0);
        }
        close(fds[1]);
        let mut outcome = Outcome::default();
        let mut bytes = vec![0u8; core::mem::size_of::<Outcome>()];
        let mut got = 0;
        while got < bytes.len() {
            let n = read(fds[0], bytes[got..].as_mut_ptr().cast(), bytes.len() - got);
            if n <= 0 {
                break;
            }
            got += n as usize;
        }
        close(fds[0]);
        let mut status = 0;
        waitpid(pid, &mut status, 0);
        if got >= 32 && u64::from_le_bytes(bytes[..8].try_into().unwrap()) == CRASH_MARK {
            let word = |at: usize| u64::from_le_bytes(bytes[8 * at..8 * at + 8].try_into().unwrap());
            LAST_CRASH.set(Some((word(1), word(2), word(3))));
            return None;
        }
        LAST_CRASH.set(None);
        if status != 0 || got != bytes.len() {
            return None;
        }
        core::ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            (&raw mut outcome).cast::<u8>(),
            bytes.len(),
        );
        Some(outcome)
    }
}

/// A translation installed in the child: its entered code and the rows
/// its exits count.
struct Installed {
    code: *mut u8,
    body: usize,
    length: usize,
    rows: u64,
}

fn child(mode: Mode, source: &[u8], entry: u64, state: &mut GuestState) -> Outcome {
    let options = options();
    let end = entry + source.len() as u64;
    let mut installed: BTreeMap<u64, Installed> = BTreeMap::new();
    let mut outcome = Outcome::default();
    let mut pc = entry;
    let mut retired = 0u64;
    loop {
        if outcome.runs >= u64::from(MAX_RUNS) || COUNT.load(Ordering::Relaxed) >= MAX_HOST {
            break;
        }
        if !installed.contains_key(&pc) {
            let Some(compiled) = compile(mode, &source[(pc - entry) as usize..], pc, &options)
            else {
                // A word neither translator takes: the run stops at it.
                outcome.exit = pc;
                outcome.kind = u64::MAX;
                outcome.finished = 1;
                break;
            };
            let body = &compiled.native.x86_64_code;
            outcome.bytes += body.len() as u64;
            let entered = crate::x86_64::pinned::entered(body);
            let length = entered.len().next_multiple_of(4096) + 4096;
            let code = map(0, length, false);
            // SAFETY: the mapping holds the code and a RET after it.
            unsafe {
                core::ptr::copy_nonoverlapping(entered.as_ptr(), code, entered.len());
                code.add(entered.len()).write(0xc3);
                mprotect(code.cast(), length, 5);
            }
            installed.insert(
                pc,
                Installed {
                    code,
                    body: entered.len() - body.len(),
                    length: body.len(),
                    rows: compiled.native.source_map.len() as u64,
                },
            );
        }
        let unit = &installed[&pc];
        UNIT_PC.store(pc, Ordering::Relaxed);
        BASE.store(unit.code as usize + unit.body, Ordering::Relaxed);
        LENGTH.store(unit.length, Ordering::Relaxed);
        // SAFETY: the code is the engine's entry around a translation,
        // run with the state it was prepared for.
        let Pair(exit, word) = unsafe {
            let run: unsafe extern "sysv64" fn(*mut GuestState) -> Pair =
                core::mem::transmute(unit.code);
            core::arch::asm!("pushfq", "or qword ptr [rsp], 0x100", "popfq");
            let pair = run(state);
            core::arch::asm!("pushfq", "and qword ptr [rsp], -257", "popfq");
            pair
        };
        outcome.runs += 1;
        let (kind, skipped) = chain::ExitWord::split(word);
        let kind = ExitKind::try_from(kind).ok();
        let counted = match kind {
            Some(
                ExitKind::Fallthrough
                | ExitKind::Direct
                | ExitKind::Indirect
                | ExitKind::Call
                | ExitKind::Return,
            ) => unit.rows.wrapping_sub(u64::from(skipped)),
            _ => exit.wrapping_sub(pc) / 4,
        };
        retired = retired
            .wrapping_add(counted)
            .wrapping_add(chain::Account(state.account).retired())
            .wrapping_add(state.loop_retired);
        state.account = chain::Account::new(chain::MAX_BUDGET).0;
        state.loop_retired = 0;
        let within = (entry..end).contains(&exit);
        if !(matches!(kind, Some(ExitKind::Direct | ExitKind::Fallthrough)) && within) {
            outcome.exit = exit;
            outcome.kind = kind.map_or(u64::MAX - 1, |kind| kind as u64);
            outcome.finished = 1;
            break;
        }
        pc = exit;
    }
    outcome.x = state.x;
    outcome.sp = state.sp;
    outcome.nzcv = u64::from(crate::x86_64::flag_word::arm(state.nzcv));
    outcome.v = state.v;
    outcome.retired = retired;
    outcome.host = COUNT.load(Ordering::Relaxed);
    // SAFETY: the arena is mapped for the child's life.
    outcome.memory = digest(unsafe {
        core::slice::from_raw_parts(ARENA_ADDRESS as *const u8, ARENA)
    });
    outcome
}

fn bytes(words: &[u32]) -> Vec<u8> {
    words.iter().flat_map(|word| word.to_le_bytes()).collect()
}

/// Run `words` at `entry` with both translators from the same state and
/// require the same guest outcome; both outcomes.
fn agree(words: &[u32], setup: impl Fn(&mut GuestState) + Copy) -> (Outcome, Outcome) {
    let source = bytes(words);
    let entry = 0x10_0040_0000u64 - 0x1000;
    let jit = run(Mode::Jit, &source, entry, setup).expect("the JIT run completes");
    let aot = run(Mode::Aot, &source, entry, setup).expect("the AOT run completes");
    assert_eq!(jit.finished, 1, "the JIT run finishes: {jit:x?}");
    assert_eq!(jit.guest(), aot.guest(), "JIT {jit:x?}\nAOT {aot:x?}");
    (jit, aot)
}

/// `strlen`: a byte loop, then the length.
const STRLEN: [u32; 6] = [
    0xaa00_03e1, // mov x1, x0
    0x3840_1422, // ldrb w2, [x1], #1
    0x35ff_ffe2, // cbnz w2, #-4
    0xcb00_0020, // sub x0, x1, x0
    0xd100_0400, // sub x0, x0, #1
    0xd65f_03c0, // ret
];

#[test]
fn a_byte_loop_agrees_with_the_jit() {
    let (_, aot) = agree(&STRLEN, |state| {
        // Point X0 at a run of nonzero bytes that ends in a zero.
        let at = (ARENA_ADDRESS + 0x1000) as *mut u8;
        // SAFETY: inside the arena the child mapped.
        unsafe {
            for index in 0..100 {
                at.add(index).write(1 + index as u8);
            }
            at.add(100).write(0);
        }
        state.x[0] = at as u64;
    });
    assert_eq!(aot.x[0], 100);
    assert_eq!(aot.kind, ExitKind::Return as u64);
}

/// A sum over an array with a counted loop, its registers outside the
/// pinned set.
const SUM: [u32; 9] = [
    0xd280_0009, // mov x9, #0
    0xd280_000a, // mov x10, #0
    0xf86a_7aab, // ldr x11, [x21, x10, lsl #3]
    0x8b0b_0129, // add x9, x9, x11
    0x9100_054a, // add x10, x10, #1
    0xf101_915f, // cmp x10, #100
    0x54ff_ff81, // b.ne #-16
    0xaa09_03e0, // mov x0, x9
    0xd65f_03c0, // ret
];

#[test]
fn a_counted_loop_agrees_and_keeps_its_registers() {
    let (jit, aot) = agree(&SUM, |state| {
        state.x[21] = (ARENA_ADDRESS + 0x2000) as u64;
    });
    assert_eq!(aot.retired, 2 + 100 * 5 + 2);
    let rows: Vec<_> = crate::block::aot::rows(&bytes(&SUM), GuestAddress(0x1000), &options());
    let region = super::super::edges::Region::plan(
        &rows.iter().map(super::Row::planned).collect::<Vec<_>>(),
        None,
    );
    let locals = super::plan(&rows, &region);
    assert!(locals.contains(&Some(crate::aarch64::Register::X(9))));
    assert!(locals.contains(&Some(crate::aarch64::Register::X(10))));
    assert!(locals.contains(&Some(crate::aarch64::Register::X(21))));
    assert!(
        aot.host < jit.host,
        "the unit runs fewer host instructions: JIT {} AOT {}",
        jit.host,
        aot.host
    );
}

/// A call and a return in the middle: each path leaves at its own.
const MIDDLE: [u32; 6] = [
    0xb400_0060, // cbz x0, #12
    0x9100_0421, // add x1, x1, #1
    0x9400_0100, // bl #0x400
    0x9100_0800, // add x0, x0, #2
    0xd65f_03c0, // ret
    0xd65f_03c0, // ret
];

#[test]
fn terminals_in_the_middle_end_only_their_paths() {
    let (_, taken) = agree(&MIDDLE, |state| state.x[0] = 0);
    assert_eq!((taken.x[0], taken.kind), (2, ExitKind::Return as u64));
    assert_eq!(taken.retired, 3);
    let (_, call) = agree(&MIDDLE, |state| state.x[0] = 5);
    assert_eq!(call.kind, ExitKind::Call as u64);
    assert_eq!(call.retired, 3);
    let entry = 0x10_0040_0000u64 - 0x1000;
    assert_eq!(call.x[30], entry + 12);
}

/// A flag setter whose branch jumps to a row that reads NZCV again: the
/// taken path stores the word the landing reads, both ways.
#[test]
fn flags_held_in_the_host_reach_a_landing() {
    let words = [
        0xf100_145f, // cmp x2, #5
        0x5400_006b, // b.lt #12
        0x9100_0463, // add x3, x3, #1
        0x1400_0002, // b #8
        0x9a9f_b7e4, // cset x4, ge
        0x9a9f_a7e5, // cset x5, lt
        0xd65f_03c0, // ret
    ];
    for value in [3, 7] {
        let (_, aot) = agree(&words, |state| state.x[2] = value);
        assert_eq!(aot.x[5], u64::from(value < 5));
    }
}

/// A System word stops a unit at itself, as it stops a block, counted by
/// its position.
#[test]
fn a_system_word_stops_both_at_itself() {
    let words = [
        0x9100_0421, // add x1, x1, #1
        0xd538_d080, // mrs x0, tpidr_el1
        0xd65f_03c0, // ret
    ];
    let (_, aot) = agree(&words, |_| {});
    assert_eq!((aot.kind, aot.retired), (ExitKind::System as u64, 1));
    let entry = 0x10_0040_0000u64 - 0x1000;
    assert_eq!(aot.exit, entry + 4);
}

/// A long loop that stores: its back edge leaves at the bound and the unit
/// is entered again, the counts exact across entries.
const FILL: [u32; 6] = [
    0xd280_000a, // mov x10, #0
    0xf82a_7a6a, // str x10, [x19, x10, lsl #3]
    0x9100_054a, // add x10, x10, #1
    0xf10f_a15f, // cmp x10, #1000
    0x54ff_ffa1, // b.ne #-12
    0xd65f_03c0, // ret
];

#[test]
fn a_long_storing_loop_agrees_across_entries() {
    let (jit, aot) = agree(&FILL, |state| state.x[19] = (ARENA_ADDRESS + 0x3000) as u64);
    assert_eq!(aot.retired, 1 + 1000 * 4 + 1);
    assert!(aot.runs > 1 && jit.runs > 1);
}

/// Eight registers live across a loop with flags read twice.
const WIDE: [u32; 14] = [
    0xd280_0062, // mov x2, #3
    0xd280_00a3, // mov x3, #5
    0xd280_0004, // mov x4, #0
    0x8b03_0044, // add x4, x2, x3        (loop)
    0xca04_0065, // eor x5, x3, x4
    0x8a05_0086, // and x6, x4, x5
    0xaa06_00a7, // orr x7, x5, x6
    0x9a87_0042, // csel x2, x2, x7, eq
    0xd100_0463, // sub x3, x3, #1
    0xf100_007f, // cmp x3, #0
    0x54ff_ff21, // b.ne #-28
    0x8b07_00c0, // add x0, x6, x7
    0x9a9f_17e1, // cset x1, eq
    0xd65f_03c0, // ret
];

#[test]
fn a_register_heavy_loop_agrees() {
    agree(&WIDE, |_| {});
}

/// Random straight-line and branching programs over a small instruction
/// set, both translators from the same state.
#[test]
fn random_programs_agree() {
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for _ in 0..60 {
        let count = 8 + (next() % 24) as usize;
        let mut words = Vec::new();
        for at in 0..count {
            let r = |value: u64| (value % 12) as u32 + 2;
            let (d, n, m) = (r(next()), r(next()), r(next()));
            let word = match next() % 10 {
                0 => 0x8b00_0000 | m << 16 | n << 5 | d,             // add
                1 => 0xcb00_0000 | m << 16 | n << 5 | d,             // sub
                2 => 0xeb00_0000 | m << 16 | n << 5 | 31,            // cmp
                3 => 0x9100_0000 | ((next() % 4096) as u32) << 10 | n << 5 | d, // add imm
                4 => 0xf940_0000 | ((next() % 64) as u32) << 10 | 21 << 5 | d, // ldr [x21]
                5 => 0xf900_0000 | ((next() % 64) as u32) << 10 | 21 << 5 | n, // str [x21]
                6 => 0x9a80_0000 | m << 16 | ((next() % 14) as u32) << 12 | n << 5 | d, // csel
                7 if at + 2 < count => {
                    // b.cond forward within the program
                    let skip = 1 + (next() % (count - at - 1) as u64) as u32;
                    0x5400_0000 | skip << 5 | (next() % 14) as u32
                }
                8 => 0xd280_0000 | ((next() % 65536) as u32) << 5 | d, // movz
                _ => 0xaa00_0000 | m << 16 | n << 5 | d,             // orr
            };
            words.push(word);
        }
        words.push(0xd65f_03c0);
        let source = bytes(&words);
        let entry = 0x10_0040_0000u64 - 0x1000;
        let setup = |state: &mut GuestState| state.x[21] = (ARENA_ADDRESS + 0x4000) as u64;
        let jit = run(Mode::Jit, &source, entry, setup);
        let aot = run(Mode::Aot, &source, entry, setup);
        let (Some(jit), Some(aot)) = (jit, aot) else {
            panic!("a run failed for {words:08x?}");
        };
        assert_eq!(jit.guest(), aot.guest(), "for {words:08x?}");
    }
}

/// The hottest functions of a boot's samples, run by hand (see the
/// module's doc).
#[test]
#[ignore = "manual: opt-in"]
fn units_of_real_functions() {
    use std::io::Write;
    let input = std::env::var("AOT_IN").expect("AOT_IN names the units");
    let out_path = std::env::var("AOT_OUT").unwrap_or_else(|_| alloc::string::String::from("/dev/null"));
    let mut out = std::fs::File::create(out_path).expect("the output");
    // Weighted: retired guest instructions, host instructions of each
    // mode, and their transfers; and the plain counts.
    let mut weighted = [0f64; 5];
    let (mut agreed, mut disagreed, mut skipped, mut bytes) = (0, 0, 0, [0u64; 2]);
    for line in std::fs::read_to_string(input).expect("the units").lines() {
        let mut fields = line.split(' ');
        let (Some(pc), Some(weight), Some(hex)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let pc = u64::from_str_radix(pc, 16).unwrap();
        let weight: f64 = weight.parse().unwrap_or(1.0);
        let source: Vec<u8> = (0..hex.len() / 2)
            .map(|at| u8::from_str_radix(&hex[2 * at..2 * at + 2], 16).unwrap())
            .collect();
        let jit = run(Mode::Jit, &source, pc, |_| {});
        let jit_crash = LAST_CRASH.get();
        let aot = run(Mode::Aot, &source, pc, |_| {});
        let aot_crash = LAST_CRASH.get();
        let (Some(jit), Some(aot)) = (jit, aot) else {
            // Both fault alike where the arena's values send an access
            // outside it; one that faults alone is a translator's bug.
            let alone = jit.is_some() != aot.is_some();
            writeln!(
                out,
                "X {pc:x} {} {} jit={jit_crash:x?} aot={aot_crash:x?}",
                jit.is_some(),
                aot.is_some()
            )
            .unwrap();
            if alone && let Some((_, at, offset)) = aot_crash.or(jit_crash) {
                let unit = crate::block::aot::compile_unit(
                    &source[(at - pc) as usize..],
                    GuestAddress(at),
                    &options(),
                );
                if let Ok(unit) = unit {
                    let code: alloc::string::String = unit
                        .native
                        .x86_64_code
                        .iter()
                        .map(|byte| std::format!("{byte:02x}"))
                        .collect();
                    writeln!(
                        out,
                        "  unit {at:x} offset {offset:x} map {:x?}\n  code {code}",
                        unit.native.source_map
                    )
                    .unwrap();
                }
                disagreed += 1;
            } else {
                skipped += 1;
            }
            continue;
        };
        if jit.finished == 0 || aot.finished == 0 || jit.retired == 0 {
            writeln!(out, "U {pc:x} jit={}:{}:{}:{}:{:x}:{} aot={}:{}:{}:{}:{:x}:{}", jit.finished, jit.runs, jit.host, jit.retired, jit.exit, jit.kind, aot.finished, aot.runs, aot.host, aot.retired, aot.exit, aot.kind).unwrap();
            skipped += 1;
            continue;
        }
        let same = jit.guest() == aot.guest();
        writeln!(
            out,
            "{} {pc:x} {weight} retired={} jit={}+{} aot={}+{} bytes={}/{} exit={:x}:{}",
            if same { "M" } else { "D" },
            jit.retired,
            jit.host,
            jit.runs,
            aot.host,
            aot.runs,
            jit.bytes,
            aot.bytes,
            jit.exit,
            jit.kind
        )
        .unwrap();
        if !same {
            writeln!(out, "  JIT {jit:x?}\n  AOT {aot:x?}").unwrap();
            disagreed += 1;
            continue;
        }
        agreed += 1;
        bytes[0] += jit.bytes;
        bytes[1] += aot.bytes;
        // Each function's weight spread over its retired instructions.
        let scale = weight / jit.retired as f64;
        weighted[0] += weight;
        weighted[1] += scale * jit.host as f64;
        weighted[2] += scale * aot.host as f64;
        weighted[3] += scale * (jit.runs - 1) as f64;
        weighted[4] += scale * (aot.runs - 1) as f64;
    }
    let [guest, jit, aot, jit_links, aot_links] = weighted;
    let links = LINK_INSTRUCTIONS as f64;
    let summary = std::format!(
        "units agreed={agreed} disagreed={disagreed} skipped={skipped}\n\
         host per guest instruction (sample-weighted): JIT {:.3} AOT {:.3} ({:+.1}%)\n\
         with {LINK_INSTRUCTIONS} per transfer between translations: JIT {:.3} AOT {:.3} ({:+.1}%)\n\
         transfers per guest instruction: JIT {:.4} AOT {:.4}\n\
         code bytes of the translations run: JIT {} AOT {}\n",
        jit / guest,
        aot / guest,
        100.0 * (aot / jit - 1.0),
        (jit + links * jit_links) / guest,
        (aot + links * aot_links) / guest,
        100.0 * ((aot + links * aot_links) / (jit + links * jit_links) - 1.0),
        jit_links / guest,
        aot_links / guest,
        bytes[0],
        bytes[1],
    );
    write!(out, "{summary}").unwrap();
    std::print!("{summary}");
    assert_eq!(disagreed, 0, "every unit agrees with the JIT");
}
