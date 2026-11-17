//! The graphics memory the firmware reserves for an integrated Intel GPU
//! ("stolen memory"), found as Linux's x86 early quirks find it before any
//! driver runs (`arch/x86/kernel/early-quirks.c`, `intel_graphics_stolen`):
//! the size from the GMCH control register and the base from the GPU's or the
//! host bridge's configuration, by the method that file's `intel_early_ids`
//! names for the device. i915 reads the result as
//! `intel_graphics_stolen_res`; Stela's arch runs no early quirks, so the
//! boot computes it here from the same registers. The build reads
//! `intel_early_ids` (`build/i915.rs`), so the device to method map is
//! Linux's own.

/// A PCI function the method reads: the GPU itself, or a function of the
/// host bridge's device (bus 0, device 0).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Function {
    Gpu,
    Bridge(u8),
}

/// A read of 32 bits of configuration at a dword-aligned offset.
pub type Read<'a> = &'a mut dyn FnMut(Function, u16) -> Option<u32>;

/// What a read of the registers found.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Stolen {
    pub base: u64,
    pub size: u64,
}

/// Why no stolen memory is reported.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Absent {
    /// Linux's early quirks name no method for the device: i915 finds a
    /// newer GPU's stolen memory from the GPU itself, and a discrete GPU's
    /// is its own memory.
    NoMethod,
    /// A register could not be read.
    Unreadable(Function, u16),
    /// The GMCH control register holds a value Linux does not know.
    UnknownControl(u16),
    /// The size or the base read as zero: none is reserved.
    None,
}

const KB: u64 = 1024;
const MB: u64 = 1024 * KB;

/// Configuration registers, `include/drm/intel/i915_drm.h`.
const SNB_GMCH_CTRL: u16 = 0x50;
const I830_GMCH_CTRL: u16 = 0x52;
const I830_DRB3: u16 = 0x63;
const I85X_DRB3: u16 = 0x43;
const I865_TOUD: u16 = 0xc4;
const I830_ESMRAMC: u16 = 0x91;
const I845_ESMRAMC: u16 = 0x9e;
const I85X_ESMRAMC: u16 = 0x61;
const INTEL_BSM: u16 = 0x5c;
const INTEL_GEN11_BSM_DW0: u16 = 0xc0;
const INTEL_GEN11_BSM_DW1: u16 = 0xc4;
const INTEL_BSM_MASK: u32 = 0xfff0_0000;
const TSEG_ENABLE: u8 = 1;

/// Reads of narrower width, from the dword that holds them.
struct Config<'a, 'b> {
    read: &'a mut Read<'b>,
}

impl Config<'_, '_> {
    fn dword(&mut self, function: Function, offset: u16) -> Result<u32, Absent> {
        (self.read)(function, offset & !3).ok_or(Absent::Unreadable(function, offset))
    }

    fn word(&mut self, function: Function, offset: u16) -> Result<u16, Absent> {
        Ok((self.dword(function, offset)? >> ((offset & 2) * 8)) as u16)
    }

    fn byte(&mut self, function: Function, offset: u16) -> Result<u8, Absent> {
        Ok((self.dword(function, offset)? >> ((offset & 3) * 8)) as u8)
    }
}

const HOST: Function = Function::Bridge(0);

fn i830_tseg_size(config: &mut Config) -> Result<u64, Absent> {
    let esmramc = config.byte(HOST, I830_ESMRAMC)?;
    Ok(match (esmramc & TSEG_ENABLE != 0, esmramc & 2 != 0) {
        (false, _) => 0,
        (true, true) => MB,
        (true, false) => 512 * KB,
    })
}

fn i845_tseg_size(config: &mut Config) -> Result<u64, Absent> {
    let esmramc = config.byte(HOST, I845_ESMRAMC)?;
    if esmramc & TSEG_ENABLE == 0 {
        return Ok(0);
    }
    // Linux warns of the two other values and counts no segment.
    Ok(match esmramc & 6 {
        4 => 512 * KB,
        6 => MB,
        _ => 0,
    })
}

fn i85x_tseg_size(config: &mut Config) -> Result<u64, Absent> {
    let esmramc = config.byte(HOST, I85X_ESMRAMC)?;
    Ok(if esmramc & TSEG_ENABLE != 0 { MB } else { 0 })
}

fn i830_stolen_size(config: &mut Config) -> Result<u64, Absent> {
    let control = config.word(HOST, I830_GMCH_CTRL)?;
    match (control >> 4) & 7 {
        2 => Ok(512 * KB),
        3 => Ok(MB),
        4 => Ok(8 * MB),
        // Local memory is not part of the normal address space.
        1 => Ok(0),
        _ => Err(Absent::UnknownControl(control)),
    }
}

fn gen3_stolen_size(config: &mut Config) -> Result<u64, Absent> {
    let control = config.word(HOST, I830_GMCH_CTRL)?;
    let megabytes = match (control >> 4) & 0xf {
        1 => 1,
        2 => 4,
        3 => 8,
        4 => 16,
        5 => 32,
        6 => 48,
        7 => 64,
        8 => 128,
        9 => 256,
        0xa => 96,
        0xb => 160,
        0xc => 224,
        0xd => 352,
        _ => return Err(Absent::UnknownControl(control)),
    };
    Ok(megabytes * MB)
}

fn snb_gms(config: &mut Config) -> Result<u64, Absent> {
    Ok(u64::from(
        (config.word(Function::Gpu, SNB_GMCH_CTRL)? >> 3) & 0x1f,
    ))
}

fn bdw_gms(config: &mut Config) -> Result<u64, Absent> {
    Ok(u64::from(
        (config.word(Function::Gpu, SNB_GMCH_CTRL)? >> 8) & 0xff,
    ))
}

fn gen3_stolen_base(config: &mut Config) -> Result<u64, Absent> {
    Ok(u64::from(
        config.dword(Function::Gpu, INTEL_BSM)? & INTEL_BSM_MASK,
    ))
}

/// The stolen memory of the device whose early-quirk method is `method`
/// (the `*_early_ops` name without the suffix), read through `read`.
pub fn find(method: &str, read: Read) -> Result<Stolen, Absent> {
    let mut read = read;
    let config = &mut Config { read: &mut read };
    let (size, base) = match method {
        "i830" | "i845" => {
            let size = i830_stolen_size(config)?;
            let memory = u64::from(config.byte(HOST, I830_DRB3)?) * 32 * MB;
            let tseg = if method == "i830" {
                i830_tseg_size(config)?
            } else {
                i845_tseg_size(config)?
            };
            (size, memory.wrapping_sub(tseg).wrapping_sub(size))
        }
        "i85x" => {
            let size = gen3_stolen_size(config)?;
            let memory = u64::from(config.byte(Function::Bridge(1), I85X_DRB3)?) * 32 * MB;
            let tseg = i85x_tseg_size(config)?;
            (size, memory.wrapping_sub(tseg).wrapping_sub(size))
        }
        "i865" => {
            let size = gen3_stolen_size(config)?;
            let toud = u64::from(config.word(HOST, I865_TOUD)?);
            (size, toud * 64 * KB + i845_tseg_size(config)?)
        }
        "gen3" => (gen3_stolen_size(config)?, gen3_stolen_base(config)?),
        "gen6" => (snb_gms(config)? * 32 * MB, gen3_stolen_base(config)?),
        "gen8" => (bdw_gms(config)? * 32 * MB, gen3_stolen_base(config)?),
        "chv" => {
            let gms = snb_gms(config)?;
            let size = match gms {
                0..0x11 => gms * 32 * MB,
                0x11..0x17 => (gms - 0x11) * 4 * MB + 8 * MB,
                _ => (gms - 0x17) * 4 * MB + 36 * MB,
            };
            (size, gen3_stolen_base(config)?)
        }
        "gen9" | "gen11" => {
            let gms = bdw_gms(config)?;
            let size = if gms < 0xf0 {
                gms * 32 * MB
            } else {
                (gms - 0xf0) * 4 * MB + 4 * MB
            };
            let base = if method == "gen9" {
                gen3_stolen_base(config)?
            } else {
                u64::from(config.dword(Function::Gpu, INTEL_GEN11_BSM_DW0)? & INTEL_BSM_MASK)
                    | u64::from(config.dword(Function::Gpu, INTEL_GEN11_BSM_DW1)?) << 32
            };
            (size, base)
        }
        _ => return Err(Absent::NoMethod),
    };
    if size == 0 || base == 0 {
        return Err(Absent::None);
    }
    Ok(Stolen { base, size })
}

/// The early-quirk method Linux names for `device`, from `intel_early_ids`.
pub fn method(device: u16) -> Option<&'static str> {
    super::STOLEN
        .binary_search_by_key(&device, |(id, _)| *id)
        .ok()
        .map(|index| super::STOLEN[index].1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{collections::BTreeMap, format};

    /// A configuration space of the GPU and the host bridge, by byte.
    fn space(bytes: &[(Function, u16, u8)]) -> impl FnMut(Function, u16) -> Option<u32> {
        let map = bytes
            .iter()
            .map(|(function, offset, value)| ((format!("{function:?}"), *offset), *value))
            .collect::<BTreeMap<_, _>>();
        move |function, offset| {
            assert_eq!(offset & 3, 0);
            let byte = |at: u16| {
                map.get(&(format!("{function:?}"), offset + at))
                    .copied()
                    .unwrap_or(0)
            };
            Some(u32::from_le_bytes([byte(0), byte(1), byte(2), byte(3)]))
        }
    }

    #[test]
    fn methods_are_linuxs_by_device() {
        assert!(
            super::super::STOLEN
                .windows(2)
                .all(|pair| pair[0].0 < pair[1].0)
        );
        for (device, expected) in [
            (0x3577, "i830"),
            (0x2562, "i845"),
            (0x3582, "i85x"),
            (0x2572, "i865"),
            (0x2582, "gen3"),
            (0x2a42, "gen3"),
            (0x0046, "gen3"),
            (0x0f31, "gen6"),
            (0x0126, "gen6"),
            (0x0166, "gen6"),
            (0x0416, "gen6"),
            (0x1616, "gen8"),
            (0x22b0, "chv"),
            (0x1916, "gen9"),
            (0x3185, "gen9"),
            (0x3e92, "gen9"),
            (0x9b41, "gen9"),
            (0x8a56, "gen11"),
            (0x4e71, "gen11"),
            (0x9a49, "gen11"),
            (0x46a6, "gen11"),
            (0xa7a0, "gen11"),
        ] {
            assert_eq!(method(device), Some(expected), "{device:04x}");
        }
        // i915 finds Meteor Lake's from the GPU; DG2's memory is its own.
        assert_eq!(method(0x7d55), None);
        assert_eq!(method(0x56a0), None);
    }

    #[test]
    fn skylake_takes_gms_in_32_mib_steps_and_the_bsm() {
        // GMS 2 (64 MiB) in bits 15:8, BSM 0x7b80_0001 masked to 1 MiB.
        let mut read = space(&[
            (Function::Gpu, 0x51, 2),
            (Function::Gpu, 0x5c, 0x01),
            (Function::Gpu, 0x5e, 0x80),
            (Function::Gpu, 0x5f, 0x7b),
        ]);
        assert_eq!(
            find("gen9", &mut read),
            Ok(Stolen {
                base: 0x7b80_0000,
                size: 64 * MB
            })
        );
        // GMS 0xf1: 4 MiB steps from 4 MiB.
        let mut small = space(&[(Function::Gpu, 0x51, 0xf1), (Function::Gpu, 0x5f, 0x7b)]);
        assert_eq!(find("gen9", &mut small).unwrap().size, 8 * MB);
    }

    #[test]
    fn gen11_reads_a_64_bit_base() {
        let mut read = space(&[
            (Function::Gpu, 0x51, 1),
            (Function::Gpu, 0xc2, 0x40),
            (Function::Gpu, 0xc3, 0x8f),
            (Function::Gpu, 0xc4, 0x01),
        ]);
        assert_eq!(
            find("gen11", &mut read),
            Ok(Stolen {
                base: 0x1_8f40_0000,
                size: 32 * MB
            })
        );
    }

    #[test]
    fn older_methods_read_the_host_bridge() {
        // gen3: GMS 7 of I830_GMCH_CTRL (0x52) is 64 MiB, the base from BSM.
        let mut read = space(&[
            (Function::Bridge(0), 0x52, 0x70),
            (Function::Gpu, 0x5f, 0x3f),
        ]);
        assert_eq!(
            find("gen3", &mut read),
            Ok(Stolen {
                base: 0x3f00_0000,
                size: 64 * MB
            })
        );
        // i830: 8 MiB below top of memory (DRB3 32 x 32 MiB) less a 1 MiB
        // TSEG.
        let mut read = space(&[
            (Function::Bridge(0), 0x52, 0x40),
            (Function::Bridge(0), 0x63, 32),
            (Function::Bridge(0), 0x91, 3),
        ]);
        assert_eq!(
            find("i830", &mut read),
            Ok(Stolen {
                base: 1024 * MB - MB - 8 * MB,
                size: 8 * MB
            })
        );
        // Sandy Bridge: GMS in bits 7:3 of the GPU's mirror.
        let mut read = space(&[(Function::Gpu, 0x50, 2 << 3), (Function::Gpu, 0x5f, 0x20)]);
        assert_eq!(find("gen6", &mut read).unwrap().size, 64 * MB);
        // Cherry Trail: 0x12 is 12 MiB.
        let mut read = space(&[
            (Function::Gpu, 0x50, 0x12 << 3),
            (Function::Gpu, 0x5f, 0x20),
        ]);
        assert_eq!(find("chv", &mut read).unwrap().size, 12 * MB);
    }

    #[test]
    fn nothing_reserved_or_unknown_is_said() {
        let mut read = space(&[]);
        assert_eq!(find("gen9", &mut read), Err(Absent::None));
        let mut read = space(&[(Function::Bridge(0), 0x52, 0xf0)]);
        assert_eq!(find("gen3", &mut read), Err(Absent::UnknownControl(0xf0)));
        let mut none = |_: Function, _: u16| None;
        assert_eq!(
            find("gen9", &mut none),
            Err(Absent::Unreadable(Function::Gpu, 0x50))
        );
        assert_eq!(find("gen12", &mut space(&[])), Err(Absent::NoMethod));
    }
}
