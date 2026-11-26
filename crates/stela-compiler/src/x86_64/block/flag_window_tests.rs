//! Pinned blocks whose setters leave NZCV to a flag window
//! ([`crate::x86_64::flag_window`]) run as the frame layout does: random
//! blocks of setters, readers, accesses, locals and side exits, from random
//! states, and with every access faulting in turn, leave the same state,
//! NZCV included, after the engine's settlement of the fault.
use super::vector_cache_tests::faulting;
use super::*;
use crate::{
    GuestState, Lifter, Limit, TargetCpu,
    aarch64::Decoder,
    block::{FaultRecovery, HostFaultSnapshot, HostGprSnapshot, HostSpillSnapshot, NZCV_BINDING},
    x86_64::{MemoryChecks, NativeSystem, pinned},
};

/// Guest memory at X1.
const WORDS: usize = 64;

struct Random(u64);
impl Random {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, bound: u32) -> u32 {
        (self.next() >> 33) as u32 % bound
    }
    /// A register the blocks compute in: pinned X0 and X19, the rest
    /// unpinned, never X1 (the memory) or X4 (the guard page).
    fn register(&mut self) -> u32 {
        [0, 2, 3, 6, 7, 9, 10, 11, 19, 20][self.below(10) as usize]
    }
}

/// A side exit well past the block: imm19 or imm14 of 0x100 words.
const FAR: u32 = 0x100;

fn word(random: &mut Random) -> u32 {
    let (d, n, m) = (random.register(), random.register(), random.register());
    let condition = random.below(14);
    let small = random.below(4);
    match random.below(25) {
        0 | 1 => 0xeb00_001f | m << 16 | n << 5, // cmp xn, xm
        2 => 0xf100_001f | small << 10 | n << 5, // cmp xn, #k
        3 => 0xf100_0000 | (1 + small) << 10 | d << 5 | d, // subs xd, xd, #k
        4 => 0x3100_0000 | small << 10 | n << 5 | d, // adds wd, wn, #k
        5 => 0xab00_001f | m << 16 | n << 5,     // cmn xn, xm
        6 => 0xf240_001f | n << 5,               // tst xn, #1
        7 => 0xea00_0000 | m << 16 | n << 5 | d, // ands xd, xn, xm
        8 => 0xeb00_0c00 | m << 16 | n << 5 | d, // subs xd, xn, xm, lsl #3
        9 => 0x6b00_001f | m << 16 | n << 5,     // cmp wn, wm
        10 | 11 => 0x5400_0000 | FAR << 5 | condition, // b.cond far
        12 => 0x9a80_0000 | m << 16 | condition << 12 | n << 5 | d, // csel xd, xn, xm, cond
        13 | 14 => 0xf940_0020 | random.below(WORDS as u32) << 10 | d, // ldr xd, [x1, #8k]
        15 => 0xf900_0020 | random.below(WORDS as u32) << 10 | d, // str xd, [x1, #8k]
        16 => {
            // ldp xd, xm, [x1, #16k], distinct
            let m = if m == d { [2, 3][(d == 2) as usize] } else { m };
            0xa940_0020 | random.below(WORDS as u32 / 2) << 15 | m << 10 | d
        }
        17 => 0x9100_0000 | small << 10 | n << 5 | d, // add xd, xn, #k
        18 => 0xaa00_03e0 | n << 16 | d,              // mov xd, xn
        19 => 0xb400_0000 | FAR << 5 | n,             // cbz xn, far
        20 => 0x3700_0000 | random.below(32) << 19 | FAR << 5 | n, // tbnz wn, #b, far
        21 => 0xd340_fc00 | (1 + small) << 16 | n << 5 | d, // lsr xd, xn, #k
        22 => 0xb900_0020 | random.below(WORDS as u32 * 2) << 10 | d, // str wd, [x1, #4k]
        23 => 0xcb00_0000 | m << 16 | n << 5 | d,     // sub xd, xn, xm
        _ => 0xd280_0000 | random.below(4) << 5 | d,  // mov xd, #k
    }
}

fn lift(words: &[u32]) -> Vec<Instruction> {
    words
        .iter()
        .enumerate()
        .map(|(index, &word)| {
            Decoder
                .lift(GuestAddress(0x1000 + 4 * index as u64), word)
                .unwrap_or_else(|error| panic!("{word:08x}: {error:?}"))
        })
        .collect()
}

fn initial(random: &mut Random, memory: &mut [u64; WORDS]) -> GuestState {
    let mut state = GuestState {
        nzcv: (random.below(16)) << 28,
        lower_limit: Limit(u64::MAX),
        ..GuestState::default()
    };
    let values = [0, 1, 2, 3, u64::MAX, 1 << 63, 0x7fff_ffff, 0x8000_0000];
    for x in state.x.iter_mut() {
        *x = match random.below(3) {
            0 => values[random.below(values.len() as u32) as usize],
            _ => random.next() >> random.below(64),
        };
    }
    for word in memory.iter_mut() {
        *word = random.next() >> random.below(64);
    }
    state.x[1] = memory.as_mut_ptr() as u64;
    state
}

fn tiers() -> Vec<TargetCpu> {
    let host = TargetCpu::detect();
    [TargetCpu::Baseline, TargetCpu::X86_64_V3, host]
        .into_iter()
        .filter(|target| host.supports(*target))
        .collect()
}

/// Random blocks, run whole and with each access's base moved to a guard
/// page in turn: the system-mode block's state at its exit, and after the
/// engine's settlement of its fault (pinned registers, then the row's
/// bindings), is the frame layout's, NZCV included; and some faults find
/// NZCV only by recomputing it.
#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
#[test]
fn flag_windows_recover_the_exact_nzcv_at_every_fault() {
    let guard = faulting::guard_page();
    for target in tiers() {
        let mut random = Random(0x5eed_f1a6_0b57_0001);
        let (mut faults, mut recomputed, mut exits) = (0, 0, 0);
        for case in 0..600 {
            let length = 3 + random.below(24) as usize;
            let words: Vec<u32> = (0..length).map(|_| word(&mut random)).collect();
            let accesses: Vec<usize> = lift(&words)
                .iter()
                .enumerate()
                .filter(|(_, instruction)| {
                    matches!(
                        instruction.operation,
                        crate::aarch64::Operation::Memory { .. }
                            | crate::aarch64::Operation::MemoryPair { .. }
                    )
                })
                .map(|(index, _)| index)
                .collect();
            let mut memory = [0; WORDS];
            let initial_state = initial(&mut random, &mut memory);
            for at in core::iter::once(None).chain(accesses.iter().copied().map(Some)) {
                let mut faulting = words.clone();
                if let Some(at) = at {
                    faulting[at] = faulting[at] & !(31 << 5) | 4 << 5;
                }
                let instructions = lift(&faulting);
                let end = GuestAddress(0x1000 + 4 * length as u64);
                let frame = emit(&instructions, None, end);
                let (block, recovery) = emit_system_mode(
                    &instructions,
                    None,
                    end,
                    target,
                    MemoryChecks::DEFAULT,
                    NativeSystem::NONE,
                );
                let entered = pinned::entered(&block.x86_64_code);
                let prefix = entered.len() - block.x86_64_code.len();
                let mut expected_memory = memory;
                let mut expected = initial_state.clone();
                expected.x[1] = expected_memory.as_mut_ptr() as u64;
                expected.x[4] = guard;
                let mut actual_memory = memory;
                let mut state = initial_state.clone();
                state.x[1] = actual_memory.as_mut_ptr() as u64;
                state.x[4] = guard;
                let frame_run = faulting::run(&frame.x86_64_code, &mut expected);
                let block_run = faulting::run(&entered, &mut state);
                let context =
                    alloc::format!("{target:?} case {case} access {at:?}: {faulting:08x?}");
                match (frame_run, block_run) {
                    (Ok(expected_exit), Ok(exit)) => {
                        assert_eq!(exit, expected_exit, "{context}");
                        exits += 1;
                    }
                    (Err(_), Err(fault)) => {
                        faults += 1;
                        let offset = fault.offset - prefix;
                        let gprs = HostGprSnapshot::legacy(fault.gprs).with_vectors(fault.vectors);
                        pinned::settle(&gprs, &mut state);
                        let spills = HostSpillSnapshot::new();
                        FaultRecovery::apply(
                            &recovery,
                            offset,
                            HostFaultSnapshot {
                                gprs: &gprs,
                                spills: &spills,
                            },
                            &mut state,
                        )
                        .unwrap_or_else(|error| panic!("{context}: {error}"))
                        .unwrap_or_else(|| panic!("{context}: no guest site"));
                        recomputed += usize::from(recovery.iter().any(|row| {
                            (row.start..row.end).contains(&offset)
                                && row
                                    .bindings
                                    .iter()
                                    .any(|binding| binding.guest == NZCV_BINDING)
                        }));
                    }
                    (frame_run, block_run) => {
                        panic!("{context}: frame {frame_run:?}, block {block_run:?}")
                    }
                }
                state.x[1] = 0;
                expected.x[1] = 0;
                assert_eq!(state, expected, "{context}");
                assert_eq!(actual_memory, expected_memory, "{context}");
            }
        }
        assert!(faults > 500, "{target:?}: {faults} faults");
        assert!(exits > 300, "{target:?}: {exits} exits");
        assert!(
            recomputed > 50,
            "{target:?}: {recomputed} faults recomputed NZCV"
        );
    }
}
