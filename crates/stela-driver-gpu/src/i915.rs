//! i915 as a stack's kernel driver: what the boot asks about an Intel GPU
//! before Linux sees it, answered from i915's own PCI ID table as the build
//! read it (`build/i915.rs`).
use crate::{Adapter, stack::Control};

include!(concat!(env!("OUT_DIR"), "/i915_devices.rs"));

pub mod stolen;

/// What i915's table says of a device.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Platform {
    /// Its name (`PLATFORM()` in `i915_pci.c`, without `INTEL_`).
    pub name: &'static str,
    /// Its graphics IP version: the `GEN()` of its descriptor, or the one a
    /// descriptor with a version register states for checking.
    pub version: u8,
    /// The DMA mask bits i915 sets (`dma_mask_size`).
    pub dma_bits: u8,
    /// Whether i915 binds it only by force probe (`require_force_probe`).
    pub forced: bool,
    /// Whether it is a discrete GPU (`is_dgfx`).
    pub discrete: bool,
}

/// What i915's table says of `device`, if it holds the device.
pub fn platform(device: u16) -> Option<Platform> {
    DEVICES
        .binary_search_by_key(&device, |entry| entry.0)
        .ok()
        .map(|index| {
            let (_, dma_bits, forced, version, name, discrete) = DEVICES[index];
            Platform {
                name,
                version,
                dma_bits,
                forced,
                discrete,
            }
        })
}

/// Whether i915 binds the adapter: its table holds the device without a
/// force-probe mark. Stela keeps Linux's default of no force probe
/// (`CONFIG_DRM_I915_FORCE_PROBE`), so `i915_pci_probe` refuses a marked
/// device exactly as it says here.
pub fn admits(adapter: &Adapter) -> Result<(), &'static str> {
    match platform(adapter.device) {
        Some(platform) if !platform.forced => Ok(()),
        Some(_) => Err(
            "i915's PCI ID table marks this Intel GPU as not properly supported (it binds only by force probe, which Linux does not set by default)",
        ),
        None => Err("i915's PCI ID table does not hold this Intel GPU"),
    }
}

/// The DMA mask i915 sets on the adapter. One outside its table is never
/// presented to it ([`admits`]); it gets the narrowest mask of any platform.
pub fn dma_bits(adapter: &Adapter) -> u8 {
    platform(adapter.device)
        .map(|platform| platform.dma_bits)
        .or_else(|| DEVICES.iter().map(|entry| entry.1).min())
        .unwrap_or(32)
}

/// Whether i915's table describes the adapter as built into the processor
/// (no `is_dgfx`). One outside the table is never presented to i915.
pub fn integrated(adapter: &Adapter) -> bool {
    platform(adapter.device).is_some_and(|platform| !platform.discrete)
}

/// The BAR of the register window i915 maps (`intel_mmio_bar`): BAR 1 on
/// graphics version 2, BAR 0 from version 3 on.
pub fn registers(adapter: &Adapter) -> usize {
    match platform(adapter.device) {
        Some(platform) if platform.version == 2 => 1,
        _ => 0,
    }
}

/// i915's device queries (`uapi/drm/i915_drm.h`).
pub mod query {
    use super::Control;
    use alloc::vec;
    use core::ffi::c_void;

    /// `DRM_IOWR(DRM_COMMAND_BASE + DRM_I915_GETPARAM, drm_i915_getparam_t)`.
    const GETPARAM: u32 = 0xc010_6446;
    const PARAM_CHIPSET_ID: i32 = 4;
    /// `DRM_IOWR(DRM_COMMAND_BASE + DRM_I915_QUERY, struct drm_i915_query)`.
    const QUERY: u32 = 0xc010_6479;
    const QUERY_MEMORY_REGIONS: u64 = 4;
    /// `struct drm_i915_memory_region_info` in 64-bit words, and the class
    /// of device memory.
    const REGION_WORDS: usize = 11;
    const CLASS_DEVICE: u16 = 1;

    /// `drm_i915_getparam_t`.
    #[repr(C)]
    struct GetParam {
        param: i32,
        value: *mut i32,
    }

    /// `struct drm_i915_query`.
    #[repr(C)]
    struct Query {
        num_items: u32,
        flags: u32,
        items_ptr: u64,
    }

    /// `struct drm_i915_query_item`.
    #[repr(C)]
    #[derive(Default)]
    struct Item {
        query_id: u64,
        length: i32,
        flags: u32,
        data_ptr: u64,
    }

    /// The bound GPU's PCI device ID (`I915_PARAM_CHIPSET_ID`).
    pub fn chip(control: Control) -> Option<u32> {
        let mut value = 0i32;
        let mut request = GetParam {
            param: PARAM_CHIPSET_ID,
            value: &raw mut value,
        };
        (control(GETPARAM, (&raw mut request).cast::<c_void>()) == 0 && value > 0)
            .then_some(value as u32 & 0xffff)
    }

    /// What one query item answers, in 64-bit words: asked for its length,
    /// then filled. None when the driver refuses either (an item's negative
    /// length is its error) or answers nothing.
    fn ask(control: Control, query_id: u64) -> Option<alloc::vec::Vec<u64>> {
        let mut item = Item {
            query_id,
            ..Item::default()
        };
        let mut query = Query {
            num_items: 1,
            flags: 0,
            items_ptr: &raw mut item as u64,
        };
        if control(QUERY, (&raw mut query).cast::<c_void>()) != 0 || item.length <= 0 {
            return None;
        }
        let mut words = vec![0u64; (item.length as usize).div_ceil(8)];
        item.data_ptr = words.as_mut_ptr() as u64;
        (control(QUERY, (&raw mut query).cast::<c_void>()) == 0 && item.length > 0).then_some(words)
    }

    /// The device memory i915 manages: its device-memory regions' total, or
    /// on a GPU without any the system memory region it allocates from.
    pub fn memory_bytes(control: Control) -> Option<u64> {
        // `num_regions` and three reserved words, then the regions.
        let words = ask(control, QUERY_MEMORY_REGIONS)?;
        let count = (*words.first()? & 0xffff_ffff) as usize;
        let regions = words.get(2..2 + count.checked_mul(REGION_WORDS)?)?;
        let (mut device, mut system) = (0u64, 0u64);
        for region in regions.as_chunks::<REGION_WORDS>().0 {
            // The class and instance pair, then `probed_size`.
            let total = region[1];
            if region[0] as u16 == CLASS_DEVICE {
                device = device.saturating_add(total);
            } else {
                system = system.max(total);
            }
        }
        let bytes = if device != 0 { device } else { system };
        (bytes != 0).then_some(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn adapter(device: u16) -> Adapter {
        Adapter {
            address: crate::bus::pci::Address {
                segment: 0,
                bus: 0,
                device: 2,
                function: 0,
            },
            vendor: crate::Vendor::Intel,
            device,
            class: [3, 0, 0],
        }
    }

    #[test]
    fn the_table_is_i915s_own_with_each_descriptors_fields() {
        assert!(DEVICES.windows(2).all(|pair| pair[0].0 < pair[1].0));
        // (device, platform, version, mask bits): i830 through Meteor Lake,
        // each mask its descriptor's, overrides winning as in C (Gemini Lake
        // takes its GEN9_LP mask, Alder Lake P its own 39 over GEN12's).
        for (device, name, version, bits) in [
            (0x3577, "I830", 2, 32),
            (0x2582, "I915G", 3, 32),
            (0x29c2, "G33", 3, 36),
            (0x2a42, "GM45", 4, 36),
            (0x0046, "IRONLAKE", 5, 36),
            (0x0126, "SANDYBRIDGE", 6, 40),
            (0x0166, "IVYBRIDGE", 7, 40),
            (0x0f31, "VALLEYVIEW", 7, 40),
            (0x0416, "HASWELL", 7, 40),
            (0x1616, "BROADWELL", 8, 39),
            (0x22b0, "CHERRYVIEW", 8, 39),
            (0x1916, "SKYLAKE", 9, 39),
            (0x5a85, "BROXTON", 9, 39),
            (0x3185, "GEMINILAKE", 9, 39),
            (0x5916, "KABYLAKE", 9, 39),
            (0x3e92, "COFFEELAKE", 9, 39),
            (0x3ea0, "COFFEELAKE", 9, 39),
            (0x9b41, "COMETLAKE", 9, 39),
            (0x8a56, "ICELAKE", 11, 39),
            (0x4571, "ELKHARTLAKE", 11, 39),
            (0x4e71, "JASPERLAKE", 11, 39),
            (0x9a49, "TIGERLAKE", 12, 39),
            (0x46a6, "ALDERLAKE_P", 12, 39),
            (0x4905, "DG1", 12, 39),
            (0x56a0, "DG2", 12, 46),
            (0x7d55, "METEORLAKE", 12, 46),
        ] {
            let platform = platform(device).unwrap_or_else(|| panic!("{device:04x}"));
            assert_eq!(
                (platform.name, platform.version, platform.dma_bits),
                (name, version, bits),
                "{device:04x}"
            );
        }
        // Lunar Lake and Battlemage are xe's alone.
        assert_eq!(platform(0x64a0), None);
        assert_eq!(platform(0xe20b), None);
    }

    #[test]
    fn force_probe_and_discrete_are_the_descriptors_marks() {
        // Only the Data Center GPU Flex (ATS-M) is force-probed in i915.
        let forced = DEVICES
            .iter()
            .filter(|entry| entry.2)
            .map(|entry| entry.0)
            .collect::<alloc::vec::Vec<_>>();
        assert!(forced.contains(&0x56c0) && forced.contains(&0x56c1));
        assert!(
            DEVICES
                .iter()
                .filter(|entry| entry.2)
                .all(|entry| entry.4 == "DG2")
        );
        assert!(admits(&adapter(0x56c0)).is_err());
        assert!(admits(&adapter(0x56a0)).is_ok());
        assert!(admits(&adapter(0x64a0)).is_err());
        assert!(admits(&adapter(0x0166)).is_ok());
        // DG1 and the Arc A-series are discrete; the rest is built in.
        for device in [0x4905, 0x56a0] {
            assert!(!integrated(&adapter(device)), "{device:04x}");
        }
        for device in [0x3577, 0x0166, 0x1916, 0x8a56, 0x9a49, 0x7d55] {
            assert!(integrated(&adapter(device)), "{device:04x}");
        }
        assert!(!integrated(&adapter(0x64a0)));
    }

    #[test]
    fn the_register_window_is_bar_1_only_on_version_2() {
        assert_eq!(registers(&adapter(0x3577)), 1);
        assert_eq!(registers(&adapter(0x2582)), 0);
        assert_eq!(registers(&adapter(0x1916)), 0);
        assert_eq!(dma_bits(&adapter(0x1916)), 39);
        assert_eq!(dma_bits(&adapter(0x64a0)), 32);
    }

    #[test]
    fn the_chip_is_the_chipset_id_parameter() {
        let mut driver = |request: u32, argument: *mut core::ffi::c_void| {
            assert_eq!(request, 0xc010_6446);
            // SAFETY: the query passes its `drm_i915_getparam_t`.
            let param = unsafe { &mut *argument.cast::<(i32, *mut i32)>() };
            if param.0 != 4 {
                return -22;
            }
            // SAFETY: the parameter's value points at an int.
            unsafe { *param.1 = 0x1916 };
            0
        };
        assert_eq!(query::chip(&mut driver), Some(0x1916));
        let mut refusing = |_: u32, _: *mut core::ffi::c_void| -22;
        assert_eq!(query::chip(&mut refusing), None);
    }

    /// A driver answering `DRM_IOCTL_I915_QUERY` for memory regions with
    /// `words`.
    fn regions(words: &[u64]) -> impl FnMut(u32, *mut core::ffi::c_void) -> i32 + '_ {
        move |request, argument| {
            assert_eq!(request, 0xc010_6479);
            // SAFETY: the query passes its `struct drm_i915_query`.
            let query = unsafe { &mut *argument.cast::<(u32, u32, u64)>() };
            assert_eq!(query.0, 1);
            // SAFETY: it points at one `struct drm_i915_query_item`.
            let item = unsafe { &mut *(query.2 as *mut (u64, i32, u32, u64)) };
            if item.0 != 4 {
                item.1 = -22;
                return 0;
            }
            if item.1 == 0 {
                item.1 = (words.len() * 8) as i32;
            } else {
                // SAFETY: the item's buffer holds its length.
                unsafe {
                    core::ptr::copy_nonoverlapping(words.as_ptr(), item.3 as *mut u64, words.len())
                };
            }
            0
        }
    }

    #[test]
    fn memory_is_device_memory_or_else_the_system_region() {
        let region = |class: u64, total: u64| {
            let mut words = [0u64; 11];
            words[0] = class;
            words[1] = total;
            words
        };
        let mut integrated = vec![1, 0];
        integrated.extend(region(0, 16 << 30));
        assert_eq!(
            query::memory_bytes(&mut regions(&integrated)),
            Some(16 << 30)
        );
        let mut discrete = vec![2, 0];
        discrete.extend(region(0, 32 << 30));
        discrete.extend(region(1, 8 << 30));
        assert_eq!(query::memory_bytes(&mut regions(&discrete)), Some(8 << 30));
        assert_eq!(query::memory_bytes(&mut regions(&[3, 0])), None);
    }
}
