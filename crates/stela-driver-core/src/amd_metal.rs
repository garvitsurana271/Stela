//! Which AMD GPUs can serve Stela's Metal device.
//!
//! Stela drives an AMD GPU with amdgpu and draws on it with RADV. The Metal
//! device needs a set of Vulkan features from every device it opens
//! (stela-driver-gpu's `metal::vulkan::backend::requirements`), and RADV
//! offers some of them only from a graphics generation on. A GPU whose RADV
//! device can never meet them is left out: amdgpu's port does not claim it,
//! the boot reports it once as it reports any GPU it has no driver for, and
//! its firmware is neither pinned nor bundled.
//!
//! The generation is the major version of amdgpu's graphics block
//! (`gfx_v6_0.c` to `gfx_v12_0.c`), which Mesa calls `GFX6` to `GFX12`.
//! `cargo xtask gpu radv-test` opens RADV on every chip it drives and fails
//! when what a device lacks differs from [`lacking`], so the table follows
//! the pinned Mesa.

/// Each requirement of the Metal device that RADV does not meet on every
/// generation it drives, by its name in the requirement list, with the
/// first generation that meets it. RADV meets every other requirement from
/// GFX6 on.
pub const LATE_REQUIREMENTS: &[(&str, u8)] = &[
    // Half-float arithmetic (`radv_shader_fp16_enabled`): GFX8 runs it at
    // single rate and GFX9 on at double rate; GFX6 and GFX7 have none.
    ("shaderFloat16", 8),
];

/// The first generation whose RADV device meets every requirement.
pub const FIRST_GENERATION: u8 = {
    let mut first = 0;
    let mut index = 0;
    while index < LATE_REQUIREMENTS.len() {
        if LATE_REQUIREMENTS[index].1 > first {
            first = LATE_REQUIREMENTS[index].1;
        }
        index += 1;
    }
    first
};

/// The families of amdgpu's `enum amd_asic_type` (without `CHIP_`) that RADV
/// opens no device on at all: the compute accelerators without graphics,
/// which it refuses (`radv_is_gpu_supported`).
pub const NO_VULKAN_FAMILIES: &[&str] = &["ARCTURUS", "ALDEBARAN"];

/// The requirements RADV's device of graphics generation `generation`
/// lacks, in [`LATE_REQUIREMENTS`]' order.
pub fn lacking(generation: u8) -> impl Iterator<Item = &'static str> {
    LATE_REQUIREMENTS
        .iter()
        .filter(move |(_, first)| generation < *first)
        .map(|(name, _)| *name)
}

/// Whether a GPU of amdgpu's family `family` (without `CHIP_`), whose
/// graphics block is of `generation`, can serve the Metal device.
pub fn serves(family: &str, generation: u8) -> bool {
    generation >= FIRST_GENERATION && !NO_VULKAN_FAMILIES.contains(&family)
}

/// Why the port leaves out a GPU [`serves`] refuses, as the boot reports
/// it.
pub const LEFT_OUT: &str = "RADV, its Vulkan driver, cannot give Stela's Metal device what it \
                            requires on this GPU (half-float shader arithmetic, shaderFloat16, \
                            before the GFX8 graphics generation; no Vulkan device at all on a \
                            compute accelerator), and Stela drives no GPU that cannot serve it";

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    #[test]
    fn a_generation_serves_once_it_meets_every_late_requirement() {
        assert_eq!(FIRST_GENERATION, 8);
        assert_eq!(lacking(6).collect::<Vec<_>>(), ["shaderFloat16"]);
        assert_eq!(lacking(7).collect::<Vec<_>>(), ["shaderFloat16"]);
        for generation in 8..=12 {
            assert_eq!(lacking(generation).count(), 0, "{generation}");
            assert!(serves("POLARIS10", generation));
        }
        // Southern and Sea Islands: GFX6 and GFX7.
        assert!(!serves("TAHITI", 6));
        assert!(!serves("KAVERI", 7));
        // A compute accelerator of a generation that otherwise serves.
        assert!(!serves("ARCTURUS", 9));
        assert!(!serves("ALDEBARAN", 9));
        assert!(serves("VEGA20", 9));
        // The report names each late requirement.
        for (name, _) in LATE_REQUIREMENTS {
            assert!(LEFT_OUT.contains(name), "{name}");
        }
    }
}
