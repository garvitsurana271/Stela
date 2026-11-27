//! Framebuffer reads and the Apple families on drivers with and without
//! multisampled storage images: NVK and RADV report them, ANV on no Intel
//! GPU (`shaderStorageImageMultisample` in `anv_physical_device.c`).
use super::{ApiFeatures, Capabilities, Cause, ShaderFeatures};
use crate::metal::{
    abi::GpuFamily,
    nvidia::{CHIPS, Chip, fixture::nvk},
    open::{Attachment, PcieLink},
    service::wire,
    vulkan::{
        DriverId, PhysicalDeviceType, SampleCounts,
        physical::{Identity, PhysicalDevice},
    },
};
use alloc::{string::String, vec::Vec};

fn attachment() -> Attachment {
    Attachment {
        link: PcieLink {
            speed: 4,
            width: 16,
        },
        external: false,
        location_number: 0,
    }
}

fn gb203() -> &'static Chip {
    Chip::find(0x1b3).unwrap()
}

fn derive(
    device: &PhysicalDevice,
    chip: &Chip,
    shaders: ShaderFeatures,
    api: ApiFeatures,
) -> Capabilities {
    Capabilities::derive(device, &chip.platform(attachment()), shaders, api)
}

/// The features and limits ANV gives a Gen12.0 GPU (Tiger Lake to Raptor
/// Lake's Iris Xe) where they differ from NVK's, in
/// `anv_physical_device.c`, over the NVK fixture's simplified format
/// table: no multisampled storage images, mesh shaders (Gen12.5 on) or
/// barycentric coordinates (Xe2 on), 16384-texel images, single-sample
/// storage images and the stencil export NVK lacks.
fn anv_gen12_device() -> PhysicalDevice {
    let mut device = nvk(gb203(), 4 << 30);
    device.identity = Identity {
        driver_id: DriverId::IntelOpenSourceMesa,
        vendor_id: 0x8086,
        device_id: 0xa7a0,
        device_type: PhysicalDeviceType::IntegratedGpu,
        device_name: String::from("Intel(R) Iris(R) Xe Graphics (RPL-P)"),
        pci_bus: None,
        ..device.identity
    };
    let features = &mut device.features;
    features.shader_storage_image_multisample = false;
    features.mesh_shader = false;
    features.task_shader = false;
    features.fragment_shader_barycentric = false;
    features.cooperative_matrix = false;
    // ANV's own values of what the families need, as NVK's.
    assert!(features.fragment_shader_pixel_interlock && features.dynamic_rendering_local_read);
    assert!(features.maintenance8 && features.shader_storage_image_read_without_format);
    let limits = &mut device.limits;
    limits.max_image_dimension_1d = 1 << 14;
    limits.max_image_dimension_2d = 1 << 14;
    limits.max_image_dimension_cube = 1 << 14;
    limits.max_image_dimension_3d = 1 << 11;
    limits.max_framebuffer_width = 1 << 14;
    limits.max_framebuffer_height = 1 << 14;
    limits.storage_image_sample_counts = SampleCounts::C1;
    limits.mesh = None;
    limits.stencil_export = true;
    device
}

/// [`anv_gen12_device`]'s capabilities, which the pipeline and resource
/// tests take for an Intel GPU's.
pub(crate) fn anv_gen12() -> Capabilities {
    derive(
        &anv_gen12_device(),
        gb203(),
        ShaderFeatures::KNOWN,
        ApiFeatures::IMPLEMENTED,
    )
}

fn cause(capabilities: &Capabilities, name: &str) -> Option<Cause> {
    capabilities
        .gaps()
        .iter()
        .find(|gap| gap.capability == name)
        .map(|gap| gap.cause)
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

/// FNV-1a over `capabilities`' block, what the guest reads of the device:
/// every capability, family, format and gap.
fn digest(hash: u64, capabilities: &Capabilities) -> u64 {
    wire::bytes(&*capabilities.block().unwrap())
        .iter()
        .fold(hash, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
        })
}

/// Devices that report multisampled storage images, as NVK on every
/// NVIDIA GPU and RADV on every AMD one do, modeled over the NVK fixture,
/// with each block's digest from before framebuffer reads stopped
/// requiring them (335f4ea6d).
fn with_multisampled_storage() -> Vec<(&'static str, u64, u64)> {
    let complete = |device: &PhysicalDevice, chip: &Chip| {
        derive(
            device,
            chip,
            ShaderFeatures::KNOWN,
            ApiFeatures::IMPLEMENTED,
        )
    };
    let every_chip = CHIPS.iter().fold(FNV_OFFSET, |hash, chip| {
        digest(hash, &complete(&nvk(chip, 16 << 30), chip))
    });
    let chip = gb203();
    let every_api = derive(
        &nvk(chip, 16 << 30),
        chip,
        ShaderFeatures::KNOWN,
        ApiFeatures::KNOWN,
    );
    let no_raster_order_groups = derive(
        &nvk(chip, 16 << 30),
        chip,
        ShaderFeatures::KNOWN.difference(ShaderFeatures::RASTER_ORDER_GROUPS),
        ApiFeatures::IMPLEMENTED,
    );
    // RADV on GFX9 and GFX10.1: no mesh shaders or barycentric
    // coordinates; on GFX8 no fragment shader interlock either.
    let turing = Chip::find(0x172).unwrap();
    let mut gfx9 = nvk(turing, 16 << 30);
    gfx9.features.mesh_shader = false;
    gfx9.features.task_shader = false;
    gfx9.features.fragment_shader_barycentric = false;
    let mut gfx8 = gfx9.clone();
    gfx8.features.fragment_shader_pixel_interlock = false;
    let mut no_local_read = nvk(chip, 16 << 30);
    no_local_read.features.dynamic_rendering_local_read = false;
    let one = |capabilities: &Capabilities| digest(FNV_OFFSET, capabilities);
    alloc::vec![
        ("every NVIDIA chip", every_chip, 0x51c6_eb72_26e5_3488),
        (
            "GB203, every API path",
            one(&every_api),
            0x6b81_5f3f_512e_8dcd
        ),
        (
            "GB203, no raster order groups in the compiler",
            one(&no_raster_order_groups),
            0xd618_22ff_0bc1_a246,
        ),
        (
            "RADV GFX9 and GFX10.1 model",
            one(&complete(&gfx9, turing)),
            0xeb78_cc20_7f3f_ae91,
        ),
        (
            "RADV GFX8 model",
            one(&complete(&gfx8, turing)),
            0xf8c3_3d92_c92b_3cf0,
        ),
        (
            "GB203, no local reads",
            one(&complete(&no_local_read, chip)),
            0x7dbd_09c1_ca87_3298,
        ),
    ]
}

#[test]
fn devices_with_multisampled_storage_images_derive_what_they_did() {
    for (device, digest, before) in with_multisampled_storage() {
        assert_eq!(digest, before, "{device}");
    }
    let capabilities = crate::metal::nvidia::fixture::capabilities();
    assert!(capabilities.framebuffer_reads && !capabilities.single_sample_storage);
    assert!(capabilities.framebuffer_reads_at(4));
    assert_eq!(cause(&capabilities, "multisampled framebuffer reads"), None);
}

#[test]
fn anv_reads_single_sample_framebuffers_in_apple3() {
    let capabilities = anv_gen12();
    assert!(capabilities.framebuffer_reads);
    assert!(capabilities.single_sample_storage);
    assert!(capabilities.framebuffer_reads_at(1));
    assert!(!capabilities.framebuffer_reads_at(2));
    assert!(!capabilities.framebuffer_reads_at(4));
    assert_eq!(
        cause(&capabilities, "multisampled framebuffer reads"),
        Some(Cause::Driver)
    );
    assert_eq!(cause(&capabilities, "framebuffer reads"), None);
    assert_eq!(cause(&capabilities, "raster order groups"), None);
    for family in [
        GpuFamily::Common1,
        GpuFamily::Common2,
        GpuFamily::Common3,
        GpuFamily::Mac2,
        GpuFamily::Apple3,
    ] {
        assert!(capabilities.supports_family(family), "{family:?}");
    }
    // Apple4's tile functions of multisampled passes and Apple5's stencil
    // resolve after a depth resolve take multisampled storage images;
    // fragment functions test single-sample fragments themselves.
    assert!(!capabilities.supports_family(GpuFamily::Apple4));
    assert!(!capabilities.supports_family(GpuFamily::Apple5));
    assert!(!capabilities.stencil_follows_depth && capabilities.fragment_tests);
    assert!(!capabilities.supports_family(GpuFamily::Metal3));
    assert_eq!(capabilities.no_apple_family(), None);
    assert!(
        capabilities
            .family_shortfall()
            .unwrap()
            .starts_with("Intel(R) Iris(R) Xe Graphics (RPL-P): Metal device in Apple3 at most")
    );
    // The flag crosses the block.
    let block = capabilities.block().unwrap();
    assert_eq!(block.capabilities().unwrap(), capabilities);
}

/// What ANV derived before: framebuffer reads took multisampled storage
/// images, so the device was in no Apple family, which the boot now
/// refuses to draw macOS on.
#[test]
fn a_device_in_no_apple_family_says_why_it_is_not_used() {
    let mut device = anv_gen12_device();
    device.features.fragment_shader_pixel_interlock = false;
    let capabilities = derive(
        &device,
        gb203(),
        ShaderFeatures::KNOWN,
        ApiFeatures::IMPLEMENTED,
    );
    assert!(capabilities.supports_family(GpuFamily::Mac2));
    assert!(!capabilities.supports_family(GpuFamily::Apple3));
    assert!(
        capabilities
            .families()
            .iter()
            .all(|family| !alloc::format!("{family:?}").starts_with("Apple"))
    );
    assert_eq!(
        capabilities.no_apple_family().unwrap(),
        "Intel(R) Iris(R) Xe Graphics (RPL-P): Metal device in no Apple GPU family (no raster \
         order groups); macOS draws its window server's framebuffers with Metal only on an \
         Apple3 device, so this GPU's Metal device is not used and macOS draws without it"
    );

    let mut device = anv_gen12_device();
    device.features.dynamic_rendering_local_read = false;
    let capabilities = derive(
        &device,
        gb203(),
        ShaderFeatures::KNOWN,
        ApiFeatures::IMPLEMENTED,
    );
    assert!(
        capabilities
            .no_apple_family()
            .unwrap()
            .contains("(no framebuffer reads)")
    );

    // Below Mac2 the first family it lacks is named.
    let mut device = anv_gen12_device();
    device.limits.max_viewports = 1;
    let capabilities = derive(
        &device,
        gb203(),
        ShaderFeatures::KNOWN,
        ApiFeatures::IMPLEMENTED,
    );
    assert!(
        capabilities
            .no_apple_family()
            .unwrap()
            .contains("(no GPU family Mac2)")
    );

    // A compute-only compiler is in no family at all.
    let capabilities = derive(
        &nvk(gb203(), 16 << 30),
        gb203(),
        ShaderFeatures::COMPUTE,
        ApiFeatures::IMPLEMENTED,
    );
    assert!(
        capabilities
            .no_apple_family()
            .unwrap()
            .contains("(no GPU family Common1, ")
    );
}
