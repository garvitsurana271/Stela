//! High-level emulation (HLE): native x86-64 routines that replace guest
//! library functions the engine finds by their bytes (`stela_engine::hle`).
//! The routines are named here, where the composite blocks they are emitted
//! into are laid out, so the engine's signatures and the compiler's
//! emitters share one id.
//!
//! A composite block ([`compose`]) is a routine's fast path, then the
//! function's first instruction compiled alone, its bail:
//!
//! - Bytes `[0, prefix_end)` are the fast path, padded with INT3 to a
//!   multiple of [`PREFIX_ALIGN`]. It reads X0 in R15 and X1 in RBP and
//!   writes no guest register or frame field before its commit point. Each
//!   way it gives up (a size, range or overlap miss) is a `jmp` to
//!   `prefix_end`. On success it takes a direct exit to the function's own
//!   `ret`, after a forward link slot.
//! - Bytes from `prefix_end` are the bail's code unchanged, its direct
//!   exits, fault recovery rows and cold code moved by `prefix_end`. As
//!   `prefix_end` is a multiple of 8, the patch points keep the residues
//!   the bail was padded for.
//!
//! The composite retires as one instruction, the first: its source map
//! names it once, at the fast path, and its source bytes are that word's. A
//! fault in the fast path leaves the entry's guest state, for the engine to
//! run the original once and take it (`stela_engine`'s `hle_fault`).
mod compare;
mod copy;
mod emit;
mod fill;
mod scan;

use super::asm::{Code, Gpr};
use super::chain::DirectExit;
use super::pinned::PINNED;
use crate::aarch64::Register;
use crate::{GuestAddress, NativeBlock, TargetCpu, block::CompiledBlock};
use alloc::vec;
use alloc::vec::Vec;

/// A native routine, by the C function whose contract it keeps. Its bit in a
/// configuration's enabled set is [`Self::bit`]; its name is the one
/// `--hle-off` takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Routine {
    Memmove,
    Bzero,
    Memset,
    Memcmp,
    Strlen,
    /// For tests: a fast path that only bails to the original.
    #[cfg(any(test, feature = "test-support"))]
    Bail,
    /// For tests: a fast path that loads one byte at X0, then bails.
    #[cfg(any(test, feature = "test-support"))]
    Touch,
}

impl Routine {
    /// The routines a boot can enable, in bit order.
    pub const PRODUCTION: [Routine; 5] = [
        Self::Memmove,
        Self::Bzero,
        Self::Memset,
        Self::Memcmp,
        Self::Strlen,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Self::Memmove => "memmove",
            Self::Bzero => "bzero",
            Self::Memset => "memset",
            Self::Memcmp => "memcmp",
            Self::Strlen => "strlen",
            #[cfg(any(test, feature = "test-support"))]
            Self::Bail => "bail",
            #[cfg(any(test, feature = "test-support"))]
            Self::Touch => "touch",
        }
    }

    /// The production routine named `name`: never a test routine.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::PRODUCTION
            .into_iter()
            .find(|routine| routine.name() == name)
    }

    /// Its bit in a set of routines: `1 << index`.
    pub const fn bit(self) -> u32 {
        1 << self as u32
    }
}

/// What a fast path's length is a multiple of: a bail moved by it keeps its
/// patch points' residues mod 8 (`chain::patchable`).
pub const PREFIX_ALIGN: u32 = 64;
/// The longest fast path a composite holds, 255 multiples of
/// [`PREFIX_ALIGN`]: what the engine's mark of a composite names.
pub const MAX_PREFIX_BYTES: u32 = 255 * PREFIX_ALIGN;
/// The bounds of [`Request::max_bytes`].
pub const MAX_BYTES: core::ops::RangeInclusive<u32> = 256..=1 << 20;

/// The bytes of one guest instruction, a bail's.
const INSTRUCTION_BYTES: usize = 4;
/// The fetch page a composite, its function and its exit lie in.
const PAGE: u64 = 4096;
/// What pads a fast path: INT3, which no path reaches.
const INT3: u8 = 0xcc;

// A fast path reads X0 in R15 and X1 in RBP, and X2 at the frame's byte
// 16 (the frame's X array first): an upstream change to the pinned map or
// the frame breaks the build. `link_slot` and `offset` are not `const`, so
// the forward link slot and X2's slot are pinned by tests.
const fn pins(guest: u8, host: Gpr) -> bool {
    let mut at = 0;
    while at < PINNED.len() {
        if let (Register::X(index), pinned) = PINNED[at]
            && index == guest
            && pinned.number() == host.number()
        {
            return true;
        }
        at += 1;
    }
    false
}
const _: () = assert!(pins(0, Gpr::R15) && pins(1, Gpr::Rbp));
const _: () = assert!(core::mem::offset_of!(crate::GuestState, x) == 0);

/// X2's frame slot, where a fast path reads its third argument.
fn x2() -> super::asm::Mem {
    super::frame(2 * 8)
}

/// The composite [`compose`] lays out: which routine, at which function
/// entry, with its exit `ret`, and the most bytes one call may work on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Request {
    pub routine: Routine,
    /// The function's entry, the composite's PC.
    pub pc: GuestAddress,
    /// The function's exit `ret`, where a call the fast path completes goes.
    pub exit: GuestAddress,
    /// A call on more bytes bails ([`MAX_BYTES`] bounds it).
    pub max_bytes: u32,
    /// The host the composite runs on, whose widest vectors (SSE2, AVX2 or
    /// AVX-512F) the fast path moves, fills and compares with.
    pub target: TargetCpu,
}

/// A composite block and where its fast path ends, the bail's code starts.
#[derive(Debug, Clone)]
pub struct Composite {
    pub block: CompiledBlock,
    /// A multiple of [`PREFIX_ALIGN`], at most [`MAX_PREFIX_BYTES`].
    pub prefix_end: u32,
}

/// Why [`compose`] laid out no composite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The bail is not one instruction's plain block: more or fewer source
    /// bytes, more source-map entries, or a self-loop or region.
    BailNotOneInstruction,
    /// The exit is not in the entry's 4 KiB page.
    ExitOutsidePage,
    /// The exit is the entry.
    ExitIsEntry,
    /// `max_bytes` is outside [`MAX_BYTES`].
    MaxBytes,
    /// The fast path is longer than [`MAX_PREFIX_BYTES`].
    PrefixTooLong,
}

/// The composite of `request`'s routine at its entry, in front of `bail`,
/// the entry's first instruction compiled alone as the engine compiles a
/// block of the variant (its direct exits with link slots for the block at
/// `request.pc`).
pub fn compose(request: Request, bail: CompiledBlock) -> Result<Composite, Refusal> {
    let Request { pc, exit, .. } = request;
    if bail.source_bytes != INSTRUCTION_BYTES
        || bail.native.source_map.len() != 1
        || bail.self_loop
        || bail.region
    {
        return Err(Refusal::BailNotOneInstruction);
    }
    if exit == pc {
        return Err(Refusal::ExitIsEntry);
    }
    if (exit.0 ^ pc.0) & !(PAGE - 1) != 0 {
        return Err(Refusal::ExitOutsidePage);
    }
    if !MAX_BYTES.contains(&request.max_bytes) {
        return Err(Refusal::MaxBytes);
    }
    let mut code = Code::pinned();
    code.set_linked_block(pc.0);
    let mut bails = emit::Bails::default();
    let mut exits = fast_path(&mut code, request, &mut bails)?;
    let align = PREFIX_ALIGN as usize;
    let prefix_end = code.len().next_multiple_of(align).max(align);
    if prefix_end > MAX_PREFIX_BYTES as usize {
        return Err(Refusal::PrefixTooLong);
    }
    bails.bind(&mut code, prefix_end);
    let (mut bytes, _) = code.into_parts();
    bytes.resize(prefix_end, INT3);
    let CompiledBlock {
        native,
        direct_exits,
        mut fault_recovery,
        cold,
        ..
    } = bail;
    bytes.extend_from_slice(&native.x86_64_code);
    exits.extend(direct_exits.iter().map(|exit| DirectExit {
        offset: exit.offset + prefix_end,
        ..*exit
    }));
    for row in &mut fault_recovery {
        row.start += prefix_end;
        row.end += prefix_end;
    }
    let shift = prefix_end as u32;
    let cold = match cold {
        (0, 0) => (0, 0),
        (start, end) => (start + shift, end + shift),
    };
    super::block::align_exits(&mut bytes, &exits, Some(pc.0), None);
    Ok(Composite {
        block: CompiledBlock {
            native: NativeBlock {
                x86_64_code: bytes,
                source_map: vec![(pc, 0)],
                // Never stored, offered or rebased.
                relocations: None,
            },
            source_bytes: INSTRUCTION_BYTES,
            direct_exits: exits,
            fault_recovery,
            self_loop: false,
            region: false,
            cold,
            position_independent: false,
        },
        prefix_end: shift,
    })
}

/// Emit `request`'s fast path into `code`, its bails into `bails`, and
/// give its direct exits.
fn fast_path(
    code: &mut Code,
    request: Request,
    bails: &mut emit::Bails,
) -> Result<Vec<DirectExit>, Refusal> {
    match request.routine {
        #[cfg(any(test, feature = "test-support"))]
        Routine::Bail => {
            bails.jump(code);
            Ok(Vec::new())
        }
        #[cfg(any(test, feature = "test-support"))]
        Routine::Touch => {
            use super::asm::{Mem, Size};
            code.asm().mov_imm32(Gpr::Rcx, 1);
            emit::checked(code, Gpr::R15, Gpr::Rcx, Gpr::R8, bails);
            code.asm()
                .movzx(Size::Dword, Gpr::Rax, Size::Byte, Mem::base(Gpr::R8, 0));
            bails.jump(code);
            // Never reached: a success exit's layout, for its tests.
            Ok(vec![emit::success(code, request.exit)])
        }
        Routine::Memmove => Ok(copy::memmove(code, request, bails)),
        Routine::Bzero => Ok(fill::fill(code, request, fill::Fill::Bzero, bails)),
        Routine::Memset => Ok(fill::fill(code, request, fill::Fill::Memset, bails)),
        Routine::Memcmp => Ok(compare::memcmp(code, request, bails)),
        Routine::Strlen => Ok(scan::strlen(code, request, bails)),
    }
}

#[cfg(test)]
mod tests;
