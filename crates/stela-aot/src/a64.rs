//! A word-level classifier of A64 instructions for whole-image analysis:
//! each word's control flow, and which general registers and NZCV it reads
//! and writes. It is not the translator's decoder (`stela_compiler`'s
//! `aarch64`), which lifts semantics: this one only says what an analysis
//! of a function needs, for every encoding class, and marks what it does
//! not know (`Effects::known`), which an analysis then treats as reading
//! every register and writing none.
//!
//! Register masks have bit `n` for Xn (n < 31) and bit 31 for SP. XZR is in
//! no mask: reading it reads nothing and writing it writes nothing.

/// SP's bit in a register mask.
pub const SP: u32 = 1 << 31;
/// Every general register and SP.
pub const ALL: u32 = u32::MAX;

/// How control leaves an instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    /// To the next word.
    Next,
    /// B, or B.cond with an always-true condition: to the target only.
    Branch(u64),
    /// B.cond, CBZ, CBNZ, TBZ, TBNZ: to the target or the next word.
    Conditional(u64),
    /// BL: to the target, returning to the next word.
    Call(u64),
    /// BLR and BLRA*: to a register, returning to the next word.
    IndirectCall,
    /// BR and BRA*: to a register (a jump table or a tail call).
    Indirect,
    /// RET, RETAA, RETAB.
    Return,
    /// ERET, ERETAA, ERETAB.
    ExceptionReturn,
    /// BRK, HLT, UDF and other words that never continue.
    Trap,
}

impl Flow {
    /// Whether the next word may run after this one.
    pub const fn falls_through(self) -> bool {
        matches!(
            self,
            Flow::Next | Flow::Conditional(_) | Flow::Call(_) | Flow::IndirectCall
        )
    }
}

/// What an instruction does with the general registers, NZCV and memory.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Effects {
    pub reads: u32,
    pub writes: u32,
    pub flags_read: bool,
    pub flags_write: bool,
    pub memory: bool,
    /// A PAC sign, authentication or strip.
    pub pac: bool,
    /// A System instruction, exception-generating instruction or barrier.
    pub system: bool,
    /// An Advanced SIMD or floating-point instruction.
    pub simd: bool,
    /// The classifier knows this encoding's register effects.
    pub known: bool,
}

/// One classified word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Insn {
    pub word: u32,
    pub flow: Flow,
    pub effects: Effects,
}

/// Xn's bit, or SP's for 31 where the field names SP, or none (XZR).
const fn reg(n: u32, sp: bool) -> u32 {
    if n == 31 {
        if sp { SP } else { 0 }
    } else {
        1 << n
    }
}

const fn sign_extend(value: u32, bits: u32) -> i64 {
    ((value as i64) << (64 - bits)) >> (64 - bits)
}

fn target(pc: u64, field: u32, bits: u32) -> u64 {
    pc.wrapping_add_signed(sign_extend(field, bits) * 4)
}

/// Classify the word `word` at `pc`.
pub fn classify(pc: u64, word: u32) -> Insn {
    let w = word;
    let rd = w & 31;
    let rn = (w >> 5) & 31;
    let rm = (w >> 16) & 31;
    let mut fx = Effects {
        known: true,
        ..Effects::default()
    };
    let mut flow = Flow::Next;
    let insn = |flow, effects| Insn {
        word,
        flow,
        effects,
    };

    // Branches, exception generation and System.
    if w & 0x7c00_0000 == 0x1400_0000 {
        let to = target(pc, w & 0x03ff_ffff, 26);
        if w & 0x8000_0000 != 0 {
            fx.writes = reg(30, false);
            return insn(Flow::Call(to), fx);
        }
        return insn(Flow::Branch(to), fx);
    }
    if w & 0xff00_0010 == 0x5400_0000 {
        let to = target(pc, (w >> 5) & 0x7_ffff, 19);
        let condition = w & 15;
        if condition >= 14 {
            return insn(Flow::Branch(to), fx);
        }
        fx.flags_read = true;
        return insn(Flow::Conditional(to), fx);
    }
    if w & 0x7e00_0000 == 0x3400_0000 {
        fx.reads = reg(rd, false);
        return insn(Flow::Conditional(target(pc, (w >> 5) & 0x7_ffff, 19)), fx);
    }
    if w & 0x7e00_0000 == 0x3600_0000 {
        fx.reads = reg(rd, false);
        return insn(Flow::Conditional(target(pc, (w >> 5) & 0x3fff, 14)), fx);
    }
    if w & 0xfe00_0000 == 0xd600_0000 {
        let opc = (w >> 21) & 15;
        let op3 = (w >> 10) & 63;
        // The register forms authenticate with Xm (or SP) as the modifier.
        let modifier = if op3 & 2 != 0 && rd != 31 {
            reg(rd, true)
        } else {
            0
        };
        fx.pac = op3 & 2 != 0;
        match opc {
            0 | 8 => {
                fx.reads = reg(rn, false) | modifier;
                flow = Flow::Indirect;
            }
            1 | 9 => {
                fx.reads = reg(rn, false) | modifier;
                fx.writes = reg(30, false);
                flow = Flow::IndirectCall;
            }
            2 => {
                fx.reads = if fx.pac {
                    reg(30, false) | SP
                } else {
                    reg(rn, false)
                };
                flow = Flow::Return;
            }
            4 => {
                fx.system = true;
                flow = Flow::ExceptionReturn;
            }
            _ => {
                fx.known = false;
                fx.reads = ALL;
                flow = Flow::Trap;
            }
        }
        return insn(flow, fx);
    }
    if w & 0xff00_0000 == 0xd400_0000 {
        fx.system = true;
        let opc = (w >> 21) & 7;
        let ll = w & 3;
        // SVC, HVC and SMC return to the next word; BRK, HLT and DCPS*
        // do not continue here.
        if opc == 0 && ll != 0 {
            // The handler may read and write any register.
            fx.reads = ALL;
            fx.writes = 0;
            fx.known = false;
            return insn(Flow::Next, fx);
        }
        return insn(Flow::Trap, fx);
    }
    if w & 0xffff_0000 == 0 {
        // UDF.
        return insn(Flow::Trap, fx);
    }
    if w & 0xffc0_0000 == 0xd500_0000 {
        fx.system = true;
        if w & 0xffff_f01f == 0xd503_201f {
            let hint = (w >> 5) & 0x7f;
            let lr = reg(30, false);
            match hint {
                // PACIASP, PACIBSP, AUTIASP, AUTIBSP.
                0x19 | 0x1b | 0x1d | 0x1f => {
                    fx.pac = true;
                    fx.reads = lr | SP;
                    fx.writes = lr;
                }
                // PACIAZ, PACIBZ, AUTIAZ, AUTIBZ, XPACLRI.
                0x18 | 0x1a | 0x1c | 0x1e | 0x07 => {
                    fx.pac = true;
                    fx.reads = lr;
                    fx.writes = lr;
                }
                // PACIA1716, PACIB1716, AUTIA1716, AUTIB1716.
                0x08 | 0x0a | 0x0c | 0x0e => {
                    fx.pac = true;
                    fx.reads = reg(17, false) | reg(16, false);
                    fx.writes = reg(17, false);
                }
                // NOP, YIELD, WFE, WFI, SEV, BTI, CSDB and the other hints.
                _ => {}
            }
            return insn(flow, fx);
        }
        let l = w & (1 << 21) != 0;
        let op0 = (w >> 19) & 3;
        if op0 >= 2 {
            // MRS / MSR (register).
            if l {
                fx.writes = reg(rd, false);
            } else {
                fx.reads = reg(rd, false);
                // MSR NZCV, Xt.
                if w & 0xffff_ffe0 == 0xd51b_4200 {
                    fx.flags_write = true;
                }
            }
            if w & 0xffff_ffe0 == 0xd53b_4200 {
                fx.flags_read = true;
            }
            return insn(flow, fx);
        }
        if op0 == 1 {
            // SYS / SYSL.
            if l {
                fx.writes = reg(rd, false);
            } else {
                fx.reads = reg(rd, false);
            }
            fx.memory = true;
            return insn(flow, fx);
        }
        // MSR (immediate), barriers, CFINV, XAFLAG, AXFLAG.
        if w & 0xffff_f0ff == 0xd500_401f {
            fx.flags_read = true;
            fx.flags_write = true;
        }
        return insn(flow, fx);
    }

    // Data processing, immediate.
    if w & 0x1f00_0000 == 0x1000_0000 {
        fx.writes = reg(rd, false);
        return insn(flow, fx);
    }
    if w & 0x1f80_0000 == 0x1100_0000 {
        let s = w & (1 << 29) != 0;
        fx.reads = reg(rn, true);
        fx.writes = reg(rd, !s);
        fx.flags_write = s;
        return insn(flow, fx);
    }
    if w & 0x1f80_0000 == 0x1180_0000 {
        // ADDG / SUBG.
        fx.reads = reg(rn, true);
        fx.writes = reg(rd, true);
        return insn(flow, fx);
    }
    if w & 0x1f80_0000 == 0x1200_0000 {
        let s = (w >> 29) & 3 == 3;
        fx.reads = reg(rn, false);
        fx.writes = reg(rd, !s);
        fx.flags_write = s;
        return insn(flow, fx);
    }
    if w & 0x1f80_0000 == 0x1280_0000 {
        // MOVN, MOVZ, MOVK: MOVK keeps the other halfwords.
        if (w >> 29) & 3 == 3 {
            fx.reads = reg(rd, false);
        }
        fx.writes = reg(rd, false);
        return insn(flow, fx);
    }
    if w & 0x1f80_0000 == 0x1300_0000 {
        // SBFM, BFM, UBFM: BFM keeps the destination's other bits.
        fx.reads = reg(rn, false);
        if (w >> 29) & 3 == 1 {
            fx.reads |= reg(rd, false);
        }
        fx.writes = reg(rd, false);
        return insn(flow, fx);
    }
    if w & 0x1f80_0000 == 0x1380_0000 {
        fx.reads = reg(rn, false) | reg(rm, false);
        fx.writes = reg(rd, false);
        return insn(flow, fx);
    }

    // Data processing, register.
    if w & 0x0e00_0000 == 0x0a00_0000 {
        let s = w & (1 << 29) != 0;
        if w & 0x1f00_0000 == 0x0a00_0000 {
            let flags = (w >> 29) & 3 == 3;
            fx.reads = reg(rn, false) | reg(rm, false);
            fx.writes = reg(rd, false);
            fx.flags_write = flags;
        } else if w & 0x1f20_0000 == 0x0b00_0000 {
            fx.reads = reg(rn, false) | reg(rm, false);
            fx.writes = reg(rd, false);
            fx.flags_write = s;
        } else if w & 0x1f20_0000 == 0x0b20_0000 {
            fx.reads = reg(rn, true) | reg(rm, false);
            fx.writes = reg(rd, !s);
            fx.flags_write = s;
        } else if w & 0x1fe0_0000 == 0x1a00_0000 {
            // ADC, SBC (and RMIF, SETF, which set flags only).
            fx.reads = reg(rn, false) | reg(rm, false);
            fx.writes = reg(rd, false);
            fx.flags_read = true;
            fx.flags_write = s;
        } else if w & 0x1fe0_0000 == 0x1a40_0000 {
            // CCMN, CCMP.
            fx.reads = reg(rn, false)
                | if w & (1 << 11) == 0 {
                    reg(rm, false)
                } else {
                    0
                };
            fx.flags_read = true;
            fx.flags_write = true;
        } else if w & 0x1fe0_0000 == 0x1a80_0000 {
            // CSEL, CSINC, CSINV, CSNEG.
            fx.reads = reg(rn, false) | reg(rm, false);
            fx.writes = reg(rd, false);
            fx.flags_read = true;
        } else if w & 0x1f00_0000 == 0x1b00_0000 {
            let ra = (w >> 10) & 31;
            fx.reads = reg(rn, false) | reg(rm, false) | reg(ra, false);
            fx.writes = reg(rd, false);
        } else if w & 0x5fe0_0000 == 0x1ac0_0000 {
            fx.reads = reg(rn, true) | reg(rm, true);
            fx.writes = reg(rd, true);
        } else if w & 0x5fe0_0000 == 0x5ac0_0000 {
            let opcode2 = (w >> 16) & 31;
            if opcode2 == 1 {
                // PACIA, AUTDB, XPACI and the other PAC forms: Xd in and
                // out, Xn (or SP) the modifier.
                fx.pac = true;
                fx.reads = reg(rd, false) | reg(rn, true);
                fx.writes = reg(rd, false);
            } else {
                fx.reads = reg(rn, false);
                fx.writes = reg(rd, false);
            }
        } else {
            fx.known = false;
            fx.reads = ALL;
        }
        return insn(flow, fx);
    }

    // Loads and stores.
    if w & 0x0a00_0000 == 0x0800_0000 {
        fx.memory = true;
        let vector = w & (1 << 26) != 0;
        let base = reg(rn, true);
        let rt = rd;
        let rt2 = (w >> 10) & 31;
        // A load or store of Xt by size and opc, for the register forms.
        let load_store = |fx: &mut Effects| {
            let size = w >> 30;
            let opc = (w >> 22) & 3;
            if vector {
                return;
            }
            if opc == 0 {
                fx.reads |= reg(rt, false);
            } else if !(size == 3 && opc == 2) {
                // Not PRFM.
                fx.writes |= reg(rt, false);
            }
        };
        if w & 0x3b00_0000 == 0x3900_0000 {
            fx.reads = base;
            load_store(&mut fx);
        } else if w & 0x3b00_0000 == 0x1800_0000 {
            if !vector && w >> 30 != 3 {
                fx.writes = reg(rt, false);
            }
        } else if w & 0x3a00_0000 == 0x2800_0000 {
            let mode = (w >> 23) & 3;
            let load = w & (1 << 22) != 0;
            fx.reads = base;
            if mode == 1 || mode == 3 {
                fx.writes = base;
            }
            if !vector {
                if load {
                    fx.writes |= reg(rt, false) | reg(rt2, false);
                } else {
                    fx.reads |= reg(rt, false) | reg(rt2, false);
                }
            }
        } else if w & 0x3b20_0000 == 0x3800_0000 {
            fx.reads = base;
            let index = (w >> 10) & 3;
            if index == 1 || index == 3 {
                fx.writes = base;
            }
            load_store(&mut fx);
        } else if w & 0x3b20_0c00 == 0x3820_0800 {
            fx.reads = base | reg(rm, false);
            load_store(&mut fx);
        } else if !vector && w & 0x3b20_0c00 == 0x3820_0000 {
            // LDADD, SWP and the other atomics: Xs in, Xt out.
            fx.reads = base | reg(rm, false);
            fx.writes = reg(rt, false);
        } else if !vector && w & 0xff20_0400 == 0xf820_0400 {
            // LDRAA, LDRAB.
            fx.pac = true;
            fx.reads = base;
            fx.writes = reg(rt, false);
            if w & (1 << 11) != 0 {
                fx.writes |= base;
            }
        } else if w & 0x3f00_0000 == 0x0800_0000 {
            let o2 = w & (1 << 23) != 0;
            let load = w & (1 << 22) != 0;
            let o1 = w & (1 << 21) != 0;
            let pair = |r: u32| reg(r, false) | if r < 30 { reg(r + 1, false) } else { 0 };
            fx.reads = base;
            if o2 && o1 {
                // CAS, CASP: Xs compared and loaded, Xt stored.
                let size = w >> 30;
                let paired = size < 2;
                let s = if paired { pair(rm) } else { reg(rm, false) };
                let t = if paired { pair(rt) } else { reg(rt, false) };
                fx.reads |= s | t;
                fx.writes = s;
            } else if !o2 {
                // Exclusives; o1 names a pair.
                let t = reg(rt, false) | if o1 { reg(rt2, false) } else { 0 };
                if load {
                    fx.writes = t;
                } else {
                    fx.reads |= t;
                    fx.writes = reg(rm, false);
                }
            } else if load {
                fx.writes = reg(rt, false);
            } else {
                fx.reads |= reg(rt, false);
            }
        } else if w & 0x3f20_0c00 == 0x1900_0000 {
            // LDAPUR, STLUR.
            fx.reads = base;
            let opc = (w >> 22) & 3;
            if opc == 0 {
                fx.reads |= reg(rt, false);
            } else {
                fx.writes = reg(rt, false);
            }
        } else if w & 0xbe00_0000 == 0x0c00_0000 {
            // Advanced SIMD structures, with a register or immediate
            // post-index.
            fx.simd = true;
            fx.reads = base;
            if w & (1 << 23) != 0 {
                fx.writes = base;
                fx.reads |= reg(rm, false);
            }
        } else {
            fx.known = false;
            fx.reads = ALL;
        }
        return insn(flow, fx);
    }

    // Advanced SIMD and floating point.
    if w & 0x0e00_0000 == 0x0e00_0000 {
        fx.simd = true;
        if w & 0x5f20_fc00 == 0x1e20_0000 {
            // Conversions between floating point and integer.
            let opcode = (w >> 16) & 7;
            if matches!(opcode, 2 | 3 | 7) {
                fx.reads = reg(rn, false);
            } else {
                fx.writes = reg(rd, false);
            }
        } else if w & 0x9fe0_8400 == 0x0e00_0400 {
            match (w >> 11) & 15 {
                // DUP and INS (general).
                1 | 3 => fx.reads = reg(rn, false),
                // SMOV, UMOV.
                5 | 7 => fx.writes = reg(rd, false),
                _ => {}
            }
        } else if w & 0x5f20_3c00 == 0x1e20_2000 {
            // FCMP, FCMPE.
            fx.flags_write = true;
        } else if w & 0x5f20_0c00 == 0x1e20_0400 {
            // FCCMP, FCCMPE.
            fx.flags_read = true;
            fx.flags_write = true;
        } else if w & 0x5f20_0c00 == 0x1e20_0c00 {
            // FCSEL.
            fx.flags_read = true;
        }
        return insn(flow, fx);
    }
    fx.known = false;
    fx.reads = ALL;
    insn(Flow::Next, fx)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn x(registers: &[u32]) -> u32 {
        registers.iter().map(|&n| 1 << n).fold(0, |a, b| a | b)
    }

    #[test]
    fn branches_name_their_targets() {
        // b #-8 at 0x1000; bl #+0x40; b.ne #+8; cbz x3, #+16; tbnz w5, #3, #-4.
        assert_eq!(classify(0x1000, 0x17ff_fffe).flow, Flow::Branch(0xff8));
        let bl = classify(0x1000, 0x9400_0010);
        assert_eq!(bl.flow, Flow::Call(0x1040));
        assert_eq!(bl.effects.writes, x(&[30]));
        let bne = classify(0x1000, 0x5400_0041);
        assert_eq!(bne.flow, Flow::Conditional(0x1008));
        assert!(bne.effects.flags_read);
        let cbz = classify(0x1000, 0xb400_0083);
        assert_eq!(cbz.flow, Flow::Conditional(0x1010));
        assert_eq!(cbz.effects.reads, x(&[3]));
        let tbnz = classify(0x1000, 0x371f_ffe5);
        assert_eq!(tbnz.flow, Flow::Conditional(0xffc));
        assert_eq!(tbnz.effects.reads, x(&[5]));
    }

    #[test]
    fn register_transfers_and_returns() {
        assert_eq!(classify(0, 0xd65f_03c0).flow, Flow::Return); // ret
        let retab = classify(0, 0xd65f_0fff);
        assert_eq!(retab.flow, Flow::Return);
        assert!(retab.effects.pac);
        assert_eq!(retab.effects.reads, x(&[30]) | SP);
        let br = classify(0, 0xd61f_0200); // br x16
        assert_eq!(br.flow, Flow::Indirect);
        assert_eq!(br.effects.reads, x(&[16]));
        let blr = classify(0, 0xd63f_0100); // blr x8
        assert_eq!(blr.flow, Flow::IndirectCall);
        assert_eq!(blr.effects.writes, x(&[30]));
        let blraa = classify(0, 0xd73f_0911); // blraa x8, x17
        assert_eq!(blraa.flow, Flow::IndirectCall);
        assert_eq!(blraa.effects.reads, x(&[8, 17]));
        assert_eq!(classify(0, 0xd420_0000).flow, Flow::Trap); // brk #0
        let svc = classify(0, 0xd400_1001); // svc #0x80
        assert_eq!(svc.flow, Flow::Next);
        assert!(!svc.effects.known);
    }

    #[test]
    fn arithmetic_and_moves() {
        // add x0, x1, #16
        let add = classify(0, 0x9100_4020).effects;
        assert_eq!((add.reads, add.writes, add.flags_write), (x(&[1]), x(&[0]), false));
        // sub sp, sp, #0x40 reads and writes SP.
        let sub = classify(0, 0xd101_03ff).effects;
        assert_eq!((sub.reads, sub.writes), (SP, SP));
        // cmp x2, #5 (subs xzr, x2, #5) writes flags only.
        let cmp = classify(0, 0xf100_145f).effects;
        assert_eq!((cmp.reads, cmp.writes, cmp.flags_write), (x(&[2]), 0, true));
        // mov x3, x4 (orr x3, xzr, x4)
        let mov = classify(0, 0xaa04_03e3).effects;
        assert_eq!((mov.reads, mov.writes), (x(&[4]), x(&[3])));
        // movk x5, #1, lsl #16 reads x5.
        let movk = classify(0, 0xf2a0_0025).effects;
        assert_eq!((movk.reads, movk.writes), (x(&[5]), x(&[5])));
        // csel x0, x1, x2, eq reads flags.
        let csel = classify(0, 0x9a82_0020).effects;
        assert_eq!((csel.reads, csel.writes, csel.flags_read), (x(&[1, 2]), x(&[0]), true));
        // madd x0, x1, x2, x3
        let madd = classify(0, 0x9b02_0c20).effects;
        assert_eq!(madd.reads, x(&[1, 2, 3]));
        // adrp x8, #0
        assert_eq!(classify(0, 0x9000_0008).effects.writes, x(&[8]));
    }

    #[test]
    fn memory_forms() {
        // ldr x0, [x1, #8]
        let ldr = classify(0, 0xf940_0420).effects;
        assert_eq!((ldr.reads, ldr.writes, ldr.memory), (x(&[1]), x(&[0]), true));
        // str w2, [sp, #12]
        let str = classify(0, 0xb900_0fe2).effects;
        assert_eq!((str.reads, str.writes), (x(&[2]) | SP, 0));
        // stp x29, x30, [sp, #-16]! writes SP back.
        let stp = classify(0, 0xa9bf_7bfd).effects;
        assert_eq!((stp.reads, stp.writes), (x(&[29, 30]) | SP, SP));
        // ldp x29, x30, [sp], #16
        let ldp = classify(0, 0xa8c1_7bfd).effects;
        assert_eq!((ldp.reads, ldp.writes), (SP, x(&[29, 30]) | SP));
        // ldr x0, [x1, x2, lsl #3]
        let indexed = classify(0, 0xf862_7820).effects;
        assert_eq!((indexed.reads, indexed.writes), (x(&[1, 2]), x(&[0])));
        // ldr q0, [x1] reads only the base.
        let vector = classify(0, 0x3dc0_0020).effects;
        assert_eq!((vector.reads, vector.writes), (x(&[1]), 0));
        // ldxr x0, [x1]; stxr w2, x3, [x1]
        let ldxr = classify(0, 0xc85f_7c20).effects;
        assert_eq!((ldxr.reads, ldxr.writes), (x(&[1]), x(&[0])));
        let stxr = classify(0, 0xc802_7c23).effects;
        assert_eq!((stxr.reads, stxr.writes), (x(&[1, 3]), x(&[2])));
        // ldadd x0, x1, [x2]
        let ldadd = classify(0, 0xf820_0041).effects;
        assert_eq!((ldadd.reads, ldadd.writes), (x(&[0, 2]), x(&[1])));
        // ldr x0, #8 (literal)
        assert_eq!(classify(0, 0x5800_0040).effects.writes, x(&[0]));
    }

    #[test]
    fn system_and_pac_hints() {
        let pacibsp = classify(0, 0xd503_237f).effects;
        assert!(pacibsp.pac);
        assert_eq!((pacibsp.reads, pacibsp.writes), (x(&[30]) | SP, x(&[30])));
        let nop = classify(0, 0xd503_201f);
        assert_eq!((nop.flow, nop.effects.reads, nop.effects.writes), (Flow::Next, 0, 0));
        // mrs x0, tpidr_el1
        let mrs = classify(0, 0xd538_d080).effects;
        assert_eq!((mrs.writes, mrs.system), (x(&[0]), true));
        // msr nzcv, x1 writes the flags.
        let msr = classify(0, 0xd51b_4201).effects;
        assert!(msr.flags_write);
        // fmov x0, d1 writes x0; scvtf d0, x1 reads x1.
        assert_eq!(classify(0, 0x9e66_0020).effects.writes, x(&[0]));
        assert_eq!(classify(0, 0x9e62_0020).effects.reads, x(&[1]));
    }
}
