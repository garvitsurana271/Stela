//! Tier 1's output, pinned: a deterministic corpus of blocks compiled with
//! the options the system-mode engine compiles with, each result's digest
//! compared against the golden set `tier1_golden.txt`, which the compiler
//! of `72a0c83c0` (before tier 2 existed) produced, written again for the
//! block ABI's guest-state-v16 frame layout, whose shorter frame
//! displacements change the blocks' bytes and the offsets into them.
//!
//! Tier 2 (`--tier2`) must leave every tier-1 compilation byte for byte as
//! it was: the native code, its source map, relocations, direct exits,
//! fault-recovery rows and shape. A mismatch names the case, its options
//! and its words.
//!
//! To write the golden set again from a compiler known to be tier 1's
//! (never to paper over a mismatch):
//! `STELA_TIER1_GOLDEN_WRITE=/path/tier1_golden.txt cargo test -p stela-engine tier1_golden`.
use stela_compiler::{
    GuestAddress, Options, TargetCpu,
    block::{BlockProfile, Features},
    x86_64::{MemoryChecks, NativeSystem, RangeCheck, cache_clean},
};

const GOLDEN: &str = include_str!("tier1_golden.txt");
const CASES: usize = 2000;

/// A family of encodings: the fixed bits and the bits drawn at random.
struct Family {
    fixed: u32,
    random: u32,
}

const fn family(fixed: u32, random: u32) -> Family {
    Family { fixed, random }
}

/// Branch families draw their offsets small, so most land in the block.
const FAMILIES: &[Family] = &[
    // ADD/SUB (immediate), with and without flags, 32 and 64 bits.
    family(0x1100_0000, 0xe07f_ffff),
    // ADD/SUB (shifted register).
    family(0x0b00_0000, 0xe0df_ffff),
    // ADD/SUB (extended register).
    family(0x0b20_0000, 0xe01f_ffff),
    // ADC/SBC.
    family(0x1a00_0000, 0xe01f_03ff),
    // Logical (shifted register).
    family(0x0a00_0000, 0xe0ff_ffff),
    // Logical (immediate): some masks are reserved, which refuse.
    family(0x1200_0000, 0xe07f_ffff),
    // MOVN/MOVZ/MOVK.
    family(0x1280_0000, 0xe07f_ffff),
    // Bitfield (SBFM/BFM/UBFM).
    family(0x1300_0000, 0xe03f_ffff),
    // EXTR.
    family(0x1380_0000, 0x801f_ffff),
    // CSEL/CSINC/CSINV/CSNEG.
    family(0x1a80_0000, 0xc01f_fcff),
    // CCMP/CCMN (immediate and register).
    family(0x3a40_0000, 0xc01f_fbef),
    // MADD/MSUB.
    family(0x1b00_0000, 0x801f_ffff),
    // UDIV/SDIV, LSLV/LSRV/ASRV/RORV.
    family(0x1ac0_0800, 0x801f_07ff),
    // CLZ/CLS/RBIT/REV.
    family(0x5ac0_0000, 0x8000_1fff),
    // ADR/ADRP.
    family(0x1000_0000, 0xe0ff_ffff),
    // LDR/STR (unsigned offset), byte to doubleword, small offsets.
    family(0x3900_0000, 0xc0c0_7fff),
    // LDR/STR (pre- and post-index).
    family(0x3800_0400, 0xc0c1_fbff),
    // LDR/STR (register offset).
    family(0x3820_0800, 0xc0df_f3ff),
    // LDUR/STUR.
    family(0x3800_0000, 0xc0c1_f3ff),
    // LDP/STP (signed offset, pre- and post-index), 32 and 64 bits.
    family(0x2900_0000, 0x81c1_ffff),
    family(0x2880_0000, 0x81c1_ffff),
    // LDR (literal).
    family(0x1800_0000, 0x4000_0fff),
    // LDXR/STXR, LDAXR/STLXR.
    family(0x885f_7c00, 0x4000_83ff),
    family(0x8800_7c00, 0x401f_83ff),
    // LDAR/STLR.
    family(0x88df_fc00, 0x4040_03ff),
    // LSE atomics: LDADD/LDCLR/LDEOR/LDSET/SWP.
    family(0xb820_0000, 0x40df_f3ff),
    // CAS.
    family(0x88a0_7c00, 0x405f_83ff),
    // B.cond, small offsets either way.
    family(0x5400_0000, 0x0000_03ef),
    family(0x54ff_fc00, 0x0000_03ef),
    // CBZ/CBNZ.
    family(0x3400_0000, 0x8100_03ff),
    family(0x34ff_fc00, 0x8100_03ff),
    // TBZ/TBNZ.
    family(0x3600_0000, 0x81f8_03ff),
    // B, small offsets either way.
    family(0x1400_0000, 0x0000_001f),
    family(0x17ff_ffe0, 0x0000_001f),
    // BL.
    family(0x9400_0000, 0x0000_00ff),
    // Scalar FP arithmetic (FADD/FSUB/FMUL/FDIV), single and double.
    family(0x1e20_0800, 0x005f_f3ff),
    // FCMP.
    family(0x1e20_2000, 0x005f_fc18),
    // FMOV between general and FP registers.
    family(0x1e26_0000, 0x8081_03ff),
    // SIMD three same (integer), 64 and 128 bits.
    family(0x0e20_0400, 0x601f_fbff),
    // LD1/ST1 (multiple structures, no offset).
    family(0x0c00_7000, 0x4040_0fff),
];

/// Single words the corpus mixes in.
const WORDS: &[u32] = &[
    0xd503_201f, // nop
    0xd503_3bbf, // dmb ish
    0xd503_3fdf, // isb
    0xd538_d080, // mrs x0, tpidr_el1
    0xd53b_d040, // mrs x0, tpidr_el0
    0xd53b_d060, // mrs x0, tpidrro_el0
    0xd53b_e040, // mrs x0, cntvct_el0
    0xd518_d080, // msr tpidr_el1, x0
    0xd503_237f, // pacibsp
    0xd503_23ff, // autibsp
    0xdac1_0e11, // autib x17, x16 (autib x17? a PAC form)
    0xd50b_7420, // dc zva, x0
    0xd50b_7a20, // dc cvac, x0
    0xd50b_7520, // ic ivau, x0
    0xd508_871f, // tlbi vmalle1
    0xd400_0001, // svc #0
    0xd420_0000, // brk #0
    0xd503_3f9f, // dsb sy
    0xd65f_03c0, // ret
    0xd65f_0fff, // retab
    0xd61f_0200, // br x16
    0xd63f_0200, // blr x16
    0xd71f_0a11, // braa x16, x17
];

/// The terminals a case may end with.
const TERMINALS: &[u32] = &[
    0xd65f_03c0, // ret
    0xd61f_0200, // br x16
    0x9400_0010, // bl +0x40
    0x1400_0040, // b +0x100
    0xd400_0001, // svc #0
];

/// Loops of several blocks, where regions form.
const LOOPS: &[&[u32]] = &[
    &[
        0x3840_1408,
        0x7100_811f,
        0x5400_0061,
        0x9100_054a,
        0x1400_0002,
        0x9100_056b,
        0xf100_0442,
        0x54ff_ff21,
        0xd65f_03c0,
    ],
    &[
        0x1400_0003,
        0xf800_8403,
        0x9100_0463,
        0xf100_0442,
        0x54ff_ffa2,
        0x9100_04e7,
        0xd65f_03c0,
    ],
    &[
        0xaa04_03e5,
        0x3840_1426,
        0x3800_1406,
        0xf100_04a5,
        0x54ff_ffa1,
        0x8b03_0000,
        0xf100_0442,
        0x54ff_ff21,
        0xd65f_03c0,
    ],
    &[
        0xaa1f_03e9,
        0xf840_8408,
        0xf100_0442,
        0x54ff_ffc1,
        0xd65f_03c0,
    ],
    &[0x3840_1408, 0x35ff_ffe8, 0xd65f_03c0],
];

/// xorshift64*: deterministic across hosts and toolchains.
pub(super) struct Rng(u64);

impl Rng {
    pub(super) fn new(seed: u64) -> Self {
        Self(seed | 1)
    }
    pub(super) fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    pub(super) fn below(&mut self, bound: usize) -> usize {
        (self.next() >> 33) as usize % bound
    }
}

/// The words of case `index`, and the PC of its first.
fn case(index: usize) -> (Vec<u32>, u64) {
    let mut rng = Rng::new(0x5eed_0000_0000_0000 ^ (index as u64).wrapping_mul(0x9e37_79b9));
    // Pages at both halves, some cases ending near a page's end.
    let page = match rng.below(3) {
        0 => 0xffff_fe00_0712_4000,
        1 => 0x0000_0001_0003_8000,
        _ => 0x0000_7ff8_1234_c000,
    };
    if index.is_multiple_of(17) {
        let words = LOOPS[index / 17 % LOOPS.len()].to_vec();
        return (words, page + 0x100);
    }
    let length = 1 + rng.below(40);
    let mut words = Vec::with_capacity(length + 1);
    for _ in 0..length {
        let word = if rng.below(8) == 0 {
            WORDS[rng.below(WORDS.len())]
        } else {
            let family = &FAMILIES[rng.below(FAMILIES.len())];
            family.fixed | (rng.next() as u32 & family.random)
        };
        words.push(word);
    }
    if rng.below(4) != 0 {
        words.push(TERMINALS[rng.below(TERMINALS.len())]);
    }
    let room = 0x1000 - 4 * words.len() as u64;
    let offset = match rng.below(4) {
        // Ending at the page's last word.
        0 => room,
        _ => (rng.next() % (room / 4 + 1)) * 4,
    };
    (words, page + offset)
}

/// The fixed host the corpus compiles for: a Zen 4 laptop's features, so the
/// golden set does not depend on the machine the test runs on.
pub(super) fn target() -> TargetCpu {
    TargetCpu::X86_64_V4
        .with_aes()
        .with_pclmulqdq()
        .with_sha()
        .with_gfni()
        .with_fast_pdep()
        .with_atomic_load16()
}

/// The option sets: the engine's own at EL1 and EL0, without regions and
/// self-loops, and its profiling form.
pub(super) fn variants() -> [Options; 4] {
    let target = target();
    let profile = BlockProfile::new(Features::SYSTEM_MODE);
    let full = NativeSystem {
        zero_block: Some(64),
        cache_clean: cache_clean::Operations::ALL,
        exclusive: true,
        pointer: Some(super::super::pointer::cells(target)),
        mirrored: super::super::regime_state::inline_mirrored_reads(),
        amx: false,
        self_loops: true,
        loop_counts: false,
        regions: true,
        traces: false,
    };
    let el1 = MemoryChecks::system(true, false, true, false);
    let el0 = MemoryChecks::system(true, false, true, false)
        .with_range(RangeCheck::FIVE_LEVEL.one_sided(true));
    [
        Options::new(profile, target)
            .with_memory_checks(el1)
            .with_native_system(full),
        Options::new(profile, target)
            .with_memory_checks(el0)
            .with_native_system(full),
        Options::new(profile, target)
            .with_memory_checks(el1)
            .with_native_system(NativeSystem {
                self_loops: false,
                regions: false,
                exclusive: false,
                pointer: None,
                ..full
            }),
        Options::new(profile, target)
            .with_memory_checks(el0)
            .with_native_system(NativeSystem {
                loop_counts: true,
                ..full
            }),
    ]
}

/// Two FNV-1a lanes: enough to see any change, which is all this needs.
#[derive(Clone, Copy)]
struct Digest([u64; 2]);

impl Digest {
    fn new() -> Self {
        Self([0xcbf2_9ce4_8422_2325, 0x6c62_272e_07bb_0142])
    }
    fn bytes(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.0[0] = (self.0[0] ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3);
            self.0[1] = (self.0[1] ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3);
            self.0[1] = self.0[1].rotate_left(5);
        }
        // Lengths delimit fields.
        self.0[0] ^= bytes.len() as u64;
        self.0[1] = self.0[1].wrapping_add(bytes.len() as u64);
    }
    fn word(&mut self, value: u64) {
        self.bytes(&value.to_le_bytes());
    }
    fn hex(self) -> String {
        format!("{:016x}{:016x}", self.0[0], self.0[1])
    }
}

/// A refusal's digest.
fn refused() -> String {
    let mut digest = Digest::new();
    digest.bytes(b"refused");
    digest.hex()
}

/// The digest of everything a tier-1 compilation leaves: or of its refusal.
pub(super) fn digest(
    result: &Result<stela_compiler::block::CompiledBlock, stela_compiler::CompileError>,
) -> String {
    let mut digest = Digest::new();
    match result {
        Err(_) => return refused(),
        Ok(block) => {
            digest.bytes(&block.native.x86_64_code);
            for (pc, offset) in &block.native.source_map {
                digest.word(pc.0);
                digest.word(*offset as u64);
            }
            match &block.native.relocations {
                None => digest.bytes(b"unrelocated"),
                Some(fields) => {
                    for field in fields {
                        digest.word(u64::from(*field));
                    }
                }
            }
            digest.word(block.source_bytes as u64);
            for exit in &block.direct_exits {
                digest.word(exit.offset as u64);
                digest.word(exit.target.0);
                digest.bytes(format!("{:?}", exit.kind).as_bytes());
                digest.word(u64::from(exit.skipped));
            }
            for row in &block.fault_recovery {
                digest.bytes(format!("{row:?}").as_bytes());
            }
            digest.word(u64::from(block.self_loop));
            digest.word(u64::from(block.region));
            digest.word(u64::from(block.cold.0) << 32 | u64::from(block.cold.1));
        }
    }
    digest.hex()
}

fn lines() -> Vec<String> {
    let variants = variants();
    let mut lines = Vec::with_capacity(CASES * variants.len());
    for index in 0..CASES {
        let (words, pc) = case(index);
        let source: Vec<u8> = words.iter().flat_map(|word| word.to_le_bytes()).collect();
        for (variant, options) in variants.iter().enumerate() {
            let compiled = stela_compiler::compile(&source, GuestAddress(pc), options);
            lines.push(format!("{index} {variant} {}", digest(&compiled)));
        }
    }
    lines
}

#[test]
fn tier1_golden_output_is_unchanged() {
    let lines = lines();
    if let Some(path) = std::env::var_os("STELA_TIER1_GOLDEN_WRITE") {
        let mut text = lines.join("\n");
        text.push('\n');
        std::fs::write(path, text).expect("golden set written");
        return;
    }
    let golden: Vec<&str> = GOLDEN.lines().collect();
    assert_eq!(golden.len(), lines.len(), "the golden set holds every case");
    let mut changed = vec![];
    for (line, expected) in lines.iter().zip(&golden) {
        if line != expected {
            changed.push(line.clone());
        }
    }
    let compiled = lines
        .iter()
        .filter(|line| !line.ends_with(&refused()))
        .count();
    // The corpus must exercise the compiler, not only its refusals.
    assert!(
        compiled * 2 > lines.len(),
        "{compiled} of {} compiled",
        lines.len()
    );
    if let Some(first) = changed.first() {
        let index: usize = first.split(' ').next().unwrap().parse().unwrap();
        let (words, pc) = case(index);
        panic!(
            "{} of {} tier-1 compilations changed; first: {first} at {pc:#x}: {words:08x?}",
            changed.len(),
            lines.len()
        );
    }
}
