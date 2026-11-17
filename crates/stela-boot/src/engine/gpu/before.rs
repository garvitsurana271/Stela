//! What each stack's kernel driver needs of the boot before Linux sees its
//! function: what identifies the device to its firmware files, the files
//! themselves, and what must have settled after the function's reset. The
//! claim's own steps (identity, the firmware's screen, BARs, decoding, the
//! reset and the private domain, MSI) are the same for every stack.
use super::{PAGE, output};
use alloc::{format, string::String, sync::Arc, vec::Vec};
use stela_drivers::{
    bus::{
        mmio::owned::{self, CacheAttribute, Mapping, Protection},
        pci::{Function, owned::OwnedConfig},
    },
    firmware::{self, Catalog, ErrorKind as FirmwareError, Image, Provider, Requirement},
    gpu::{self, stack::GpuStack},
    probe::{NativeServices, ServiceError},
};
use stela_linux::catalogue::{FirmwareTarget, PciFirmware};
use stela_loader::host_firmware::requirements::Inventory;

#[cfg(feature = "intel")]
mod intel;

/// What a stack's part found on the function before its reset.
pub(super) struct Identified {
    /// The files the driver may ask for, each under the name it asks by.
    pub(super) firmware: PciFirmware,
    /// Register 0 where it identifies the chip, so it reads the same after
    /// the reset.
    pub(super) identity: Option<u32>,
    /// The chip or platform as people know it.
    pub(super) chip: Option<&'static str>,
    /// Words of Linux's command line (`module.parameter=value`) the driver
    /// needs set for this device before it registers.
    pub(super) parameters: Option<String>,
    /// Other functions the driver's probe searches for and reads the
    /// identity of. Linux is shown them for searches only.
    pub(super) searched: Vec<Searched>,
    /// The graphics memory the firmware reserved for the GPU, as Linux's
    /// x86 early quirks find it, by base and size.
    pub(super) stolen: Option<(u64, u64)>,
}

/// A function shown to Linux for a probe's searches, with the
/// configuration the scan read for it, if the probe reads any.
#[derive(Clone)]
pub(super) struct Searched {
    pub(super) identity: stela_linux::function::Identity,
    pub(super) configuration: Option<[u8; 256]>,
}

/// The function as a stack's part may read it before its reset.
pub(super) struct Observed<'a> {
    pub(super) services: &'a dyn NativeServices,
    pub(super) adapter: &'a gpu::Adapter,
    /// The PCI-to-ISA bridges and host bridge functions the probe read,
    /// which the Intel parts show their drivers.
    #[cfg_attr(not(feature = "intel"), allow(dead_code))]
    pub(super) chipset: &'a [stela_drivers::probe::ChipsetFunction],
    /// Its configuration, for reads only; the Intel parts read it.
    #[cfg_attr(not(feature = "intel"), allow(dead_code))]
    pub(super) function: &'a mut Function<OwnedConfig>,
    /// The first page of the stack's register BAR, mapped read-only.
    pub(super) registers: &'a Mapping,
    /// The stack's register BAR, for the reads a stack's part maps itself.
    pub(super) register_bar: &'a owned::Resource,
}

/// One stack's part of the claim.
pub(super) struct Before {
    /// What the boot can check from the function's PCI identity alone, before
    /// the claim changes anything on the function or takes the firmware's
    /// screen: that the driver's files for the device are pinned and on the
    /// USB. A GPU refused here keeps the firmware's picture.
    pub(super) admits: fn(&gpu::Adapter, &Catalog) -> Result<(), ServiceError>,
    /// Reads what selects the driver's firmware, through the first page of
    /// the stack's register BAR mapped read-only before the reset, and
    /// selects it.
    pub(super) identify: fn(Observed<'_>, Arc<Catalog>) -> Result<Identified, ServiceError>,
    /// Waits, after the reset, for what the driver's probe expects to find
    /// done. The register BAR is the resource.
    pub(super) settle:
        fn(&dyn NativeServices, &owned::Resource, &Identified) -> Result<(), ServiceError>,
}

/// The part for `stack`'s kernel driver, if the boot has one.
pub(super) fn of(stack: &GpuStack) -> Option<&'static Before> {
    match stack.kernel.driver {
        "nouveau" => Some(&NOUVEAU),
        "amdgpu" => Some(&AMDGPU),
        #[cfg(feature = "intel")]
        "xe" => Some(&intel::XE),
        _ => None,
    }
}

/// nouveau: the chip is `PMC_BOOT_0`'s, its GSP-RM files are that chip's, and
/// a Turing-to-Ada GPU's own firmware must have booted again after the reset.
static NOUVEAU: Before = Before {
    // Its files are the chip's, which only its registers name.
    admits: |_, _| Ok(()),
    identify: |observed, catalog| {
        let registers = observed.registers;
        let endian = registers.read32(4).map_err(ServiceError::Mapping)?;
        if endian == u32::MAX || endian == 0x01000001 {
            return Err(ServiceError::Refused(
                "the GPU register byte order is unavailable",
            ));
        }
        let boot0 = registers.read32(0).map_err(ServiceError::Mapping)?;
        let chip = gpu::nouveau_chip::Chip::from_boot0(boot0).ok_or(ServiceError::Refused(
            "the GPU's PMC chipset is unsupported",
        ))?;
        let name = chip.name.to_ascii_lowercase();
        let mut inventory = Inventory::default();
        stela_loader::host_firmware::recipes::require_gsp(&mut inventory, &name)
            .map_err(|_| ServiceError::Refused("the GPU has no pinned GSP recipe"))?;
        Ok(Identified {
            firmware: select_firmware(&inventory, &name, catalog, None)?,
            identity: Some(boot0),
            chip: Some(chip.name),
            parameters: None,
            searched: Vec::new(),
            stolen: None,
        })
    },
    settle: |services, registers, identified| match identified
        .identity
        .and_then(gpu::nouveau_chip::Chip::from_boot0)
        .filter(|chip| chip.waits_for_firmware_boot())
    {
        Some(chip) => wait_for_firmware_boot(services, registers, chip),
        None => Ok(()),
    },
};

/// amdgpu: the files are those it may ask for on the PCI device ID's GPU,
/// as pinned: its family's for a device its table names; for one it
/// identifies by the device's own IP discovery table, the files of the
/// block versions that table names, read here before the reset as amdgpu
/// reads it ([`ip_discovery`]), or every file named after an IP version
/// where it cannot be read. The driver loads every engine's firmware
/// itself, so nothing boots by itself after the reset and nothing waits
/// here.
static AMDGPU: Before = Before {
    admits: |adapter, catalog| {
        use stela_loader::host_firmware::recipes::AmdGpu;
        let files = amdgpu_files(adapter.device)?;
        // A discovered GPU's files are known only from its table, which
        // `identify` reads; it checks them there.
        if AmdGpu::of(adapter.device) == AmdGpu::Discovered {
            return Ok(());
        }
        on_usb(&files, &format!("{:04x}", adapter.device), catalog)
    },
    identify: |observed, catalog| {
        use stela_loader::host_firmware::recipes;
        let adapter = observed.adapter;
        // Register 0 identifies nothing here; the read proves the window.
        observed
            .registers
            .read32(0)
            .map_err(ServiceError::Mapping)?;
        let name = format!("{:04x}", adapter.device);
        let inventory = if recipes::AmdGpu::of(adapter.device) == recipes::AmdGpu::Discovered {
            discovered_files(&observed)?
        } else {
            amdgpu_files(adapter.device)?
        };
        Ok(Identified {
            firmware: select_firmware(&inventory, &name, catalog, None)?,
            identity: None,
            chip: recipes::amd_gpu_family(adapter.device),
            parameters: None,
            searched: Vec::new(),
            stolen: None,
        })
    },
    settle: |_, _, _| Ok(()),
};

/// The files amdgpu may ask for on the AMD GPU with PCI device ID `device`.
fn amdgpu_files(device: u16) -> Result<Inventory, ServiceError> {
    let mut inventory = Inventory::default();
    stela_loader::host_firmware::recipes::require_amdgpu(&mut inventory, device).map_err(|_| {
        ServiceError::Refused("amdgpu leaves this GPU to an older driver and loads no files for it")
    })?;
    Ok(inventory)
}

/// The files amdgpu asks for on the AMD GPU it identifies by its IP
/// discovery table: those of the block versions the table names, else
/// every file named after an IP version, with a line saying which.
fn discovered_files(observed: &Observed<'_>) -> Result<Inventory, ServiceError> {
    use stela_boot_core::amd_discovery::{self as discovery, Ip};
    use stela_loader::host_firmware::recipes::{self, DiscoveredIp};
    let address = observed.adapter.address;
    let device = observed.adapter.device;
    let ips = match ip_discovery(observed.services, observed.register_bar) {
        Ok(ips) => ips,
        Err(why) => {
            output::arguments(format_args!(
                "GPU {address}: its IP discovery table was not read before its reset ({why}); \
                 its driver is given every firmware file named after an IP version\n"
            ));
            return amdgpu_files(device);
        }
    };
    let named: Vec<String> = [
        ("GC", discovery::GC),
        ("MP0", discovery::MP0),
        ("MP1", discovery::MP1),
        ("SDMA", discovery::SDMA0),
        ("VCN", discovery::VCN),
        ("VPE", discovery::VPE),
        ("DCN", discovery::DMU),
    ]
    .iter()
    .filter_map(|&(block, hw_id)| {
        discovery::version(&ips, hw_id)
            .map(|(major, minor, revision)| format!("{block} {major}.{minor}.{revision}"))
    })
    .collect();
    output::arguments(format_args!(
        "GPU {address}: its IP discovery table names {}; its driver is given those blocks' \
         firmware\n",
        named.join(", ")
    ));
    let ips: Vec<DiscoveredIp> = ips
        .iter()
        .map(
            |&Ip {
                 hw_id,
                 major,
                 minor,
                 revision,
                 ..
             }| DiscoveredIp {
                hw_id,
                major,
                minor,
                revision,
            },
        )
        .collect();
    let mut inventory = Inventory::default();
    recipes::require_amdgpu_discovered(&mut inventory, device, &ips)
        .map_err(|_| ServiceError::Refused("the GPU's discovered firmware cannot be selected"))?;
    Ok(inventory)
}

/// The IP blocks of the AMD GPU whose register BAR is `registers`, from
/// its IP discovery table, read before the reset as amdgpu reads it at its
/// probe ([`stela_boot_core::amd_discovery`]): through the `MM_INDEX` and
/// `MM_DATA` registers, one dword at a time, whose index is put back after.
fn ip_discovery(
    services: &dyn NativeServices,
    registers: &owned::Resource,
) -> Result<Vec<stela_boot_core::amd_discovery::Ip>, String> {
    use stela_boot_core::amd_discovery::{self as discovery, MM_DATA, MM_INDEX, MM_INDEX_HI};
    if (registers.physical_span().bytes() as usize) < discovery::REGISTER_BYTES {
        return Err(String::from(
            "its register BAR is smaller than amdgpu's discovery reads",
        ));
    }
    let read = |offset| read_register(services, registers, offset).unwrap_or(u32::MAX);
    let mut window = services
        .authority()
        .map_err(|error| format!("{error:?}"))?
        .map(
            registers.clone(),
            0,
            PAGE as usize,
            CacheAttribute::Uncached,
            Protection::ReadWrite,
        )
        .map_err(|error| format!("its index registers did not map ({error:?})"))?;
    let index = (window.read32(MM_INDEX), window.read32(MM_INDEX_HI));
    let vram = |position: u64| -> u32 {
        let read = window
            .write32(MM_INDEX, position as u32 | 0x8000_0000)
            .and_then(|()| window.write32(MM_INDEX_HI, (position >> 31) as u32))
            .and_then(|()| window.read32(MM_DATA));
        read.unwrap_or(u32::MAX)
    };
    let result = discovery::place(read, &vram)
        .map_err(|error| format!("{error:?}"))
        .and_then(|place| {
            let mut binary = Vec::new();
            binary
                .try_reserve_exact(place.bytes)
                .map_err(|_| String::from("no memory for the table"))?;
            for at in (0..place.bytes as u64).step_by(4) {
                binary.extend_from_slice(&vram(place.offset + at).to_le_bytes());
            }
            discovery::parse(&binary).map_err(|error| format!("{error:?}"))
        });
    if let (Ok(low), Ok(high)) = index {
        let _ = window.write32(MM_INDEX, low);
        let _ = window.write32(MM_INDEX_HI, high);
    }
    // SAFETY: the accesses above were this scoped mapping's only use.
    unsafe { window.retire() }.map_err(|error| format!("{error:?}"))?;
    result
}

/// The image `selection` names in `stored`: its exact binding, else the
/// bundle's file by its pinned digest.
fn find<'a>(
    stored: &'a Catalog,
    selection: &stela_loader::host_firmware::recipes::Selection,
) -> Result<Image<'a>, firmware::Error> {
    use stela_loader::host_firmware::bundle;
    let request = &selection.request;
    match stored.load(request.view()) {
        Err(error) if error.kind == FirmwareError::Missing => {
            let name = bundle::name(selection.recipe.image.sha256);
            match stored.load(Requirement {
                name: &name,
                target: bundle::TARGET,
                minimum_bytes: request.minimum_bytes,
            }) {
                Err(stored_error) if stored_error.kind == FirmwareError::Missing => Err(error),
                other => other,
            }
        }
        other => other,
    }
}

/// Whether every file of `inventory` the driver needs is in `catalog`,
/// before anything of the GPU `name` changes: one line naming the files the
/// USB lacks, and the refusal [`select_firmware`] would give for them later.
/// A file the driver starts without (`recipes::optional`) may be missing.
fn on_usb(inventory: &Inventory, name: &str, catalog: &Catalog) -> Result<(), ServiceError> {
    use stela_loader::host_firmware::recipes;
    let plan = recipes::Plan::select(inventory)
        .map_err(|_| ServiceError::Refused("the GPU firmware recipe cannot be selected"))?;
    let missing: Vec<_> = plan
        .selections()
        .iter()
        .filter(|selection| {
            !recipes::optional(&selection.request)
                && find(catalog, selection).is_err_and(|error| error.kind == FirmwareError::Missing)
        })
        .map(|selection| selection.request.name.clone())
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    lacks(name, &missing, plan.selections().len());
    Err(ServiceError::Refused(NO_FIRMWARE))
}

/// Says which of the `files` files of the GPU `name` the USB lacks.
fn lacks(name: &str, missing: &[String], files: usize) {
    output::arguments(format_args!(
        "GPU {name}: the USB's firmware bundle lacks {} of this chip's {files} files ({}); \
         `stela repair` completes it\n",
        missing.len(),
        missing.join(", ")
    ));
}

/// The firmware of every request in `inventory`, each file bound under the
/// exact name and target its driver asks for. A driver needs every file of
/// its recipe but those it starts without (`recipes::optional`): one of the
/// others missing fails its start later with a generic error, so the GPU is
/// refused; an optional one missing is said once and its request answers
/// that the file does not exist, as on Linux.
/// `read` is an image the boot read from the PC itself and the name the
/// driver asks for it by, bound beside the files under the same target.
fn select_firmware(
    inventory: &Inventory,
    name: &str,
    catalog: Arc<Catalog>,
    read: Option<(Image<'static>, &str)>,
) -> Result<PciFirmware, ServiceError> {
    use stela_loader::host_firmware::recipes;
    let mut names = Vec::new();
    let request = inventory.requests().first().ok_or(ServiceError::Refused(
        "the GPU has no pinned firmware recipe",
    ))?;
    let target = FirmwareTarget::new(
        request.target.driver.clone(),
        request.target.hardware.clone(),
        request.target.revision.clone(),
        request.target.board.clone(),
    )
    .map_err(|_| ServiceError::Refused("the GPU firmware target is invalid"))?;
    let plan = recipes::Plan::select(inventory)
        .map_err(|_| ServiceError::Refused("the GPU firmware recipe cannot be selected"))?;
    // SAFETY: the reference count is never given back, so the catalog
    // stays allocated for every image borrowed from it for the boot.
    let stored: &'static Catalog = unsafe { &*Arc::into_raw(catalog.clone()) };
    let mut bindings = Vec::new();
    let mut missing = Vec::new();
    // A driver that chooses among many files by what the device reports
    // (amdgpu by IP discovery) gets one line for them, not one each.
    let summarized = plan.selections().len() > SUMMARIZED;
    for selection in plan.selections() {
        let request = &selection.request;
        let found = find(stored, selection);
        if let Ok(image) = found {
            if !summarized {
                output::arguments(format_args!(
                    "GPU {name}: firmware {} of {} bytes\n",
                    request.name,
                    image.bytes().len()
                ));
            }
            bindings.push((image, request));
        }
        match found {
            Ok(image) => {
                selection
                    .recipe
                    .image
                    .verify_digest(image.bytes().len(), image.sha256())
                    .map_err(|_| {
                        ServiceError::Refused(
                            "the GPU firmware digest does not match its pinned recipe",
                        )
                    })?;
            }
            Err(error) if error.kind == FirmwareError::Missing && recipes::optional(request) => {
                output::arguments(format_args!(
                    "GPU {name}: firmware {} is not on the USB; its driver starts without it\n",
                    request.name
                ));
            }
            Err(error) if error.kind == FirmwareError::Missing => {
                missing.push(request.name.clone());
            }
            Err(error) => {
                output::arguments(format_args!(
                    "GPU {name}: firmware {} refused: {error:?}\n",
                    request.name
                ));
                return Err(ServiceError::Refused(
                    "the GPU firmware does not match its exact catalogue target",
                ));
            }
        }
        names.push(request.name.clone());
    }
    if !missing.is_empty() {
        lacks(name, &missing, names.len());
        return Err(ServiceError::Refused(NO_FIRMWARE));
    }
    if summarized {
        output::arguments(format_args!(
            "GPU {name}: {} firmware files of {} bytes, of which its driver loads those the \
             device names\n",
            bindings.len(),
            bindings
                .iter()
                .map(|(image, _)| image.bytes().len())
                .sum::<usize>()
        ));
    }
    names.extend(read.iter().map(|(_, name)| String::from(*name)));
    // Each file under the exact name and target the driver asks for.
    let catalog = Catalog::rebind_all(
        bindings
            .iter()
            .map(|(image, request)| {
                let view = request.view();
                (*image, view.name, view.target)
            })
            .chain(
                read.iter()
                    .map(|(image, name)| (*image, *name, request.view().target)),
            ),
    )
    .map_err(|_| ServiceError::Refused("the GPU firmware cannot be bound to its requests"))?;
    PciFirmware::new(target, Arc::new(catalog), names.into_boxed_slice())
        .map_err(|_| ServiceError::Refused("the GPU firmware image names are invalid"))
}

fn read_register(
    services: &dyn NativeServices,
    registers: &owned::Resource,
    offset: usize,
) -> Option<u32> {
    let page = offset & !(PAGE as usize - 1);
    let mut window = services
        .authority()
        .ok()?
        .map(
            registers.clone(),
            page,
            PAGE as usize,
            CacheAttribute::Uncached,
            Protection::ReadOnly,
        )
        .ok()?;
    let value = window.read32(offset - page).ok();
    // SAFETY: the read above was this scoped mapping's only use.
    unsafe { window.retire() }.ok()?;
    value
}

/// After the claim's function-level reset a Turing-to-Ada GPU's own firmware
/// runs devinit again. nouveau reads the VBIOS before it waits for that
/// (`firmware_boot`), so wait here first, by nouveau's bound, and print what
/// the start of the old boot flow depends on: the firmware boot, WPR2 (which
/// FWSEC-FRTS sets) and the VBIOS window the PRAMIN shadow reads.
/// `GPU AD107: firmware boot completed after N polls: gfw=… plm=… wpr2=…-…
/// display_fuse=… vbios_window=…`, or `did not complete` and a refusal.
fn wait_for_firmware_boot(
    services: &dyn NativeServices,
    registers: &owned::Resource,
    chip: &gpu::nouveau_chip::Chip,
) -> Result<(), ServiceError> {
    use gpu::nouveau_chip::firmware_boot::{self, Error as BootError};
    let page = firmware_boot::PRIV_LEVEL_MASK & !(PAGE as usize - 1);
    let mut window = services
        .authority()?
        .map(
            registers.clone(),
            page,
            PAGE as usize,
            CacheAttribute::Uncached,
            Protection::ReadOnly,
        )
        .map_err(ServiceError::Mapping)?;
    let timer = services.minimum_delay()?;
    let result = firmware_boot::wait(
        |offset| {
            window
                .read32(offset - page)
                .map_err(|_| "a register read failed")
        },
        |micros| timer.wait_us(micros).map_err(|_| "the timer failed"),
    );
    // SAFETY: the polls above were this scoped mapping's only use.
    unsafe { window.retire() }.map_err(ServiceError::Mapping)?;
    let read = |offset| read_register(services, registers, offset);
    let hex =
        |value: Option<u32>| value.map_or_else(|| String::from("unread"), |v| format!("{v:#010x}"));
    // nouveau's PRAMIN shadow reads the window only with the display fused
    // on (`shadowramin.c` `pramin_init`: 0x820c04 from GA100, else 0x021c04).
    let fuse = read(match chip.architecture {
        gpu::nouveau_chip::Architecture::Turing => 0x02_1c04,
        _ => 0x82_0c04,
    });
    let window = fuse
        .filter(|fuse| fuse & 1 == 0)
        .and_then(|_| read(0x62_5f04));
    let state = format!(
        "wpr2={}-{} display_fuse={} vbios_window={}",
        hex(read(0x1f_a824)),
        hex(read(0x1f_a828)),
        hex(fuse),
        hex(window),
    );
    match result {
        Ok(reading) => {
            output::arguments(format_args!(
                "GPU {}: firmware boot completed after {} polls: gfw={:#010x} plm={:#010x} {state}\n",
                chip.name, reading.polls, reading.progress, reading.priv_level_mask
            ));
            Ok(())
        }
        Err(BootError::Timeout(reading)) => {
            output::arguments(format_args!(
                "GPU {}: firmware boot did not complete after its reset in {} polls of {} us: \
                 gfw={:#010x} (expected low byte {:#04x}) plm={:#010x} (expected bit 0) {state}\n",
                chip.name,
                reading.polls,
                firmware_boot::POLL_MICROS,
                reading.progress,
                firmware_boot::COMPLETED,
                reading.priv_level_mask
            ));
            Err(ServiceError::Refused(
                "the GPU's firmware boot did not complete after its reset",
            ))
        }
        Err(BootError::Access(error)) => {
            output::arguments(format_args!(
                "GPU {}: firmware boot wait stopped: {error}\n",
                chip.name
            ));
            Err(ServiceError::Refused(
                "the GPU's firmware boot registers could not be read",
            ))
        }
    }
}

/// More firmware files than this for one GPU are logged as one line.
const SUMMARIZED: usize = 16;

/// Why a GPU was refused when the USB lacks any of its chip's firmware files.
pub(super) const NO_FIRMWARE: &str = "no pinned firmware for the observed GPU chipset was loaded";
