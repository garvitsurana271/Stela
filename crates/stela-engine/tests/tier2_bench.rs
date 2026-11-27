//! Host micro-benchmarks of tier 1 against tier 2 (`--tier2`): time-stamp
//! counter ticks per guest instruction retired, the engine's own work
//! (exits, links, lookups) included, on loops of the shapes macOS runs hot:
//! a 16-byte copy (memcpy), a hashed method-cache probe and call through
//! BLR and RET (objc_msgSend's hit path), a character-class scan (string
//! loops), integer arithmetic with a select, and a leaf function with an
//! if/else ladder called in a loop (the loop-free branchy code tier 2's
//! superblocks are for).
//!
//! Each program runs once to compile (and, for tier 2, to promote its hot
//! blocks), then five times timed; the fastest counts. Both tiers must end
//! in the same state. Run with
//! `cargo test -p stela-engine --features test-support --release --test tier2_bench -- --ignored --nocapture`.
#![cfg(all(
    feature = "test-support",
    feature = "std",
    target_arch = "x86_64",
    target_os = "linux"
))]
use anyhow::{Result, ensure};
use stela_compiler::{GuestState, TargetCpu};
use stela_engine::machine::engine::tier2::Tier2Options;
#[path = "support/store_host.rs"]
#[allow(dead_code, reason = "the benchmarks use only part of the host")]
mod store_host;
use store_host::{Mapping, machine};

const PAGE: u64 = 4096;

struct Program {
    name: &'static str,
    words: Vec<u32>,
    setup: fn(u64, &mut GuestState),
}

fn programs() -> Vec<Program> {
    vec![
        Program {
            name: "memcpy 16-byte loop",
            words: vec![
                0xa8c1_1023, // 1: ldp x3,x4,[x1],#16
                0xa881_1003, //    stp x3,x4,[x0],#16
                0xf100_4042, //    subs x2,x2,#16
                0x54ff_ffa1, //    b.ne 1b
                0xd100_0529, //    sub x9,x9,#1
                0xd104_0021, //    sub x1,x1,#256
                0xd104_0000, //    sub x0,x0,#256
                0xd280_2002, //    mov x2,#256
                0xb5ff_ff09, //    cbnz x9,1b
            ],
            setup: |base, state| {
                state.x[1] = base;
                state.x[0] = base + 2 * PAGE;
                state.x[2] = 256;
                state.x[9] = 4000;
            },
        },
        Program {
            name: "objc_msgSend-like probe and call",
            words: vec![
                // 0: the loop: the receiver's class, its cache, the bucket
                // of the selector's hash, a compare, a call to the IMP.
                0xf940_0010, //  0: ldr x16,[x0]           (isa)
                0x927c_ee10, //  1: and x16,x16,#~0xf      (mask)
                0xf940_0a11, //  2: ldr x17,[x16,#16]      (buckets)
                0x0a01_02a9, //  3: and w9,w21,w1          (hash)
                0x8b09_1229, //  4: add x9,x17,x9,lsl #4
                0xa940_2931, //  5: ldp x17,x10,[x9]       (imp, sel)
                0xeb01_015f, //  6: cmp x10,x1
                0x5400_0081, //  7: b.ne 11f               (miss)
                0xd63f_0220, //  8: blr x17
                0xf100_0673, //  9: subs x19,x19,#1
                0x54ff_fec1, // 10: b.ne 0b
                0x1400_0004, // 11: b 15f                  (done or miss)
                0xd503_201f, // 12: nop
                0xd503_201f, // 13: nop
                0xd503_201f, // 14: nop
                0xd503_201f, // 15: nop: the end
            ],
            setup: |base, state| {
                // The object at base points at its class; the class's
                // cache at +16 holds the bucket of selector 0x48 (hash
                // mask 0xff: bucket 0x48), whose IMP is the program's
                // function after its code (placed by the harness: x22).
                state.x[0] = base;
                state.x[1] = 0x48;
                state.x[21] = 0xff;
                state.x[19] = 20_000;
            },
        },
        Program {
            name: "string ctype scan",
            words: vec![
                0x3840_1408, // 1: ldrb w8,[x0],#1
                0x7101_851f, //    cmp w8,#'a'
                0x5400_0083, //    b.lo 2f
                0x7101_e91f, //    cmp w8,#'z'
                0x5400_0048, //    b.hi 2f
                0x9100_054a, //    add x10,x10,#1
                0xf100_0442, // 2: subs x2,x2,#1
                0x54ff_ff21, //    b.ne 1b
                0xd100_0529, //    sub x9,x9,#1
                0xd140_0400, //    sub x0,x0,#1,lsl #12
                0xd282_0002, //    mov x2,#4096
                0xb5ff_fea9, //    cbnz x9,1b
            ],
            setup: |base, state| {
                state.x[0] = base;
                state.x[2] = 4096;
                state.x[9] = 20;
            },
        },
        Program {
            name: "integer arithmetic with a select",
            words: vec![
                0x9b02_7c23, // 1: mul x3,x1,x2
                0xca03_0084, //    eor x4,x4,x3
                0x93c4_10a5, //    extr x5,x5,x4,#4
                0x8b05_0021, //    add x1,x1,x5
                0xeb02_003f, //    cmp x1,x2
                0x9a82_3021, //    csel x1,x1,x2,lo
                0x9100_0442, //    add x2,x2,#1
                0xf100_0673, //    subs x19,x19,#1
                0x54ff_ff01, //    b.ne 1b
            ],
            setup: |_, state| {
                state.x[1] = 0x1234_5678;
                state.x[2] = 3;
                state.x[19] = 100_000;
            },
        },
        Program {
            name: "branchy leaf called in a loop",
            words: vec![
                0x9400_0004, //  0: bl 4f
                0xf100_0673, //  1: subs x19,x19,#1
                0x54ff_ffc1, //  2: b.ne 0b
                0x1400_0012, //  3: b 21f (the end)
                // 4: the leaf: an if/else ladder over x0's low bits, each
                // arm joining by a B, then a common tail and RET.
                0x9100_0400, //  4: add x0,x0,#1
                0x3600_0060, //  5: tbz w0,#0,8f
                0x9100_0421, //  6: add x1,x1,#1
                0x1400_0002, //  7: b 9f
                0x9100_0442, //  8: add x2,x2,#1
                0x3608_0060, //  9: tbz w0,#1,12f
                0x9100_0463, // 10: add x3,x3,#1
                0x1400_0002, // 11: b 13f
                0x9100_0484, // 12: add x4,x4,#1
                0xf100_1c1f, // 13: cmp x0,#7
                0x5400_0068, // 14: b.hi 17f
                0x8b01_00a5, // 15: add x5,x5,x1
                0x1400_0002, // 16: b 18f
                0x8b02_00a5, // 17: add x5,x5,x2
                0x9a83_10c6, // 18: csel x6,x6,x3,ne
                0xca04_00e7, // 19: eor x7,x7,x4
                0xd65f_03c0, // 20: ret
                0xd503_201f, // 21: nop: the end
            ],
            setup: |_, state| {
                state.x[19] = 50_000;
            },
        },
    ]
}

/// The time-stamp counter.
fn ticks() -> u64 {
    // SAFETY: RDTSC has no preconditions on x86-64.
    unsafe { core::arch::x86_64::_rdtsc() }
}

/// The fastest of five timed runs of `program`: ticks, instructions retired
/// and the end state, after a run that compiles and promotes.
fn measure(
    code: &Mapping,
    data: &Mapping,
    program: &Program,
    tier2: bool,
) -> Result<(u64, u64, GuestState, u64)> {
    // Every program ends at a NOP after its words, where the run stops;
    // the msgSend program's IMP follows: add x3,x3,#1; ret.
    let mut words = program.words.clone();
    let end = code.0 + 4 * words.len() as u64;
    let imp = end + 4;
    words.extend([0xd503_201f, 0x9100_0463, 0xd65f_03c0]);
    let mut machine = machine(code, data, &words, TargetCpu::detect())?;
    machine.set_breakpoints(&[machine.host.lower_synchronous(), end]);
    if tier2 {
        machine.set_tier2(Some(Tier2Options {
            check: false,
            threshold: 1,
            check_every: 1,
        }));
    }
    // Data: an object pointing at its class, the class's cache at +16
    // pointing at buckets whose bucket 0x48 holds the IMP and selector.
    let mut contents = vec![0u8; 4 * PAGE as usize];
    for (at, byte) in contents.iter_mut().enumerate() {
        *byte = (at * 7 + 3) as u8 | 0x40;
    }
    let class = data.0 + 0x100;
    let buckets = data.0 + 0x1000;
    contents[0..8].copy_from_slice(&class.to_le_bytes());
    contents[0x110..0x118].copy_from_slice(&buckets.to_le_bytes());
    let bucket = 0x1000 + 0x48 * 16;
    contents[bucket..bucket + 8].copy_from_slice(&imp.to_le_bytes());
    contents[bucket + 8..bucket + 16].copy_from_slice(&0x48u64.to_le_bytes());
    let mut best = (u64::MAX, 0, GuestState::default(), 0);
    for round in 0..6 {
        ensure!(data.write(data.0, &contents), "write data");
        let mut state = GuestState::default();
        (program.setup)(data.0, &mut state);
        machine.cpu.state = state;
        machine.cpu.pc = code.0;
        let retired = machine.cpu.retired;
        let started = ticks();
        machine.run(machine.statistics.blocks + 100_000_000);
        let took = ticks() - started;
        let ran = machine.cpu.retired - retired;
        ensure!(
            machine.cpu.pc == end,
            "{} ended at {:#x}",
            program.name,
            machine.cpu.pc
        );
        if round > 0 && took < best.0 {
            best = (
                took,
                ran,
                machine.cpu.state.clone(),
                machine.statistics.tier2.installed,
            );
        }
    }
    Ok(best)
}

#[test]
#[ignore = "manual: opt-in"]
fn tier2_against_tier1_in_ticks_per_instruction() -> Result<()> {
    let data = Mapping::new()?;
    let code = Mapping::new()?;
    eprintln!(
        "bench program | tier 1 ticks/insn | tier 2 ticks/insn | tier 2 / tier 1 | instructions | tier-2 blocks"
    );
    for program in programs() {
        let (one, ran_one, end_one, _) = measure(&code, &data, &program, false)?;
        let (two, ran_two, end_two, installed) = measure(&code, &data, &program, true)?;
        ensure!(
            ran_one == ran_two,
            "{}: {ran_one} against {ran_two} instructions",
            program.name
        );
        ensure!(end_one == end_two, "{}: the tiers end apart", program.name);
        let per = |ticks: u64| ticks as f64 / ran_one as f64;
        eprintln!(
            "bench {} | {:.3} | {:.3} | {:.3} | {ran_one} | {installed}",
            program.name,
            per(one),
            per(two),
            two as f64 / one as f64
        );
    }
    Ok(())
}
