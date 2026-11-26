//! One function's control-flow graph, register liveness and calls, from
//! its words alone: the function runs from its start to the next function
//! start (`LC_FUNCTION_STARTS`), and every branch inside it is an edge.
//!
//! Liveness is the usual backward dataflow over basic blocks, with two
//! boundary rules:
//! - where control leaves the function (a return, tail call, indirect
//!   branch, trap or call out), the translator must hand the engine every
//!   guest register in its frame slot, so every register is live there
//!   ([`Boundary::Engine`]);
//! - [`Boundary::Abi`] instead assumes the AAPCS64 contract of compiled
//!   code: at a return X0-X7, X19-X29, SP and X30 are live, NZCV is dead, a
//!   call reads X0-X7 and clobbers X0-X18 and X30 and NZCV. That is what a
//!   function-scope register assignment may assume for the code it
//!   assigns, never for correctness at an exit.
use crate::a64::{self, ALL, Effects, Flow, Insn, SP};

/// The guest registers the system-mode translator keeps pinned in host
/// registers across every block (`x86_64::pinned::PINNED`): SP, X8, X19,
/// X0, X1, X16.
pub const PINNED: u32 = SP | 1 << 8 | 1 << 19 | 1 | 1 << 1 | 1 << 16;
/// X0-X7.
const ARGUMENTS: u32 = 0xff;
/// X19-X29, SP and X30: what a callee keeps or returns through.
const KEPT: u32 = 0x3ff8_0000 | SP | 1 << 30;
/// X0-X18 and X30: what a call may change.
const CLOBBERED: u32 = 0x7_ffff | 1 << 30;

/// Which liveness contract applies where control leaves the function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Boundary {
    /// Everything is live where the function leaves to the engine.
    Engine,
    /// AAPCS64 at calls and returns.
    Abi,
}

/// How a basic block ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum End {
    /// It falls into the next block (a leader follows it).
    Fall,
    /// A branch or conditional branch inside the function.
    Branch,
    /// A call that returns to the next word.
    Call,
    /// A return.
    Return,
    /// A branch to another function (a tail call), or out of the image.
    Tail,
    /// An indirect branch.
    Indirect,
    /// A trap or an undecodable word.
    Trap,
}

#[derive(Debug, Clone)]
pub struct Block {
    /// The index of its first word in the function.
    pub first: usize,
    /// One past its last word.
    pub last: usize,
    pub successors: Vec<usize>,
    pub end: End,
    pub live_in: u32,
    pub live_out: u32,
    pub flags_live_in: bool,
    pub flags_live_out: bool,
    /// The deepest loop it is in: 0 outside loops.
    pub depth: u32,
}

/// A call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Call {
    pub site: u64,
    /// A direct call's target; `None` for BLR and BLRA*.
    pub target: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct Function {
    pub start: u64,
    pub insns: Vec<Insn>,
    pub blocks: Vec<Block>,
    pub calls: Vec<Call>,
    /// Direct branches to elsewhere: tail calls.
    pub tails: Vec<u64>,
    /// Every word was classified and every register effect known.
    pub known: bool,
}

impl Function {
    /// The function of `bytes` at `start`, its whole extent.
    pub fn analyze(start: u64, bytes: &[u8], boundary: Boundary) -> Self {
        let insns: Vec<Insn> = bytes
            .as_chunks::<4>()
            .0
            .iter()
            .enumerate()
            .map(|(index, word)| a64::classify(start + 4 * index as u64, u32::from_le_bytes(*word)))
            .collect();
        let count = insns.len();
        let end = start + 4 * count as u64;
        let inside = |target: u64| {
            (start..end)
                .contains(&target)
                .then(|| ((target - start) / 4) as usize)
                .filter(|_| target % 4 == 0)
        };
        let mut leader = vec![false; count + 1];
        if count != 0 {
            leader[0] = true;
        }
        for (index, insn) in insns.iter().enumerate() {
            match insn.flow {
                Flow::Next => {}
                Flow::Branch(to) | Flow::Conditional(to) => {
                    if let Some(to) = inside(to) {
                        leader[to] = true;
                    }
                    leader[index + 1] = true;
                }
                _ => leader[index + 1] = true,
            }
        }
        let mut blocks = Vec::new();
        let mut index_of = vec![usize::MAX; count + 1];
        let mut first = 0;
        for index in 1..=count {
            if leader[index] {
                index_of[first] = blocks.len();
                blocks.push(Block {
                    first,
                    last: index,
                    successors: Vec::new(),
                    end: End::Fall,
                    live_in: 0,
                    live_out: 0,
                    flags_live_in: false,
                    flags_live_out: false,
                    depth: 0,
                });
                first = index;
            }
        }
        let mut calls = Vec::new();
        let mut tails = Vec::new();
        for block in &mut blocks {
            let last = &insns[block.last - 1];
            let next = (block.last < count).then_some(block.last);
            let mut successors = Vec::new();
            block.end = match last.flow {
                Flow::Next => {
                    successors.extend(next);
                    End::Fall
                }
                Flow::Branch(to) => match inside(to) {
                    Some(to) => {
                        successors.push(to);
                        End::Branch
                    }
                    None => {
                        tails.push(to);
                        End::Tail
                    }
                },
                Flow::Conditional(to) => {
                    successors.extend(next);
                    if let Some(to) = inside(to) {
                        successors.push(to);
                    } else {
                        tails.push(to);
                    }
                    End::Branch
                }
                Flow::Call(to) => {
                    calls.push(Call {
                        site: start + 4 * (block.last - 1) as u64,
                        target: Some(to),
                    });
                    successors.extend(next);
                    End::Call
                }
                Flow::IndirectCall => {
                    calls.push(Call {
                        site: start + 4 * (block.last - 1) as u64,
                        target: None,
                    });
                    successors.extend(next);
                    End::Call
                }
                Flow::Return | Flow::ExceptionReturn => End::Return,
                Flow::Indirect => End::Indirect,
                Flow::Trap => End::Trap,
            };
            block.successors = successors
                .into_iter()
                .map(|word| index_of[word])
                .filter(|&at| at != usize::MAX)
                .collect();
        }
        let known = insns.iter().all(|insn| insn.effects.known);
        let mut function = Self {
            start,
            insns,
            blocks,
            calls,
            tails,
            known,
        };
        function.liveness(boundary);
        function.loops();
        function
    }

    /// Effects of a word under `boundary`'s contract for calls.
    fn effects(insn: &Insn, boundary: Boundary) -> Effects {
        let mut effects = insn.effects;
        if !effects.known {
            effects.reads = ALL;
            effects.writes = 0;
            effects.flags_read = true;
        }
        if boundary == Boundary::Abi && matches!(insn.flow, Flow::Call(_) | Flow::IndirectCall) {
            effects.reads |= ARGUMENTS;
            effects.writes |= CLOBBERED;
            effects.flags_write = true;
        }
        effects
    }

    /// What is live where a block's last word leaves the function.
    fn leaving(end: End, boundary: Boundary) -> (u32, bool) {
        match (boundary, end) {
            (Boundary::Engine, End::Return | End::Tail | End::Indirect | End::Trap) => (ALL, true),
            (Boundary::Abi, End::Return) => (ARGUMENTS | KEPT, false),
            (Boundary::Abi, End::Tail | End::Indirect) => (ARGUMENTS | KEPT | 1 << 8, false),
            (Boundary::Abi, End::Trap) => (0, false),
            _ => (0, false),
        }
    }

    fn liveness(&mut self, boundary: Boundary) {
        let mut changed = true;
        while changed {
            changed = false;
            for at in (0..self.blocks.len()).rev() {
                let block = &self.blocks[at];
                let (mut live, mut flags) = Self::leaving(block.end, boundary);
                // A conditional branch out of the function leaves to the
                // engine on its taken side.
                if block.end == End::Branch
                    && matches!(self.insns[block.last - 1].flow, Flow::Conditional(to)
                        if !(self.start..self.start + 4 * self.insns.len() as u64).contains(&to))
                {
                    let (out, out_flags) = Self::leaving(End::Tail, boundary);
                    live |= out;
                    flags |= out_flags;
                }
                // The function's last block may run off its end.
                if block.end == End::Fall && block.successors.is_empty() {
                    let (out, out_flags) = Self::leaving(End::Tail, boundary);
                    live |= out;
                    flags |= out_flags;
                }
                for &successor in &block.successors {
                    live |= self.blocks[successor].live_in;
                    flags |= self.blocks[successor].flags_live_in;
                }
                let live_out = live;
                let flags_out = flags;
                for insn in self.insns[block.first..block.last].iter().rev() {
                    let effects = Self::effects(insn, boundary);
                    live = (live & !effects.writes) | effects.reads;
                    if effects.flags_write {
                        flags = false;
                    }
                    flags |= effects.flags_read;
                }
                let block = &mut self.blocks[at];
                if (block.live_in, block.flags_live_in, block.live_out, block.flags_live_out)
                    != (live, flags, live_out, flags_out)
                {
                    block.live_in = live;
                    block.flags_live_in = flags;
                    block.live_out = live_out;
                    block.flags_live_out = flags_out;
                    changed = true;
                }
            }
        }
    }

    /// Loop depths from back edges (to a block at or before the source),
    /// whose loop is every block between the target and the source: exact
    /// for the reducible, laid-out-in-order loops compilers emit.
    fn loops(&mut self) {
        let mut edges = Vec::new();
        for (at, block) in self.blocks.iter().enumerate() {
            for &to in &block.successors {
                if to <= at {
                    edges.push((to, at));
                }
            }
        }
        for (to, from) in edges {
            for block in &mut self.blocks[to..=from] {
                block.depth += 1;
            }
        }
    }

    /// The registers the function reads or writes, by static count of
    /// words that touch each, weighted by 8 per loop level.
    pub fn register_uses(&self) -> [u64; 32] {
        let mut uses = [0u64; 32];
        for block in &self.blocks {
            let weight = 8u64.saturating_pow(block.depth.min(4));
            for insn in &self.insns[block.first..block.last] {
                if !insn.effects.known {
                    continue;
                }
                let mut touched = insn.effects.reads | insn.effects.writes;
                while touched != 0 {
                    let index = touched.trailing_zeros() as usize;
                    uses[index] += weight;
                    touched &= touched - 1;
                }
            }
        }
        uses
    }

    /// Flag-setting words whose NZCV no path reads before it is set again
    /// or the function leaves under `Boundary::Abi`, and all flag setters.
    pub fn dead_flag_setters(&self) -> (usize, usize) {
        let (mut dead, mut setters) = (0, 0);
        for block in &self.blocks {
            let mut flags = block.flags_live_out;
            for insn in self.insns[block.first..block.last].iter().rev() {
                let effects = Self::effects(insn, Boundary::Abi);
                if effects.flags_write {
                    // A call's clobber is no setter of its own.
                    if insn.effects.flags_write {
                        setters += 1;
                        if !flags {
                            dead += 1;
                        }
                    }
                    flags = false;
                }
                flags |= effects.flags_read;
            }
        }
        (dead, setters)
    }

    /// Whether the function makes no call.
    pub fn leaf(&self) -> bool {
        self.calls.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn function(words: &[u32], boundary: Boundary) -> Function {
        let bytes: Vec<u8> = words.iter().flat_map(|word| word.to_le_bytes()).collect();
        Function::analyze(0x1000, &bytes, boundary)
    }

    /// `strlen`-like: a loop over bytes, then a return.
    ///   0: mov x1, x0
    ///   4: ldrb w2, [x1], #1
    ///   8: cbnz w2, #-4
    ///   c: sub x0, x1, x0
    ///  10: sub x0, x0, #1
    ///  14: ret
    const LOOP: [u32; 6] = [
        0xaa00_03e1,
        0x3840_1422,
        0x35ff_ffe2,
        0xcb00_0020,
        0xd100_0400,
        0xd65f_03c0,
    ];

    #[test]
    fn blocks_edges_and_loops() {
        let f = function(&LOOP, Boundary::Abi);
        let shape: Vec<_> = f
            .blocks
            .iter()
            .map(|block| (block.first, block.last, block.successors.clone(), block.depth))
            .collect();
        assert_eq!(
            shape,
            vec![
                (0, 1, vec![1], 0),
                (1, 3, vec![2, 1], 1),
                (3, 6, vec![], 0)
            ]
        );
        assert_eq!(f.blocks[2].end, End::Return);
        assert!(f.leaf());
        assert!(f.known);
    }

    #[test]
    fn liveness_under_both_contracts() {
        let abi = function(&LOOP, Boundary::Abi);
        // Only X0 (and the kept registers) are live on entry: X1 and X2
        // are written before they are read.
        assert_eq!(abi.blocks[0].live_in & 0x7, 1);
        assert_eq!(abi.blocks[0].live_in & KEPT, KEPT);
        let engine = function(&LOOP, Boundary::Engine);
        // At the engine's boundary every register is live at the return,
        // so X9 is live on entry; under the contract it is not.
        assert_eq!(engine.blocks[0].live_in & 0x7, 1);
        assert_ne!(engine.blocks[0].live_in & 1 << 9, 0);
        assert_eq!(abi.blocks[0].live_in & 1 << 9, 0);
        // The loop's registers outweigh the rest.
        let uses = abi.register_uses();
        assert!(uses[1] > uses[0] && uses[2] > uses[0]);
    }

    #[test]
    fn dead_flags_and_calls() {
        // cmp x0, #1; cmp x0, #2; b.eq #8; bl #0x100; ret
        let words = [0xf100_041f, 0xf100_081f, 0x5400_0040, 0x9400_0040, 0xd65f_03c0];
        let f = function(&words, Boundary::Abi);
        assert_eq!(f.dead_flag_setters(), (1, 2));
        assert_eq!(
            f.calls,
            vec![Call {
                site: 0x100c,
                target: Some(0x110c)
            }]
        );
        assert!(!f.leaf());
    }
}
