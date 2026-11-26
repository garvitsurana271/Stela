use super::*;
use crate::{
    block::{ExitKind, FaultRecovery, RecoverySite, RetirementPoint},
    x86_64::chain,
};

/// The composites' entry: a page's second word, so exits ahead and behind
/// stay in its page.
const PC: u64 = 0x1_0000_1004;

#[test]
fn names_round_trip() {
    for routine in Routine::PRODUCTION {
        assert_eq!(Routine::from_name(routine.name()), Some(routine));
    }
    assert_eq!(Routine::from_name("bail"), None);
    assert_eq!(Routine::from_name("touch"), None);
    assert_eq!(Routine::from_name("Memmove"), None);
    assert_eq!(Routine::Bail.name(), "bail");
    assert_eq!(Routine::Touch.name(), "touch");
    let bits = Routine::PRODUCTION
        .iter()
        .chain(&[Routine::Bail, Routine::Touch])
        .fold(0, |bits, routine| {
            assert_eq!(bits & routine.bit(), 0, "{routine:?}");
            bits | routine.bit()
        });
    assert_eq!(bits, 0x7f);
}

#[test]
fn the_layout_the_fast_paths_assume_holds() {
    // X2's frame slot, which a fast path loads n from.
    assert_eq!(super::super::offset(Register::X(2)), Some(16));
    // A success exit ahead in the page keeps a link slot, unpolled.
    let slot = chain::link_slot(Some(0x1000), GuestAddress(0x1010), ExitKind::Direct);
    assert_eq!(slot, chain::LINK_BYTES);
    assert!(PREFIX_ALIGN.is_multiple_of(8));
}

/// The plan's synthetic bail: 3 NOPs and the fallthrough exit to the next
/// word, with no link slot, at offset 3.
fn synthetic() -> CompiledBlock {
    let next = GuestAddress(PC + 4);
    let mut code = vec![0x90; 3];
    code.extend_from_slice(&chain::direct_exit(next, ExitKind::Fallthrough));
    CompiledBlock {
        native: NativeBlock {
            x86_64_code: code,
            source_map: vec![(GuestAddress(PC), 0)],
            relocations: None,
        },
        source_bytes: 4,
        direct_exits: vec![DirectExit {
            offset: 3,
            target: next,
            kind: ExitKind::Fallthrough,
            skipped: 0,
        }],
        fault_recovery: Vec::new(),
        self_loop: false,
        region: false,
        cold: (0, 0),
        position_independent: false,
    }
}

fn request(routine: Routine, exit: u64) -> Request {
    Request {
        routine,
        pc: GuestAddress(PC),
        exit: GuestAddress(PC + exit),
        max_bytes: 65_536,
        target: crate::TargetCpu::Baseline,
    }
}

/// The three vector sets a fast path picks from.
const TARGETS: [crate::TargetCpu; 3] = [
    crate::TargetCpu::Baseline,
    crate::TargetCpu::Avx2,
    crate::TargetCpu::Avx512f,
];

/// The target of the `jmp rel32` or `jcc rel32` whose displacement ends at
/// `end` in `code`.
fn target(code: &[u8], end: usize) -> usize {
    let rel = i32::from_le_bytes(code[end - 4..end].try_into().unwrap());
    (end as i64 + i64::from(rel)) as usize
}

#[test]
fn a_bail_only_composite_jumps_from_byte_zero_to_the_bail() {
    let composite = compose(request(Routine::Bail, 64), synthetic()).unwrap();
    let code = &composite.block.native.x86_64_code;
    assert_eq!(composite.prefix_end, 64);
    assert_eq!(code[0], 0xe9);
    assert_eq!(target(code, 5), 64);
    assert!(code[5..64].iter().all(|&byte| byte == INT3));
    // The bail's code follows unchanged: its exit's residue mod 8 is kept.
    assert_eq!(code[64..], synthetic().native.x86_64_code[..]);
    assert!(composite.block.native.relocations.is_none());
}

#[test]
fn compose_shifts_the_bail_s_exits_map_and_cold() {
    let mut bail = synthetic();
    bail.cold = (1, 3);
    bail.fault_recovery = vec![FaultRecovery {
        start: 0,
        end: 3,
        bindings: Vec::new(),
        retirement: RetirementPoint::before(GuestAddress(PC), 0).unwrap(),
        site: RecoverySite::guest_access(),
    }];
    let composite = compose(request(Routine::Bail, 64), bail).unwrap();
    let end = composite.prefix_end as usize;
    let block = &composite.block;
    let next = GuestAddress(PC + 4);
    assert_eq!(
        block.direct_exits,
        [DirectExit {
            offset: 3 + end,
            target: next,
            kind: ExitKind::Fallthrough,
            skipped: 0,
        }]
    );
    let exit = &block.native.x86_64_code[3 + end..3 + end + chain::DIRECT_EXIT_BYTES];
    assert!(chain::is_direct_exit(exit, next, ExitKind::Fallthrough));
    // One instruction, at the fast path: a second entry for the bail's
    // code would make two of its one word.
    assert_eq!(block.native.source_map, [(GuestAddress(PC), 0)]);
    assert_eq!(block.source_bytes, 4);
    assert_eq!(block.cold, (1 + end as u32, 3 + end as u32));
    assert_eq!(
        (block.fault_recovery[0].start, block.fault_recovery[0].end),
        (end, end + 3)
    );
    assert!(!block.self_loop && !block.region);
    // No cold code stays none.
    let plain = compose(request(Routine::Bail, 64), synthetic()).unwrap();
    assert_eq!(plain.block.cold, (0, 0));
}

#[test]
fn compose_moves_a_compiled_bail_whole() {
    use crate::{
        Options, TargetCpu,
        block::{BlockProfile, Features},
        x86_64::{MemoryChecks, RangeCheck},
    };
    // ldrb w8,[x1] as the engine compiles a block's one instruction.
    let options = Options::new(
        BlockProfile::new(Features::SYSTEM_MODE),
        TargetCpu::Baseline,
    )
    .with_memory_checks(
        MemoryChecks::system(false, false, true, false).with_range(RangeCheck::four_level(3)),
    );
    let bail = crate::compile(&0x3940_0028u32.to_le_bytes(), GuestAddress(PC), &options).unwrap();
    assert!(!bail.fault_recovery.is_empty() && !bail.direct_exits.is_empty());
    for (routine, exit) in [(Routine::Bail, 64), (Routine::Touch, 12)] {
        let composite = compose(request(routine, exit), bail.clone()).unwrap();
        let end = composite.prefix_end as usize;
        let block = &composite.block;
        assert_eq!(block.native.x86_64_code[end..], bail.native.x86_64_code[..]);
        let moved = block.direct_exits.iter().filter(|exit| exit.offset >= end);
        assert!(
            moved
                .zip(&bail.direct_exits)
                .all(|(moved, exit)| moved.offset == exit.offset + end
                    && (moved.target, moved.kind, moved.skipped)
                        == (exit.target, exit.kind, exit.skipped)),
            "{routine:?}"
        );
        for (moved, row) in block.fault_recovery.iter().zip(&bail.fault_recovery) {
            assert_eq!((moved.start, moved.end), (row.start + end, row.end + end));
            assert_eq!((moved.retirement, moved.site), (row.retirement, row.site));
        }
        // Every exit is ready to link where an 8-byte aligned install puts it.
        for exit in &block.direct_exits {
            let slot = chain::link_slot(Some(PC), exit.target, exit.kind);
            assert!(
                chain::exit_ready(&block.native.x86_64_code, exit.offset, 0, slot),
                "{routine:?} {exit:?}"
            );
        }
    }
}

#[test]
fn touch_has_a_direct_success_exit_with_a_forward_link_slot() {
    let composite = compose(request(Routine::Touch, 12), synthetic()).unwrap();
    let block = &composite.block;
    let code = &block.native.x86_64_code;
    let exit = GuestAddress(PC + 12);
    let successes: Vec<_> = block
        .direct_exits
        .iter()
        .filter(|candidate| candidate.target == exit)
        .collect();
    assert_eq!(successes.len(), 1);
    let success = successes[0];
    assert_eq!((success.kind, success.skipped), (ExitKind::Direct, 0));
    let slot = chain::link_slot(Some(PC), exit, ExitKind::Direct);
    assert_ne!(slot, 0);
    assert!(chain::exit_ready(code, success.offset, 0, slot));
    let end = composite.prefix_end as usize;
    assert!(success.offset + chain::DIRECT_EXIT_BYTES <= end);
    assert_eq!(end % PREFIX_ALIGN as usize, 0);
    // The bail's exit follows, moved.
    assert_eq!(block.direct_exits.last().unwrap().offset, 3 + end);
    // The range check's two branches and the jump after the load all go to
    // the bail; the jump ends where the success exit's slot starts.
    let checks: Vec<_> = (0..success.offset - slot)
        .filter(|&at| code[at] == 0x0f && matches!(code[at + 1], 0x82 | 0x86))
        .collect();
    assert_eq!(checks.len(), 2, "{code:02x?}");
    for at in checks {
        assert_eq!(target(code, at + 6), end);
    }
    let start = success.offset - slot;
    assert_eq!(code[start - 5], 0xe9);
    assert_eq!(target(code, start), end);
}

#[test]
fn compose_refuses_a_bail_of_two_instructions_and_an_exit_off_the_page() {
    let mut two = synthetic();
    two.source_bytes = 8;
    assert_eq!(
        compose(request(Routine::Bail, 64), two).unwrap_err(),
        Refusal::BailNotOneInstruction
    );
    let mut mapped_twice = synthetic();
    mapped_twice.native.source_map.push((GuestAddress(PC), 3));
    assert_eq!(
        compose(request(Routine::Bail, 64), mapped_twice).unwrap_err(),
        Refusal::BailNotOneInstruction
    );
    let mut looping = synthetic();
    looping.self_loop = true;
    assert_eq!(
        compose(request(Routine::Bail, 64), looping).unwrap_err(),
        Refusal::BailNotOneInstruction
    );
    // The next page, and the one before.
    for exit in [GuestAddress(PC + 4096), GuestAddress(PC - 8)] {
        let request = Request {
            exit,
            ..request(Routine::Touch, 0)
        };
        assert_eq!(
            compose(request, synthetic()).unwrap_err(),
            Refusal::ExitOutsidePage
        );
    }
    assert_eq!(
        compose(request(Routine::Touch, 0), synthetic()).unwrap_err(),
        Refusal::ExitIsEntry
    );
    for max_bytes in [0, 255, (1 << 20) + 1] {
        let request = Request {
            max_bytes,
            ..request(Routine::Touch, 12)
        };
        assert_eq!(
            compose(request, synthetic()).unwrap_err(),
            Refusal::MaxBytes
        );
    }
}

/// Every production routine lays out on each vector set: a fast path of
/// whole alignment units within a mark's bound, one direct success exit to
/// the `ret`, ready to link, and every bail jump at the bail's code.
#[test]
fn every_production_routine_composes_on_each_vector_set() {
    for routine in Routine::PRODUCTION {
        for target in TARGETS {
            let request = Request {
                target,
                ..request(routine, 60)
            };
            let composite = compose(request, synthetic()).unwrap();
            let end = composite.prefix_end as usize;
            let block = &composite.block;
            let code = &block.native.x86_64_code;
            assert!(end.is_multiple_of(PREFIX_ALIGN as usize) && end <= MAX_PREFIX_BYTES as usize);
            let exit = GuestAddress(PC + 60);
            let successes: Vec<_> = block
                .direct_exits
                .iter()
                .filter(|candidate| candidate.offset < end)
                .collect();
            assert_eq!(successes.len(), 1, "{routine:?} {target:?}");
            let success = successes[0];
            assert_eq!(
                (success.target, success.kind, success.skipped),
                (exit, ExitKind::Direct, 0)
            );
            let slot = chain::link_slot(Some(PC), exit, ExitKind::Direct);
            assert!(chain::exit_ready(code, success.offset, 0, slot));
            assert_eq!(code[end..], synthetic().native.x86_64_code[..]);
            // AVX code ends in vzeroupper before its success exit; SSE2
            // code holds no VEX or EVEX prefix at all.
            let vex = code[..end]
                .windows(3)
                .any(|bytes| bytes == [0xc5, 0xf8, 0x77]);
            assert_eq!(
                vex,
                target != crate::TargetCpu::Baseline,
                "{routine:?} {target:?}"
            );
        }
    }
}
