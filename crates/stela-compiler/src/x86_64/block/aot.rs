//! The ahead-of-time translator's emitter: one function unit at a time.
//!
//! A unit is the words of a function from an entry, at most
//! [`MAX_BLOCK_INSTRUCTIONS`] of them in one page. It keeps every contract
//! of a region (`block::region`) that the engine's settlement reads: one
//! entry PC, one source-map row per word in order, the retired count at
//! every point the region's `L + row` (`edges`), each entry bounded to
//! [`MAX_BLOCK_INSTRUCTIONS`] retired between polls, the locals of a run in
//! R8 to R11 whose fault rows name their hosts. What it does that a JIT
//! region cannot:
//!
//! - It does not end at the first terminal. A RET, BL, BLR, SVC or MRS in
//!   the middle of the function ends only the path through it, as an exit
//!   counted like a side exit; the unit goes on with the next word, which
//!   other paths reach by their branches. A JIT region stops at the first
//!   such word, and the code after it is another block entered through a
//!   transfer.
//! - Its locals are chosen for the whole unit ([`plan`]): the four guest
//!   registers outside the pinned set that the unit uses most, weighted by
//!   loop depth, stay in R8 to R11 from the entry to every exit, across
//!   every internal edge and every memory access. An instruction whose
//!   emitter may take a local's host stores them first and reloads them
//!   after, as a run of locals ends and starts again.
//!
//! Each instruction is emitted by the JIT's own [`emit_single`], each branch
//! as a region's, each terminal by [`emit_terminal`]: the unit's semantics
//! are the JIT's, and the hosted differential runs (`tests`) hold the two
//! to the same final state.
use super::{
    Held, SingleOptions, branch_condition, edges, emit_single, emit_terminal, recovery,
    region_exit, segmented, side_exit, terminal_exits,
};
use crate::x86_64::asm::{Code, Gpr, Mem, Size};
use crate::x86_64::chain::DirectExit;
use crate::x86_64::pinned::{Dirty, LocalRegisters};
use crate::{
    GuestAddress, NativeBlock,
    aarch64::{
        AddressMode, Instruction, MemoryTransfer, Operation, Register, WideMove, bitfield,
        conditional_select::CompareRight, integer_math, prefetch, simd,
    },
    block::{ExitKind, FaultRecovery, MAX_BLOCK_INSTRUCTIONS, Terminator},
};
use alloc::vec;
use alloc::vec::Vec;

/// One word of a unit, as the system-mode parser decodes it alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Row {
    /// An instruction a block holds in its body, or a branch.
    Body(Instruction),
    /// A word that ends a block.
    Terminal(GuestAddress, Terminator),
    /// A word the unit leaves to the JIT: an exit to it, counted as the
    /// words before it.
    Fallback(GuestAddress),
}

impl Row {
    pub(crate) fn pc(&self) -> GuestAddress {
        match self {
            Row::Body(instruction) => instruction.pc,
            Row::Terminal(pc, _) | Row::Fallback(pc) => *pc,
        }
    }

    /// The instruction the region's analyses see: a terminal or fallback
    /// as a branch out of the unit, which ends every path through it (the
    /// reach of an entry stops there), leaves NZCV live and lands nowhere
    /// inside.
    pub(in crate::x86_64) fn planned(&self) -> Instruction {
        match self {
            Row::Body(instruction) => instruction.clone(),
            _ => Instruction {
                pc: self.pc(),
                operation: Operation::Branch {
                    target: GuestAddress(u64::MAX & !3),
                },
            },
        }
    }
}

/// The hosts a unit's locals take, as `pinned::LOCAL_HOSTS`.
const LOCAL_HOSTS: [Gpr; 4] = [Gpr::R8, Gpr::R9, Gpr::R10, Gpr::R11];
/// The guest registers outside the pinned set: X0-X30 less SP, X0, X1,
/// X8, X16 and X19.
const UNPINNED: u32 = ((1u32 << 31) - 1) & !(1 | 1 << 1 | 1 << 8 | 1 << 16 | 1 << 19);

/// What an instruction emitted with locals mapped does with the general
/// registers: the operations a run of locals admits (`pinned::span`), whose
/// emitters reach guest registers only through `Code::host` and
/// `Asm::guest` and keep their scratch to RAX, RCX, RDX, and R8 where
/// `takes_r8` says. The unit never asks for masked bases, so R10 and R11
/// stay its own across accesses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Effects {
    pub reads: u32,
    pub writes: u32,
    pub takes_r8: bool,
}

fn bit(register: Register) -> u32 {
    match register {
        Register::X(index) if index < 31 => 1 << index,
        _ => 0,
    }
}

fn bits(registers: &[Register]) -> u32 {
    registers
        .iter()
        .fold(0, |mask, &register| mask | bit(register))
}

/// The effects of an operation a unit emits with its locals mapped, as
/// `pinned::span`'s admission; `None` for any other.
pub(crate) fn effects(operation: &Operation) -> Option<Effects> {
    let alu = |reads: &[Register], destination: Register| Effects {
        reads: bits(reads),
        writes: bit(destination),
        takes_r8: false,
    };
    let writeback = |base: Register, mode: AddressMode| match mode {
        AddressMode::Offset => 0,
        _ => bit(base),
    };
    Some(match *operation {
        Operation::Nop => alu(&[], Register::Zero),
        Operation::AddSub {
            destination,
            source,
            ..
        }
        | Operation::LogicalImmediate {
            destination,
            source,
            ..
        } => alu(&[source], destination),
        Operation::AddSubRegister {
            destination,
            left,
            right,
            ..
        }
        | Operation::AddSubExtended {
            destination,
            left,
            right,
            ..
        }
        | Operation::LogicalRegister {
            destination,
            left,
            right,
            ..
        } => alu(&[left, right], destination),
        Operation::MoveWide {
            kind, destination, ..
        } => match kind {
            WideMove::Keep => alu(&[destination], destination),
            _ => alu(&[], destination),
        },
        Operation::Bitfield(operand) => match operand.kind {
            bitfield::Kind::Insert => {
                alu(&[operand.source, operand.destination], operand.destination)
            }
            _ => alu(&[operand.source], operand.destination),
        },
        Operation::PcRelative(operand) => alu(&[], operand.destination),
        Operation::IntegerMath(operand) => match operand.kind {
            integer_math::Kind::Divide { .. } => return None,
            _ => alu(
                &[operand.left, operand.right, operand.accumulator],
                operand.destination,
            ),
        },
        Operation::ConditionalSelect(operand) => {
            alu(&[operand.left, operand.right], operand.destination)
        }
        Operation::ConditionalCompare(operand) => Effects {
            takes_r8: true,
            ..alu(
                &[
                    operand.left,
                    match operand.right {
                        CompareRight::Register(register) => register,
                        CompareRight::Immediate(_) => Register::Zero,
                    },
                ],
                Register::Zero,
            )
        },
        Operation::Memory {
            transfer,
            data,
            base,
            mode,
            ..
        } => Effects {
            reads: bit(base)
                | match transfer {
                    MemoryTransfer::Store => bit(data),
                    MemoryTransfer::Load { .. } => 0,
                },
            writes: writeback(base, mode)
                | match transfer {
                    MemoryTransfer::Store => 0,
                    MemoryTransfer::Load { .. } => bit(data),
                },
            takes_r8: false,
        },
        Operation::MemoryPair {
            transfer,
            data,
            base,
            mode,
            ..
        } => Effects {
            reads: bit(base)
                | match transfer {
                    MemoryTransfer::Store => bits(&data),
                    MemoryTransfer::Load { .. } => 0,
                },
            writes: writeback(base, mode)
                | match transfer {
                    MemoryTransfer::Store => 0,
                    MemoryTransfer::Load { .. } => bits(&data),
                },
            takes_r8: mode != AddressMode::Offset,
        },
        Operation::MemoryRegisterOffset(operand) => Effects {
            reads: bits(&[operand.base, operand.index])
                | match operand.transfer {
                    MemoryTransfer::Store => bit(operand.data),
                    MemoryTransfer::Load { .. } => 0,
                },
            writes: match operand.transfer {
                MemoryTransfer::Store => 0,
                MemoryTransfer::Load { .. } => bit(operand.data),
            },
            takes_r8: false,
        },
        Operation::Simd(
            simd::Operand::Memory { base, mode, .. } | simd::Operand::MemoryPair { base, mode, .. },
        ) => Effects {
            reads: bit(base),
            writes: writeback(base, mode),
            takes_r8: false,
        },
        Operation::Prefetch(operand) => match operand.address {
            prefetch::Address::Immediate { base, .. } => alu(&[base], Register::Zero),
            prefetch::Address::Register { base, index, .. } => alu(&[base, index], Register::Zero),
            prefetch::Address::Literal(_) => alu(&[], Register::Zero),
        },
        Operation::BranchCondition { .. } | Operation::Branch { .. } => alu(&[], Register::Zero),
        Operation::BranchZero { source, .. } | Operation::BranchBit { source, .. } => {
            alu(&[source], Register::Zero)
        }
        _ => return None,
    })
}

/// The loop depth of each row: one more inside each back edge's span.
fn depths(region: &edges::Region) -> Vec<u32> {
    let mut depth = vec![0u32; region.rows];
    for (from, to) in region
        .internal
        .iter()
        .enumerate()
        .filter_map(|(from, to)| Some((from, (*to)?)))
    {
        if to <= from {
            for row in &mut depth[to..=from] {
                *row += 1;
            }
        }
    }
    depth
}

/// The unit's locals: for each of R8 to R11, the unpinned guest register
/// it holds, the most used first, each use weighted by 8 per loop level.
/// R8 stays free when an admitted emitter may take it. A register needs at
/// least four weighted uses to pay its load at the entry and its store at
/// each exit.
pub(in crate::x86_64) fn plan(rows: &[Row], region: &edges::Region) -> LocalRegisters {
    let depth = depths(region);
    let weight = |row: usize| 8i64.pow(depth[row].min(3));
    let mut uses = [0i64; 31];
    // The first and last rows that use each register.
    let mut span = [(usize::MAX, 0usize); 31];
    let mut takes_r8 = false;
    // The rows emitted with the locals stored, which a local then loads
    // again after.
    let mut stored = Vec::new();
    for (row, entry) in rows.iter().enumerate() {
        let Row::Body(instruction) = entry else {
            continue;
        };
        let Some(effects) = effects(&instruction.operation) else {
            stored.push(row);
            continue;
        };
        takes_r8 |= effects.takes_r8;
        let mut touched = (effects.reads | effects.writes) & UNPINNED;
        while touched != 0 {
            let index = touched.trailing_zeros() as usize;
            uses[index] += weight(row);
            span[index] = (span[index].0.min(row), span[index].1.max(row));
            touched &= touched - 1;
        }
    }
    // A stored row between two uses costs a store and a load.
    for (index, gain) in uses.iter_mut().enumerate() {
        let (first, last) = span[index];
        *gain -= 2 * stored
            .iter()
            .filter(|&&row| first < row && row < last)
            .map(|&row| weight(row))
            .sum::<i64>();
    }
    let mut order: Vec<usize> = (0..31).filter(|&index| uses[index] >= 4).collect();
    order.sort_by(|a, b| uses[*b].cmp(&uses[*a]).then(a.cmp(b)));
    let mut locals = [None; LOCAL_HOSTS.len()];
    let mut candidates = order.into_iter();
    // R9 first, then R10 and R11, then R8 if no emitter takes it.
    for at in [1, 2, 3, 0] {
        if at == 0 && takes_r8 {
            continue;
        }
        let Some(index) = candidates.next() else {
            break;
        };
        locals[at] = Some(Register::X(index as u8));
    }
    locals
}

/// A set of a unit's locals: bit `i` for the local in `LOCAL_HOSTS[i]`.
type Set = u8;
/// Every local.
const ALL_LOCALS: Set = 0b1111;

/// The locals among the guest registers of `mask`.
fn locals_of(locals: LocalRegisters, mask: u32) -> Set {
    locals
        .iter()
        .enumerate()
        .filter(|(_, local)| local.is_some_and(|local| bit(local) & mask != 0))
        .fold(0, |set, (at, _)| set | 1 << at)
}

/// The `Dirty` of the locals in `set`.
fn dirty_of(locals: LocalRegisters, set: Set) -> Dirty {
    let mut dirty = Dirty::default();
    for (at, ((entry, local), host)) in dirty
        .iter_mut()
        .zip(locals)
        .zip(LOCAL_HOSTS)
        .enumerate()
    {
        if set & 1 << at != 0
            && let Some(Register::X(index)) = local
        {
            *entry = Some((index, host));
        }
    }
    dirty
}

/// What a row does with the locals.
#[derive(Clone, Copy)]
enum Use {
    /// Emitted with them mapped: which of them it writes.
    Mapped { writes: Set },
    /// Emitted with them stored and unmapped: every local is clean after
    /// it, and loads again where a later row reads it.
    Stored,
    /// An exit: the path ends.
    Exit,
}

/// Whether `row` is emitted with the locals mapped: an admitted operation
/// whose emitter keeps out of their hosts.
fn mapped(row: &Row, locals: LocalRegisters) -> Option<Effects> {
    let Row::Body(instruction) = row else {
        return None;
    };
    effects(&instruction.operation).filter(|effects| !(effects.takes_r8 && locals[0].is_some()))
}

/// Which locals a path from the entry may have written before each row
/// since they were last stored: their frame slots are stale, their hosts
/// what a fault row names and an exit stores.
pub(in crate::x86_64) struct LocalFlow {
    pub dirty: Vec<Set>,
}

impl LocalFlow {
    /// The flow of `locals` over `rows`.
    pub(in crate::x86_64) fn of(
        rows: &[Row],
        region: &edges::Region,
        locals: LocalRegisters,
    ) -> Self {
        let n = rows.len();
        let uses: Vec<Use> = rows
            .iter()
            .map(|row| match (row, mapped(row, locals)) {
                (_, Some(effects)) => Use::Mapped {
                    writes: locals_of(locals, effects.writes),
                },
                (Row::Body(_), None) => Use::Stored,
                _ => Use::Exit,
            })
            .collect();
        let successors = |row: usize| -> [Option<usize>; 2] {
            let next = (row + 1 < n).then_some(row + 1);
            match &rows[row] {
                Row::Body(instruction) => {
                    match crate::block::region::branch_target(&instruction.operation) {
                        Some((_, conditional)) => {
                            [region.internal[row], if conditional { next } else { None }]
                        }
                        None => [next, None],
                    }
                }
                _ => [None, None],
            }
        };
        // May-dirty, a union over the paths into each row.
        let mut dirty = vec![0 as Set; n];
        let mut changed = true;
        while changed {
            changed = false;
            for row in 0..n {
                let out = match uses[row] {
                    Use::Mapped { writes, .. } => dirty[row] | writes,
                    Use::Stored => 0,
                    Use::Exit => continue,
                };
                for successor in successors(row).into_iter().flatten() {
                    if dirty[successor] | out != dirty[successor] {
                        dirty[successor] |= out;
                        changed = true;
                    }
                }
            }
        }
        Self { dirty }
    }

    /// The locals a path may have written by the end of `row`.
    fn after(&self, rows: &[Row], locals: LocalRegisters, row: usize) -> Set {
        match (&rows[row], mapped(&rows[row], locals)) {
            (_, Some(effects)) => self.dirty[row] | locals_of(locals, effects.writes),
            _ => 0,
        }
    }
}

/// Load the locals of `set` from their frame slots.
fn load(code: &mut Code, locals: LocalRegisters, set: Set) {
    let mut asm = code.asm();
    for (at, (local, host)) in locals.into_iter().zip(LOCAL_HOSTS).enumerate() {
        if set & 1 << at != 0
            && let Some(Register::X(index)) = local
        {
            asm.mov_load(Size::Qword, host, Mem::base(Gpr::Rdi, i32::from(index) * 8));
        }
    }
}

/// Map the locals, `dirty` of them only in their hosts.
fn map(code: &mut Code, locals: LocalRegisters, dirty: Set) {
    code.set_local_registers(Some(locals));
    code.set_dirty_locals(dirty_of(locals, dirty));
}

/// Store the `dirty` locals and unmap them, for an emission that may take
/// their hosts or read the frame, or an exit.
fn leave(code: &mut Code, locals: LocalRegisters, dirty: Set) {
    code.set_local_registers(None);
    code.set_dirty_locals(Dirty::default());
    crate::x86_64::pinned::write_back_locals(code, dirty_of(locals, dirty));
}

/// A call, return or register branch in the middle of a unit, which
/// `terminator` (a DirectCall, Register or inline PAC transfer) makes, as
/// a block's terminal makes it but unpaired: a paired call's continuation
/// counts its block's rows as run, which a unit's rows after the call are
/// not. Its direct exit, if it has one.
fn transfer_exit(
    code: &mut Code,
    terminator: &Terminator,
    pc: GuestAddress,
    kind: ExitKind,
    fallthrough: GuestAddress,
    native: crate::x86_64::NativeSystem,
) -> Option<DirectExit> {
    match terminator {
        Terminator::DirectCall(target) => {
            crate::x86_64::guest_immediate(code, fallthrough);
            crate::x86_64::store(code, Register::X(30));
            return Some(region_exit(
                &mut code.asm(),
                *target,
                ExitKind::Call.into(),
            ));
        }
        Terminator::Register { source, .. } => {
            super::load_unless_held(code, *source, crate::x86_64::Rax::Unknown);
        }
        Terminator::PointerAuthentication(request) => {
            let cells = native.pointer.expect("an inline transfer has its cells");
            crate::x86_64::pointer::inline::authenticate_transfer(
                code,
                request.operation,
                pc,
                cells,
            );
        }
        _ => unreachable!("not a transfer"),
    }
    // The target is in RAX.
    if kind == ExitKind::Call {
        let mut asm = code.asm();
        asm.guest_imm64(Gpr::R9, fallthrough);
        let link = asm.guest(Register::X(30)).expect("X30 has storage");
        asm.mov_store(Size::Qword, link, Gpr::R9);
    }
    super::exit_kind(code, kind);
    None
}

/// What a unit's emission produced.
pub(crate) struct Unit {
    pub native: NativeBlock,
    pub fault_recovery: Vec<FaultRecovery>,
    pub exits: Vec<DirectExit>,
}

/// Emit the unit of `rows`, which start at its entry.
pub(crate) fn emit(
    rows: &[Row],
    target: crate::TargetCpu,
    checks: crate::x86_64::MemoryChecks,
    native: crate::x86_64::NativeSystem,
) -> Unit {
    use crate::x86_64::flag_word;
    assert!(!rows.is_empty() && rows.len() <= MAX_BLOCK_INSTRUCTIONS);
    let n = rows.len();
    let base = rows[0].pc();
    let planned: Vec<Instruction> = rows.iter().map(Row::planned).collect();
    let region = edges::Region::plan(&planned, None);
    let locals = plan(rows, &region);
    let flow = LocalFlow::of(rows, &region, locals);
    let checks = crate::x86_64::MemoryChecks {
        guest_range: true,
        ..checks
    };
    // Which accesses check their range and SP's alignment: per segment
    // between landings, as in a region.
    let mut range_rows = vec![true; n];
    if !checks.checks_every_access() {
        segmented(
            crate::x86_64::guest_range::checks,
            &planned,
            &region.landing,
            &mut range_rows,
        );
    }
    let mut stack_rows = vec![true; n];
    segmented(
        crate::x86_64::stack_alignment::checks,
        &planned,
        &region.landing,
        &mut stack_rows,
    );
    let checks_at = |row: usize| crate::x86_64::MemoryChecks {
        guest_range: range_rows[row],
        stack_checked: !stack_rows[row],
        ..checks
    };

    let mut block = crate::x86_64::Emitted {
        source_map: Vec::with_capacity(n),
        ..crate::x86_64::Emitted::default()
    };
    let code = &mut block.x86_64_code;
    code.set_pinned(true);
    code.set_linked_block(base.0);
    code.set_constant_time(target.data_independent_timing());
    // Leave a flag setter's NZCV in the host flags while its readers follow,
    // as pinned blocks do: `pending` holds its borrow then.
    let lazy_flags = !code.constant_time();
    let mut pending: Option<bool> = None;
    let mut scratch = Code::default();
    let mut fixups = vec![];
    let mut landings: Vec<Option<usize>> = vec![None; n];
    let mut edges_pending = edges::Pending::default();
    // Branches out of the unit: the jump, the target, the rows run, the
    // locals to store and the flags to capture.
    let mut side_exits = vec![];
    let mut exits = vec![];
    let mut pointer_paths = vec![];
    let mut mirrored_paths = vec![];
    let mut held = Held::NOTHING;
    // The locals each load where a row first reads one, and all of them
    // before a landing: every path reaches a landing with each in its host.
    let mut valid: Set = 0;
    map(code, locals, 0);
    // Whether the previous row's code can fall into this one.
    let mut falls = true;
    for (row, entry) in rows.iter().enumerate() {
        let dirty = flow.dirty[row];
        // The word goes to the frame before anything that may read it
        // there, and before a landing, which other paths reach with it
        // stored.
        if let Some(borrow) = pending
            && (region.landing[row] || !super::reads_host_flags(&planned[row].operation))
        {
            pending = None;
            if !region.nzcv_dead(&planned, row) {
                flag_word::capture(code, borrow);
                held.rax = crate::x86_64::Rax::Nzcv;
            }
        }
        if region.landing[row] {
            if falls {
                load(code, locals, ALL_LOCALS & !valid);
            }
            valid = ALL_LOCALS;
            landings[row] = Some(code.len());
            super::bind_landing(code, &mut edges_pending, row);
            held = Held::NOTHING;
        }
        code.set_dirty_locals(dirty_of(locals, dirty));
        falls = true;
        match entry {
            Row::Body(instruction)
                if crate::block::region::branch_target(&instruction.operation).is_some() =>
            {
                block.source_map.push((instruction.pc, code.len()));
                code.set_instruction(block.source_map.len() - 1);
                let (to, conditional) =
                    crate::block::region::branch_target(&instruction.operation)
                        .expect("a branch");
                let reads = mapped(entry, locals).map_or(0, |effects| locals_of(locals, effects.reads));
                load(code, locals, reads & !valid);
                valid |= reads;
                let mut borrow = pending;
                let taken = match (&instruction.operation, borrow.as_mut()) {
                    _ if !conditional => None,
                    (Operation::BranchCondition { condition, .. }, Some(borrow)) => {
                        Some(flag_word::host_condition(code, *condition, borrow))
                    }
                    _ => branch_condition(code, &instruction.operation, held.rax),
                };
                if pending.is_some() {
                    pending = borrow;
                }
                let internal = region.internal[row];
                match (internal, taken) {
                    (Some(at), taken) => {
                        let over = taken.map(|taken| code.asm().jcc(taken.negate()));
                        // The taken path stores the word unless where it
                        // lands defines NZCV again first.
                        if let Some(borrow) = borrow
                            && (at <= row || !region.nzcv_dead(&planned, at))
                        {
                            flag_word::capture(code, borrow);
                        }
                        load(code, locals, ALL_LOCALS & !valid);
                        edges::edge(
                            code,
                            &region,
                            row,
                            at,
                            to,
                            &landings,
                            &mut edges_pending,
                            dirty_of(locals, dirty),
                        );
                        match over {
                            Some(over) => code.asm().bind(over),
                            None => falls = false,
                        }
                    }
                    (None, Some(taken)) => {
                        let jump = code.asm().jcc(taken);
                        side_exits.push((jump, to, row + 1, dirty, borrow));
                    }
                    (None, None) => {
                        let jump = code.asm().jmp();
                        side_exits.push((jump, to, row + 1, dirty, borrow));
                        falls = false;
                    }
                }
                held = Held::NOTHING;
            }
            Row::Body(instruction) => {
                block.source_map.push((instruction.pc, code.len()));
                code.set_instruction(block.source_map.len() - 1);
                let admitted = mapped(entry, locals).is_some();
                match &instruction.operation {
                    Operation::PointerAuthentication(operation) => {
                        let cells = native
                            .pointer
                            .expect("units hold PAC instructions only given the service's cells");
                        leave(code, locals, dirty);
                        pointer_paths.push(crate::x86_64::pointer::inline::emit(
                            code,
                            *operation,
                            instruction.pc,
                            cells,
                        ));
                        valid = 0;
                        map(code, locals, 0);
                        held = Held::NOTHING;
                    }
                    Operation::MirroredRead { word } => {
                        let cell = native
                            .mirrored
                            .cell(*word)
                            .expect("units hold mirrored reads only given their cells");
                        leave(code, locals, dirty);
                        mirrored_paths.push(crate::x86_64::mirrored_read::emit(
                            code,
                            *word,
                            instruction.pc,
                            cell,
                        ));
                        valid = 0;
                        map(code, locals, 0);
                        held = Held::NOTHING;
                    }
                    operation => {
                        let frame_bound = crate::x86_64::pinned::frame_bound(operation);
                        if !admitted {
                            leave(code, locals, dirty);
                        } else if let Some(effects) = mapped(entry, locals) {
                            let reads = locals_of(locals, effects.reads);
                            load(code, locals, reads & !valid);
                            valid |= reads | locals_of(locals, effects.writes);
                        }
                        if frame_bound {
                            code.set_pinned(false);
                            crate::x86_64::pinned::write_back(code);
                        }
                        let start = code.len();
                        held = emit_single(
                            code,
                            &mut scratch,
                            None,
                            instruction,
                            &mut fixups,
                            SingleOptions {
                                target,
                                checks: checks_at(row),
                                live_nzcv: false,
                                defer_flags: lazy_flags
                                    && !frame_bound
                                    && super::deferral_pays(&planned, row, &region.landing, &[]),
                            },
                            held,
                            &mut pending,
                        );
                        if frame_bound {
                            crate::x86_64::pinned::reload(code);
                            code.set_pinned(true);
                        } else if held.rax == crate::x86_64::Rax::Nzcv
                            && row + 1 < n
                            && region.nzcv_dead(&planned, row + 1)
                            && let Some(tail) = flag_word::captured_tail(code, start)
                        {
                            // The next setter defines NZCV again before
                            // anything reads it: this capture is dead.
                            flag_word::drop_capture(code, tail);
                            held.rax = crate::x86_64::Rax::Unknown;
                        }
                        if !admitted {
                            valid = 0;
                            map(code, locals, 0);
                            held = Held::NOTHING;
                        }
                    }
                }
            }
            Row::Terminal(pc, terminator) => {
                // A terminal that completes counts every row at its exit, so
                // L first takes off the rows after it; one that stops at
                // itself counts by its position.
                let transfer = match terminator {
                    Terminator::DirectCall(_) => Some(ExitKind::Call),
                    Terminator::Register { kind, .. } => Some(*kind),
                    Terminator::PointerAuthentication(request) => native
                        .pointer
                        .and(crate::x86_64::pointer::inline::transfer(request.operation)),
                    _ => None,
                };
                // DC ZVA and the cache cleans complete natively and fall
                // through; any other System instruction stops at itself.
                let system_completes = |request| {
                    native
                        .zero_block
                        .zip(crate::x86_64::zero_block::operand(request))
                        .is_some()
                        || crate::x86_64::cache_clean::operand(request)
                            .is_some_and(|(operation, _)| native.cache_clean.contains(operation))
                };
                let completes = transfer.is_some()
                    || matches!(terminator, Terminator::FloatingPoint { inline: Some(operation) }
                        if crate::x86_64::simd::numerical_terminal_inline(operation, target))
                    || matches!(terminator, Terminator::System(request) if system_completes(request));
                leave(code, locals, dirty);
                let less = if completes { (n - row - 1) as u32 } else { 0 };
                if less != 0 || region.dirty(row) {
                    edges::settle(&mut code.asm(), less);
                }
                let fallthrough = GuestAddress(pc.0 + 4);
                if let Some(kind) = transfer {
                    block.source_map.push((*pc, code.len()));
                    code.set_instruction(row);
                    exits.extend(transfer_exit(code, terminator, *pc, kind, fallthrough, native));
                } else {
                    let start = code.len();
                    let terminal = (*pc, terminator.clone());
                    emit_terminal(
                        code,
                        &mut block.source_map,
                        Some(&terminal),
                        super::TerminalContext {
                            fallthrough,
                            native,
                            checks: checks_at(row),
                            rax: crate::x86_64::Rax::Unknown,
                            flags_live: false,
                            target,
                            zero_branch: None,
                        },
                    );
                    let found = terminal_exits(
                        &code[..],
                        Some(&terminal),
                        fallthrough,
                        Some(base.0),
                        block.source_map.len(),
                    );
                    debug_assert!(found.iter().all(|exit| exit.offset >= start));
                    exits.extend(found);
                }
                map(code, locals, 0);
                held = Held::NOTHING;
                falls = false;
            }
            Row::Fallback(pc) => {
                block.source_map.push((*pc, code.len()));
                code.set_instruction(block.source_map.len() - 1);
                leave(code, locals, dirty);
                let mut asm = code.asm();
                if region.dirty(row) {
                    edges::settle(&mut asm, 0);
                }
                exits.push(region_exit(&mut asm, *pc, side_exit(n - row)));
                map(code, locals, 0);
                held = Held::NOTHING;
                falls = false;
            }
        }
    }
    // Off the unit's end.
    if falls {
        if let Some(borrow) = pending.take() {
            flag_word::capture(code, borrow);
        }
        let next = GuestAddress(base.0 + 4 * n as u64);
        leave(code, locals, flow.after(rows, locals, n - 1));
        let mut asm = code.asm();
        if region.dirty(n - 1) {
            edges::settle(&mut asm, 0);
        }
        exits.push(region_exit(&mut asm, next, ExitKind::Fallthrough.into()));
    }
    debug_assert!(edges_pending.forward.is_empty(), "every landing bound");
    // Out of line: the back edges at their bound, the branches out, the
    // slow paths.
    for (jump, to) in core::mem::take(&mut edges_pending.bounded) {
        let mut asm = code.asm();
        asm.bind(jump);
        exits.push(region_exit(&mut asm, to, ExitKind::Direct.into()));
    }
    for (jump, to, ran, dirty, borrow) in side_exits {
        code.asm().bind(jump);
        if let Some(borrow) = borrow {
            flag_word::capture(code, borrow);
        }
        crate::x86_64::pinned::write_back_locals(code, dirty_of(locals, dirty));
        let mut asm = code.asm();
        if region.dirty(ran - 1) {
            edges::settle(&mut asm, 0);
        }
        exits.push(region_exit(&mut asm, to, side_exit(n - ran)));
    }
    if let Some(cells) = native.pointer {
        for path in pointer_paths {
            crate::x86_64::pointer::inline::place(code, path, cells);
        }
    }
    for path in mirrored_paths {
        crate::x86_64::mirrored_read::place(code, path);
    }
    code.place_failures();
    debug_assert!(fixups.is_empty());
    debug_assert_eq!(block.source_map.len(), n);
    let fault_recovery = recovery::bind(&block.x86_64_code, &block.source_map, &planned, vec![]);
    Unit {
        native: block.into_native(),
        fault_recovery,
        exits,
    }
}

#[cfg(all(test, target_arch = "x86_64", target_os = "linux"))]
mod tests;
