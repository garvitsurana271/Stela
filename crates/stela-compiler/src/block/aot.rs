//! Ahead-of-time units: a function's words from an entry, compiled as one
//! translation ([`compile_unit`]) by `x86_64::block::aot`.
//!
//! Each word is decoded alone by the system-mode parser, as a block of one
//! word, so the unit sees exactly the instructions and terminals the JIT
//! would. A unit is a stored block in every contract the engine reads:
//! one entry, a contiguous source of at most [`MAX_BLOCK_INSTRUCTIONS`]
//! words that stays in the entry's page, one source-map row per word, the
//! fault rows and direct exits a region has. Words the unit does not take
//! are exits to themselves, which the JIT then compiles: an undecodable
//! word, an exclusive completed natively, DC ZVA or an AMX operation in a
//! body. A unit whose first word is one is refused: the JIT compiles there.
use super::{CompiledBlock, MAX_BLOCK_INSTRUCTIONS, Terminator, analysis::parse_with};
use crate::x86_64::block::aot::{self as emitter, Row};
use crate::{CompileError, GuestAddress, aarch64::Operation};
use alloc::vec::Vec;

const PAGE: u64 = 4096;

/// The rows of the unit at `base` in `source`: to the source's end, the
/// page's or [`MAX_BLOCK_INSTRUCTIONS`].
pub(crate) fn rows(source: &[u8], base: GuestAddress, options: &crate::Options) -> Vec<Row> {
    let page_end = (base.0 / PAGE + 1) * PAGE;
    let words = (source.len() / 4)
        .min(((page_end - base.0) / 4) as usize)
        .min(MAX_BLOCK_INSTRUCTIONS);
    // One word at a time: no region forms, nothing looks past the word.
    let native = crate::x86_64::NativeSystem {
        regions: false,
        traces: false,
        ..options.native_system()
    };
    let target = options.target();
    let checks = options.memory_checks();
    (0..words)
        .map(|index| {
            let pc = GuestAddress(base.0 + 4 * index as u64);
            let word = &source[4 * index..4 * index + 4];
            let Ok(parsed) = parse_with(word, pc, options.profile(), native) else {
                return Row::Fallback(pc);
            };
            match (parsed.instructions.as_slice(), parsed.terminal) {
                ([instruction], None) => {
                    let unsupported = matches!(
                        instruction.operation,
                        Operation::ZeroBlock { .. } | Operation::Amx(_) | Operation::Return
                    ) || crate::x86_64::validate_target(instruction, target).is_err()
                        || (!checks.stack_alignment
                            && crate::x86_64::validate_target_with(instruction, target, checks)
                                .is_err());
                    if unsupported {
                        Row::Fallback(pc)
                    } else {
                        Row::Body(instruction.clone())
                    }
                }
                ([], Some((at, Terminator::Branch(operation)))) => {
                    Row::Body(crate::aarch64::Instruction { pc: at, operation })
                }
                // An exclusive completed natively may leave by either of
                // two counts; the JIT keeps it.
                ([], Some((_, Terminator::System(request))))
                    if native.exclusive
                        && crate::x86_64::native_exclusive::operand(&request).is_some() =>
                {
                    Row::Fallback(pc)
                }
                ([], Some((at, terminator))) => Row::Terminal(at, terminator),
                _ => Row::Fallback(pc),
            }
        })
        .collect()
}

/// The unit entered at `base`, whose function's words from there are
/// `source`, for a system-mode `options`.
pub fn compile_unit(
    source: &[u8],
    base: GuestAddress,
    options: &crate::Options,
) -> Result<CompiledBlock, CompileError> {
    options.validate()?;
    if !options.profile().has(super::Features::FULL_SYSTEM)
        || source.len() < 4
        || !base.0.is_multiple_of(4)
    {
        return Err(CompileError::new(
            crate::CompileErrorKind::InvalidSourceLayout,
        ));
    }
    let rows = rows(source, base, options);
    // A unit that would leave at once is the JIT's block.
    if matches!(rows[0], Row::Fallback(_)) {
        return Err(CompileError::new(
            crate::CompileErrorKind::InvalidSourceLayout,
        ));
    }
    let unit = emitter::emit(
        &rows,
        options.target(),
        options.memory_checks(),
        options.native_system(),
    );
    let mut native = unit.native;
    crate::x86_64::block::align_exits(&mut native.x86_64_code, &unit.exits, Some(base.0));
    Ok(CompiledBlock {
        native,
        source_bytes: 4 * rows.len(),
        direct_exits: unit.exits,
        fault_recovery: unit.fault_recovery,
        self_loop: false,
        region: true,
        cold: (0, 0),
    })
}
