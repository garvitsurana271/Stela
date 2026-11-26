//! Typed register/spill recovery and atomic guest retirement settlement.
mod packed;
mod qualification;
mod retirement;
mod sites;
mod spills;
use super::HostGprSnapshot;
use crate::GuestAddress;
pub use packed::PackedRecovery;
pub use qualification::{RecoveryQualificationError, RecoveryTemplate, UnqualifiedFaultRecovery};
pub use retirement::{RecoveredPosition, RetirementPhase, RetirementPoint};
pub use sites::{RecoverySettlement, RecoverySite, RecoverySiteKind, TranslatedAccess};
pub use spills::{HostSpillSnapshot, InvalidSpillOffset, NATIVE_SPILL_WORDS, SpillOffset};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoverySource {
    HostRegister(u8),
    NativeSpill(SpillOffset),
    /// NZCV, which the setter recomputes ([`NZCV_BINDING`]).
    Flags(FlagSetter),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoveryBinding {
    pub guest: u8,
    pub source: RecoverySource,
}
/// The first [`RecoveryBinding::guest`] number of a guest V register.
pub const VECTOR_BINDING: u8 = 32;

/// [`RecoveryBinding::guest`] of NZCV, which a [`RecoverySource::Flags`]
/// recomputes.
pub const NZCV_BINDING: u8 = 64;

/// A flag-setting instruction, which recomputes the NZCV it set from the
/// guest registers it read: native code that leaves a setter's NZCV
/// unstored up to the next instruction that defines all four flags binds it
/// at the faults between, where those registers still hold what the setter
/// read (`x86_64::block`'s flag windows).
///
/// Bits 1:0 the operation (ADDS, SUBS, ANDS, BICS), 2 a 64-bit operation,
/// 3 whether the left register holds the setter's result rather than its
/// left operand (an ADDS or SUBS whose destination is its left operand, or
/// an ANDS or BICS that wrote a register), 9:4 the left register, 10 a
/// register right operand. That register is in 16:11, its shift in 18:17
/// and the amount in 24:19; an immediate takes bits 63:11. A register is
/// 0-30 for X0-X30, 31 for SP and 32 for the zero register.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlagSetter(u64);

/// The operation of a [`FlagSetter`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlagOperation {
    Add,
    Subtract,
    And,
    AndNot,
}

/// The right operand of a [`FlagSetter`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlagOperand {
    Immediate(u64),
    /// A register, shifted (0 LSL, 1 LSR, 2 ASR, 3 ROR) by `amount`.
    Register {
        register: u8,
        shift: u8,
        amount: u8,
    },
}

/// The register number a [`FlagSetter`] gives the zero register.
pub const FLAG_ZERO_REGISTER: u8 = 32;
const FLAG_IMMEDIATE_BITS: u32 = 53;

impl FlagSetter {
    /// The setter of `operation` on `left` and `right`, whose `left` holds
    /// its result where `from_result`; `None` for an operand it cannot hold.
    pub const fn new(
        operation: FlagOperation,
        x64: bool,
        from_result: bool,
        left: u8,
        right: FlagOperand,
    ) -> Option<Self> {
        if left > FLAG_ZERO_REGISTER {
            return None;
        }
        let mut bits = operation as u64 | (x64 as u64) << 2 | (from_result as u64) << 3;
        bits |= (left as u64) << 4;
        match right {
            FlagOperand::Immediate(value) => {
                if value >> FLAG_IMMEDIATE_BITS != 0 {
                    return None;
                }
                bits |= value << 11;
            }
            FlagOperand::Register {
                register,
                shift,
                amount,
            } => {
                if register > FLAG_ZERO_REGISTER || shift > 3 || amount >= 64 {
                    return None;
                }
                bits |= 1 << 10
                    | (register as u64) << 11
                    | (shift as u64) << 17
                    | (amount as u64) << 19;
            }
        }
        Some(Self(bits))
    }

    /// The setter whose bits [`Self::bits`] gave.
    pub const fn from_bits(bits: u64) -> Self {
        Self(bits)
    }

    pub const fn bits(self) -> u64 {
        self.0
    }

    /// The Arm NZCV (bits 31:28) the setter set, from `state`'s registers;
    /// `None` for bits no setter has.
    pub fn nzcv(self, state: &crate::GuestState) -> Option<u32> {
        let bits = self.0;
        let x64 = bits >> 2 & 1 == 1;
        let from_result = bits >> 3 & 1 == 1;
        let size = if x64 { 64 } else { 32 };
        let mask = if x64 { u64::MAX } else { u64::from(u32::MAX) };
        let value = |register: u64| match register {
            0..=30 => Some(state.x[register as usize] & mask),
            31 => Some(state.sp & mask),
            32 => Some(0),
            _ => None,
        };
        let left = value(bits >> 4 & 0x3f)?;
        let right = if bits >> 10 & 1 == 1 {
            let register = value(bits >> 11 & 0x3f)?;
            let amount = (bits >> 19 & 0x3f) as u32;
            if amount >= size {
                return None;
            }
            match bits >> 17 & 3 {
                0 => register << amount & mask,
                1 => register >> amount,
                2 if x64 => ((register as i64) >> amount) as u64,
                2 => u64::from(((register as u32 as i32) >> amount) as u32),
                _ if x64 => register.rotate_right(amount),
                _ => u64::from((register as u32).rotate_right(amount)),
            }
        } else {
            bits >> 11
        };
        let (result, carry, overflow) = match bits & 3 {
            operation @ (0 | 1) => {
                let subtract = operation == 1;
                let left = match (from_result, subtract) {
                    (false, _) => left,
                    (true, true) => left.wrapping_add(right) & mask,
                    (true, false) => left.wrapping_sub(right) & mask,
                };
                // Arm AddWithCarry, with NOT(right) and a carry in of one
                // for a subtraction.
                let addend = if subtract { !right & mask } else { right };
                let wide = u128::from(left) + u128::from(addend) + u128::from(subtract);
                let result = wide as u64 & mask;
                let sign = 1_u64 << (size - 1);
                (
                    result,
                    wide >> size & 1 == 1,
                    !(left ^ addend) & (left ^ result) & sign != 0,
                )
            }
            operation => {
                let result = match (from_result, operation) {
                    (true, _) => left,
                    (false, 2) => left & right,
                    (false, _) => left & !right & mask,
                };
                (result, false, false)
            }
        };
        let negative = result >> (size - 1) & 1 == 1;
        Some(
            u32::from(negative) << 31
                | u32::from(result == 0) << 30
                | u32::from(carry) << 29
                | u32::from(overflow) << 28,
        )
    }
}

impl RecoveryBinding {
    /// Guest V`vector`, whose only current value is host XMM`register`
    /// (a system-mode block's home, 8 to 15).
    pub const fn vector(vector: u8, register: u8) -> Self {
        Self {
            guest: VECTOR_BINDING + vector,
            source: RecoverySource::HostRegister(register),
        }
    }
    /// Whether the binding names a guest register and a source that can
    /// hold it: X0-X30/SP from a host GPR or a spill, V0-V31 from XMM8-15.
    pub const fn is_valid(&self) -> bool {
        match (self.guest, self.source) {
            (0..=31, RecoverySource::HostRegister(host)) => host < 32,
            (0..=31, RecoverySource::NativeSpill(_)) => true,
            (32..=63, RecoverySource::HostRegister(host)) => host >= 8 && host < 16,
            (NZCV_BINDING, RecoverySource::Flags(_)) => true,
            _ => false,
        }
    }
    pub const fn gpr(guest: u8, register: u8) -> Self {
        Self {
            guest,
            source: RecoverySource::HostRegister(register),
        }
    }
    /// NZCV, which `setter` recomputes.
    pub const fn flags(setter: FlagSetter) -> Self {
        Self {
            guest: NZCV_BINDING,
            source: RecoverySource::Flags(setter),
        }
    }
    pub const fn spill(guest: u8, offset: SpillOffset) -> Self {
        Self {
            guest,
            source: RecoverySource::NativeSpill(offset),
        }
    }
}

/// Capture data from one pinned invocation and its matching code/plan epoch.
/// Native-stack permission remains with the platform capture owner.
#[derive(Debug, Clone, Copy)]
pub struct HostFaultSnapshot<'a> {
    pub gprs: &'a HostGprSnapshot,
    pub spills: &'a HostSpillSnapshot,
}

/// Native interval with a single complete guest retirement boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FaultRecovery {
    pub start: usize,
    pub end: usize,
    pub bindings: alloc::vec::Vec<RecoveryBinding>,
    pub retirement: RetirementPoint,
    pub site: RecoverySite,
}
impl FaultRecovery {
    /// Classify before attempting architectural settlement. An absent row is a
    /// host-internal fault. Ordinary frame/commit intervals do not authorize
    /// guest projection because their location metadata says Before.
    pub fn site_at(table: &[Self], offset: usize) -> Result<RecoverySite, FaultRecoveryError> {
        let mut site = None;
        for entry in table {
            if entry.start >= entry.end {
                return Err(FaultRecoveryError::InvalidNativeRange {
                    start: entry.start,
                    end: entry.end,
                });
            }
            if !(entry.start..entry.end).contains(&offset) {
                continue;
            }
            if entry.site.kind() != RecoverySiteKind::NativeInternal
                && entry.retirement.phase() != RetirementPhase::Before
            {
                return Err(FaultRecoveryError::InvalidGuestSitePhase {
                    site: entry.site.kind(),
                    phase: entry.retirement.phase(),
                });
            }
            if site.is_some_and(|site| site != entry.site) {
                return Err(FaultRecoveryError::ConflictingRecoverySite);
            }
            site = Some(entry.site);
        }
        Ok(site.unwrap_or(RecoverySite::native_internal()))
    }
    /// Required initialized spill words at this native offset. The capture
    /// owner validates each address against its mapped invocation lease.
    pub fn required_spills(
        table: &[Self],
        offset: usize,
    ) -> impl Iterator<Item = SpillOffset> + '_ {
        table
            .iter()
            .filter(move |entry| (entry.start..entry.end).contains(&offset))
            .flat_map(|entry| entry.bindings.iter())
            .filter_map(|binding| match binding.source {
                RecoverySource::NativeSpill(offset) => Some(offset),
                _ => None,
            })
    }
    /// Validate the entire active settlement before changing any guest value.
    /// PC and retirement count are returned together, never inferred from a
    /// partially executed native recipe or an unavailable capture.
    pub fn apply(
        table: &[Self],
        offset: usize,
        capture: HostFaultSnapshot<'_>,
        state: &mut crate::GuestState,
    ) -> Result<Option<RecoverySettlement>, FaultRecoveryError> {
        let mut settlement: Option<RecoverySettlement> = None;
        let mut values = [0; 32];
        let mut present = 0u32;
        let mut vectors = [[0; 2]; 32];
        let mut vectors_present = 0u32;
        let mut flags: Option<FlagSetter> = None;
        for entry in table {
            if entry.start >= entry.end {
                return Err(FaultRecoveryError::InvalidNativeRange {
                    start: entry.start,
                    end: entry.end,
                });
            }
            if !(entry.start..entry.end).contains(&offset) {
                continue;
            }
            let at = entry.retirement.position()?;
            let phase = entry.retirement.phase();
            if entry.site.kind() != RecoverySiteKind::NativeInternal
                && phase != RetirementPhase::Before
            {
                return Err(FaultRecoveryError::InvalidGuestSitePhase {
                    site: entry.site.kind(),
                    phase,
                });
            }
            if let Some(previous) = settlement {
                if previous.position != at || previous.phase != phase {
                    return Err(FaultRecoveryError::InconsistentRetirement);
                }
                if previous.site != entry.site {
                    return Err(FaultRecoveryError::ConflictingRecoverySite);
                }
            }
            settlement = Some(RecoverySettlement {
                position: at,
                site: entry.site,
                phase,
            });
            for binding in &entry.bindings {
                if let RecoverySource::Flags(setter) = binding.source {
                    if !binding.is_valid() {
                        return Err(FaultRecoveryError::InvalidGuestRegister {
                            register: binding.guest,
                        });
                    }
                    if flags.is_some_and(|flags| flags != setter) {
                        return Err(FaultRecoveryError::ConflictingGuestBinding {
                            register: binding.guest,
                        });
                    }
                    flags = Some(setter);
                    continue;
                }
                if binding.guest >= VECTOR_BINDING {
                    let vector = binding.guest - VECTOR_BINDING;
                    if !binding.is_valid() {
                        return Err(FaultRecoveryError::InvalidGuestRegister {
                            register: binding.guest,
                        });
                    }
                    let RecoverySource::HostRegister(register) = binding.source else {
                        unreachable!("checked by is_valid");
                    };
                    let value = capture
                        .gprs
                        .vector(register)
                        .ok_or(FaultRecoveryError::VectorStateUnavailable { register })?;
                    let bit = 1 << vector;
                    if vectors_present & bit != 0 && vectors[usize::from(vector)] != value {
                        return Err(FaultRecoveryError::ConflictingGuestBinding {
                            register: binding.guest,
                        });
                    }
                    vectors[usize::from(vector)] = value;
                    vectors_present |= bit;
                    continue;
                }
                if binding.guest > 31 {
                    return Err(FaultRecoveryError::InvalidGuestRegister {
                        register: binding.guest,
                    });
                }
                let value = match binding.source {
                    RecoverySource::HostRegister(register) => {
                        if register >= 32 {
                            return Err(FaultRecoveryError::InvalidHostRegister { register });
                        }
                        capture
                            .gprs
                            .get(register)
                            .ok_or(FaultRecoveryError::ExtendedStateUnavailable { register })?
                    }
                    RecoverySource::NativeSpill(offset) => capture
                        .spills
                        .get(offset)
                        .ok_or(FaultRecoveryError::NativeSpillUnavailable { offset })?,
                    RecoverySource::Flags(_) => unreachable!("taken above"),
                };
                let slot = &mut values[usize::from(binding.guest)];
                let bit = 1 << binding.guest;
                if present & bit != 0 && *slot != value {
                    return Err(FaultRecoveryError::ConflictingGuestBinding {
                        register: binding.guest,
                    });
                }
                *slot = value;
                present |= bit;
            }
        }
        while vectors_present != 0 {
            let vector = vectors_present.trailing_zeros() as usize;
            vectors_present &= vectors_present - 1;
            state.v[vector] = vectors[vector];
        }
        while present != 0 {
            let guest = present.trailing_zeros() as usize;
            present &= present - 1;
            if guest == 31 {
                state.sp = values[guest];
            } else {
                state.x[guest] = values[guest];
            }
        }
        // From the registers as the fault left them, which still hold what
        // the setter read.
        if let Some(setter) = flags {
            state.nzcv = setter
                .nzcv(state)
                .ok_or(FaultRecoveryError::InvalidFlagSetter { bits: setter.0 })?;
        }
        Ok(settlement)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultRecoveryError {
    InvalidGuestRegister {
        register: u8,
    },
    InvalidHostRegister {
        register: u8,
    },
    ExtendedStateUnavailable {
        register: u8,
    },
    VectorStateUnavailable {
        register: u8,
    },
    NativeSpillUnavailable {
        offset: SpillOffset,
    },
    InvalidNativeRange {
        start: usize,
        end: usize,
    },
    InvalidInstructionAddress {
        pc: GuestAddress,
    },
    RetirementOverflow {
        pc: GuestAddress,
    },
    UnprovenFallthrough {
        pc: GuestAddress,
    },
    InvalidRetiredCount {
        count: u16,
    },
    InconsistentRetirement,
    UnknownRecoverySite {
        tag: u8,
    },
    InvalidGuestSitePhase {
        site: RecoverySiteKind,
        phase: RetirementPhase,
    },
    ConflictingRecoverySite,
    ConflictingGuestBinding {
        register: u8,
    },
    /// A flag binding whose bits name no setter.
    InvalidFlagSetter {
        bits: u64,
    },
}
impl core::fmt::Display for FaultRecoveryError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::InvalidGuestRegister { register } => {
                write!(f, "fault recovery names invalid guest register {register}")
            }
            Self::InvalidHostRegister { register } => {
                write!(f, "fault recovery names invalid host register {register}")
            }
            Self::ExtendedStateUnavailable { register } => write!(
                f,
                "fault recovery needs host R{register}, but its extended bank was not captured"
            ),
            Self::VectorStateUnavailable { register } => write!(
                f,
                "fault recovery needs host XMM{register}, which was not captured"
            ),
            Self::NativeSpillUnavailable { offset } => write!(
                f,
                "fault recovery needs unavailable native spill at RSP+{}",
                offset.bytes()
            ),
            Self::InvalidNativeRange { start, end } => {
                write!(f, "invalid native recovery range {start}..{end}")
            }
            Self::InvalidInstructionAddress { pc } => {
                write!(f, "unaligned recovery instruction PC {:#x}", pc.0)
            }
            Self::RetirementOverflow { pc } => {
                write!(f, "recovery retirement overflows at PC {:#x}", pc.0)
            }
            Self::UnprovenFallthrough { pc } => write!(
                f,
                "instruction at PC {:#x} has no proven fallthrough recovery",
                pc.0
            ),
            Self::InvalidRetiredCount { count } => {
                write!(f, "recovery retired count {count} exceeds the block bound")
            }
            Self::InconsistentRetirement => {
                f.write_str("overlapping recovery entries disagree on retirement")
            }
            Self::UnknownRecoverySite { tag } => {
                write!(f, "unknown native recovery site tag {tag}")
            }
            Self::InvalidGuestSitePhase { site, phase } => write!(
                f,
                "guest recovery site {site:?} cannot have phase {phase:?}"
            ),
            Self::ConflictingRecoverySite => {
                f.write_str("overlapping recovery entries disagree on native fault site")
            }
            Self::ConflictingGuestBinding { register } => {
                write!(f, "recovery entries disagree on guest register {register}")
            }
            Self::InvalidFlagSetter { bits } => {
                write!(f, "fault recovery names no flag setter in {bits:#x}")
            }
        }
    }
}
impl core::error::Error for FaultRecoveryError {}

#[cfg(test)]
mod tests;
