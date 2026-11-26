//! Flag windows: NZCV a flag setter leaves unstored while the block runs on
//! to the next instruction that defines all four flags.
//!
//! Pinned code keeps a setter's NZCV in the host flags while only its
//! readers follow ([`super::flag_word`]), and stores the word before
//! anything else may need it. Where nothing after the readers needs it
//! before the next definer, the word is dead there, but an access between
//! can fault, and its guest exception takes the exact NZCV. A window covers
//! those rows instead: the word stays unstored, and each fault in the window
//! recomputes NZCV from the guest registers the setter read
//! ([`crate::block::FlagSetter`], bound by [`super::block::recovery`]). Its
//! rows may only access memory, compute without NZCV and take the side
//! exits of CBZ, CBNZ, TBZ and TBNZ, whose taken paths recompute the word
//! out of line ([`recompute`]); none writes a register the setter read.
use super::asm::{Alu, Code, Gpr, ShiftOp, Size, Unary};
use crate::{
    aarch64::{
        AddressMode, Instruction, Logical, MemoryTransfer, Operation, Register, Shift, Width,
        non_temporal, simd,
    },
    block::{FLAG_ZERO_REGISTER, FlagOperand, FlagOperation, FlagSetter},
};

/// A register's number in a [`FlagSetter`].
fn number(register: Register) -> u8 {
    match register {
        Register::X(index) => index,
        Register::Sp => 31,
        Register::Zero => FLAG_ZERO_REGISTER,
    }
}

/// A register's bit in a mask of the general registers and SP (bit 31).
fn bit(register: Register) -> u32 {
    match register {
        Register::X(index) => 1 << index,
        Register::Sp => 1 << 31,
        Register::Zero => 0,
    }
}

fn shift_number(shift: Shift) -> u8 {
    match shift {
        Shift::Lsl => 0,
        Shift::Lsr => 1,
        Shift::Asr => 2,
        Shift::Ror => 3,
    }
}

/// The setter `operation` is, if a window can take its NZCV, and the
/// registers its recomputation reads, which the window must leave alone.
pub(super) fn setter(operation: &Operation) -> Option<(FlagSetter, u32)> {
    let made = match *operation {
        Operation::AddSub {
            width,
            subtract,
            flags: true,
            destination,
            source,
            immediate,
        } => {
            let operation = if subtract {
                FlagOperation::Subtract
            } else {
                FlagOperation::Add
            };
            // Where SUBS overwrote its source, the source is the result
            // plus the immediate.
            let from_result = destination == source;
            let left = if from_result { destination } else { source };
            (
                FlagSetter::new(
                    operation,
                    width == Width::X64,
                    from_result,
                    number(left),
                    FlagOperand::Immediate(u64::from(immediate)),
                )?,
                bit(left),
            )
        }
        Operation::AddSubRegister {
            width,
            subtract,
            flags: true,
            destination,
            left,
            right,
            shift,
            amount,
        } => {
            if destination == right && destination != Register::Zero {
                return None;
            }
            let operation = if subtract {
                FlagOperation::Subtract
            } else {
                FlagOperation::Add
            };
            (
                FlagSetter::new(
                    operation,
                    width == Width::X64,
                    destination == left && destination != Register::Zero,
                    number(left),
                    FlagOperand::Register {
                        register: number(right),
                        shift: shift_number(shift),
                        amount,
                    },
                )?,
                bit(left) | bit(right),
            )
        }
        Operation::LogicalImmediate {
            width,
            kind: Logical::And,
            flags: true,
            destination,
            source,
            immediate,
        } => {
            let x64 = width == Width::X64;
            if destination == Register::Zero {
                (
                    FlagSetter::new(
                        FlagOperation::And,
                        x64,
                        false,
                        number(source),
                        FlagOperand::Immediate(immediate),
                    )?,
                    bit(source),
                )
            } else {
                // ANDS: N and Z from the result it wrote, C and V clear.
                (
                    FlagSetter::new(
                        FlagOperation::And,
                        x64,
                        true,
                        number(destination),
                        FlagOperand::Immediate(0),
                    )?,
                    bit(destination),
                )
            }
        }
        Operation::LogicalRegister {
            width,
            kind: Logical::And,
            invert,
            flags: true,
            destination,
            left,
            right,
            shift,
            amount,
        } => {
            let x64 = width == Width::X64;
            if destination == Register::Zero {
                (
                    FlagSetter::new(
                        if invert {
                            FlagOperation::AndNot
                        } else {
                            FlagOperation::And
                        },
                        x64,
                        false,
                        number(left),
                        FlagOperand::Register {
                            register: number(right),
                            shift: shift_number(shift),
                            amount,
                        },
                    )?,
                    bit(left) | bit(right),
                )
            } else {
                (
                    FlagSetter::new(
                        FlagOperation::And,
                        x64,
                        true,
                        number(destination),
                        FlagOperand::Immediate(0),
                    )?,
                    bit(destination),
                )
            }
        }
        _ => return None,
    };
    Some(made)
}

/// What a row of a window does.
enum Row {
    /// Defines all of NZCV, reading none: the window ends before it.
    Defines,
    /// Leaves NZCV alone, and writes these registers.
    Passes(u32),
}

fn writeback(base: Register, mode: AddressMode) -> u32 {
    match mode {
        AddressMode::Offset => 0,
        _ => bit(base),
    }
}

/// How `operation` may take part in a window; `None` where it may not: it
/// reads NZCV or the frame's word, or can leave other than by a fault or a
/// zero or bit test's side exit.
fn row(operation: &Operation) -> Option<Row> {
    Some(match *operation {
        Operation::AddSub {
            flags, destination, ..
        }
        | Operation::AddSubRegister {
            flags, destination, ..
        }
        | Operation::AddSubExtended {
            flags, destination, ..
        }
        | Operation::LogicalImmediate {
            flags, destination, ..
        }
        | Operation::LogicalRegister {
            flags, destination, ..
        } => {
            if flags {
                Row::Defines
            } else {
                Row::Passes(bit(destination))
            }
        }
        Operation::MoveWide { destination, .. } => Row::Passes(bit(destination)),
        Operation::Bitfield(operand) => Row::Passes(bit(operand.destination)),
        Operation::PcRelative(operand) => Row::Passes(bit(operand.destination)),
        Operation::Nop => Row::Passes(0),
        Operation::Memory {
            transfer,
            data,
            base,
            mode,
            ..
        } => Row::Passes(
            writeback(base, mode)
                | match transfer {
                    MemoryTransfer::Load { .. } => bit(data),
                    MemoryTransfer::Store => 0,
                },
        ),
        Operation::MemoryPair {
            transfer,
            data,
            base,
            mode,
            ..
        } => Row::Passes(
            writeback(base, mode)
                | match transfer {
                    MemoryTransfer::Load { .. } => bit(data[0]) | bit(data[1]),
                    MemoryTransfer::Store => 0,
                },
        ),
        Operation::MemoryRegisterOffset(operand) => Row::Passes(match operand.transfer {
            MemoryTransfer::Load { .. } => bit(operand.data),
            MemoryTransfer::Store => 0,
        }),
        Operation::MemoryNonTemporal(operand) => Row::Passes(match operand.data {
            non_temporal::Data::Integer(data) if operand.load => bit(data[0]) | bit(data[1]),
            _ => 0,
        }),
        Operation::Simd(
            simd::Operand::Memory { base, mode, .. } | simd::Operand::MemoryPair { base, mode, .. },
        ) => Row::Passes(writeback(base, mode)),
        Operation::BranchZero { .. } | Operation::BranchBit { .. } => Row::Passes(0),
        _ => return None,
    })
}

/// The row a window opened at `from` would end at, before the instruction
/// that next defines NZCV, for the setter at `setter`, whose recomputation
/// reads `reads`: `None` where none may. The setter's readers up to `from`
/// leave those registers alone too; no row of the window is a landing, and
/// `excluded` rows (branches a region or self-loop emits itself) stay out.
pub(super) fn end(
    instructions: &[Instruction],
    setter: usize,
    from: usize,
    reads: u32,
    landing: &[bool],
    excluded: impl Fn(usize) -> bool,
) -> Option<usize> {
    for reader in &instructions[setter + 1..from] {
        let writes = match reader.operation {
            Operation::ConditionalSelect(operand) => bit(operand.destination),
            Operation::BranchCondition { .. } => 0,
            _ => return None,
        };
        if writes & reads != 0 {
            return None;
        }
    }
    for at in from..instructions.len() {
        if landing[at] && at > from {
            // Another path joins the rows after it.
            return None;
        }
        match row(&instructions[at].operation)? {
            Row::Defines => return (at > from).then_some(at),
            Row::Passes(_) if excluded(at) => return None,
            Row::Passes(writes) if writes & reads != 0 => {
                // The row's own faults find what it read: the window takes
                // it, where it writes no pair, and ends after it if no row
                // up to the definer can fault or leave.
                if matches!(
                    instructions[at].operation,
                    Operation::MemoryPair { .. } | Operation::MemoryNonTemporal(_)
                ) {
                    return None;
                }
                let rest = instructions[at + 1..]
                    .iter()
                    .enumerate()
                    .find(|(_, instruction)| !pure(&instruction.operation))?;
                let definer = at + 1 + rest.0;
                return (matches!(row(&rest.1.operation), Some(Row::Defines))
                    && !landing[at + 1..=definer].contains(&true))
                .then_some(at + 1);
            }
            Row::Passes(_) => {}
        }
    }
    None
}

/// Whether `operation` neither touches NZCV nor can fault or leave.
fn pure(operation: &Operation) -> bool {
    match *operation {
        Operation::AddSub { flags, .. }
        | Operation::AddSubRegister { flags, .. }
        | Operation::AddSubExtended { flags, .. }
        | Operation::LogicalImmediate { flags, .. }
        | Operation::LogicalRegister { flags, .. } => !flags,
        Operation::MoveWide { .. }
        | Operation::Bitfield(_)
        | Operation::PcRelative(_)
        | Operation::Nop => true,
        _ => false,
    }
}

fn register(number: u8) -> Register {
    match number {
        0..=30 => Register::X(number),
        31 => Register::Sp,
        _ => Register::Zero,
    }
}

/// The host flags of `setter`, from the guest registers it read, and the
/// word stored: for a side exit in its window, which leaves to code that
/// may read NZCV. RAX and RCX change.
pub(super) fn recompute(code: &mut Code, setter: FlagSetter) {
    let bits = setter.bits();
    let size = if bits >> 2 & 1 == 1 {
        Size::Qword
    } else {
        Size::Dword
    };
    let from_result = bits >> 3 & 1 == 1;
    let operation = bits & 3;
    let mut asm = code.asm();
    let load = |asm: &mut super::asm::Asm, destination: Gpr, number: u8| match asm
        .guest(register(number))
    {
        Some(storage) => asm.mov_load(Size::Qword, destination, storage),
        None => asm.alu(Alu::Xor, Size::Dword, destination, destination),
    };
    load(&mut asm, Gpr::Rax, (bits >> 4 & 0x3f) as u8);
    if operation >= 2 && from_result {
        // ANDS or BICS: N and Z of the result, C and V clear.
        asm.test(size, Gpr::Rax, Gpr::Rax);
    } else {
        if bits >> 10 & 1 == 1 {
            load(&mut asm, Gpr::Rcx, (bits >> 11 & 0x3f) as u8);
            let amount = (bits >> 19 & 0x3f) as u8;
            if amount != 0 {
                let op = match bits >> 17 & 3 {
                    0 => ShiftOp::Shl,
                    1 => ShiftOp::Shr,
                    2 => ShiftOp::Sar,
                    _ => ShiftOp::Ror,
                };
                asm.shift_imm(op, size, Gpr::Rcx, amount);
            }
        } else {
            asm.mov_imm(Gpr::Rcx, bits >> 11);
        }
        match operation {
            0 | 1 => {
                if from_result {
                    // The left operand back from the result.
                    let undo = if operation == 1 { Alu::Add } else { Alu::Sub };
                    asm.alu(undo, size, Gpr::Rax, Gpr::Rcx);
                }
                let op = if operation == 1 { Alu::Cmp } else { Alu::Add };
                asm.alu(op, size, Gpr::Rax, Gpr::Rcx);
            }
            2 => asm.test(size, Gpr::Rax, Gpr::Rcx),
            _ => {
                asm.unary(Unary::Not, size, Gpr::Rcx);
                asm.test(size, Gpr::Rax, Gpr::Rcx);
            }
        }
    }
    super::flag_word::capture(code, operation == 1);
}
