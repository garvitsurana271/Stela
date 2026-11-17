//! The Intel stacks' parts of the claim: xe's, i915's and the display
//! code's needs of the boot, in an image built with the Intel port.
use super::{Before, Identified, Observed, Searched, on_usb, select_firmware};
use crate::output;
use alloc::{format, vec::Vec};
use stela_boot_core::opregion;
use stela_crypto::{Sha256, digest::Digest};
use stela_drivers::{
    bus::pci::ConfigSpace,
    firmware::{self, Catalog, Image, Provenance, Provider, Requirement, Resource},
    gpu,
    probe::ServiceError,
};
use stela_loader::host_firmware::requirements::Inventory;

/// xe: the platform is the PCI device ID's, as in xe's own table, and its
/// GuC, HuC, GSC and DMC files are that platform's. Nothing of the GPU's
/// boots by itself after a reset: xe loads all of it. The display code's
/// Video BIOS Table is the one file the PC itself holds.
pub(super) static XE: Before = Before {
    admits: |adapter, catalog| {
        on_usb(
            &xe_files(adapter.device)?,
            &format!("{:04x}", adapter.device),
            catalog,
        )
    },
    identify: |mut observed, catalog| {
        use stela_loader::host_firmware::recipes;
        let adapter = observed.adapter;
        // Register 0 identifies nothing here; the read proves the window.
        observed
            .registers
            .read32(0)
            .map_err(ServiceError::Mapping)?;
        if gpu::xe::force_probed(adapter.device) {
            output::arguments(format_args!(
                "GPU {}: xe binds {} only by Linux's force probe, which Stela sets for every \
                 device of xe's table: upstream Linux calls xe experimental on it\n",
                adapter.address,
                recipes::intel_gpu_platform(adapter.device).unwrap_or("this platform"),
            ));
        }
        let inventory = xe_files(adapter.device)?;
        let target = inventory
            .requests()
            .first()
            .ok_or(ServiceError::Refused(
                "the GPU has no pinned firmware recipe",
            ))?
            .view()
            .target;
        let table = video_bios_table(&mut observed, target);
        let name = format!("{:04x}", adapter.device);
        Ok(Identified {
            firmware: select_firmware(&inventory, &name, catalog, table.map(|table| (table, VBT)))?,
            identity: None,
            chip: recipes::intel_gpu_platform(adapter.device),
            // The display code asks the firmware service for the table this
            // parameter names before it looks anywhere else (`intel_bios.c`).
            parameters: table.map(|_| format!("xe.vbt_firmware={VBT}")),
            searched: chipset_bridges(&observed, "xe"),
            stolen: None,
        })
    },
    settle: |_, _, _| Ok(()),
};

/// i915: the platform is the PCI device ID's, as in i915's own table, and
/// its file is that platform's display microcontroller program, which i915
/// starts without. Its probe reads the host bridge (`i915_gmch.c`) and the
/// stolen memory Linux's x86 early quirks find; the display code takes the
/// PC's Video BIOS Table as xe's does.
pub(super) static I915: Before = Before {
    admits: |adapter, catalog| {
        on_usb(
            &i915_files(adapter.device)?,
            &format!("{:04x}", adapter.device),
            catalog,
        )
    },
    identify: |mut observed, catalog| {
        let adapter = observed.adapter;
        // Register 0 identifies nothing here; the read proves the window.
        observed
            .registers
            .read32(0)
            .map_err(ServiceError::Mapping)?;
        let platform = gpu::i915::handed(adapter.device)
            .ok_or(ServiceError::Refused("i915 is not handed the GPU"))?;
        let inventory = i915_files(adapter.device)?;
        let target = inventory
            .requests()
            .first()
            .ok_or(ServiceError::Refused(
                "the GPU has no pinned firmware recipe",
            ))?
            .view()
            .target;
        let table = video_bios_table(&mut observed, target);
        let name = format!("{:04x}", adapter.device);
        let mut searched = chipset_bridges(&observed, "i915");
        searched.extend(host_bridge(&observed));
        let stolen = graphics_stolen(&mut observed);
        Ok(Identified {
            firmware: select_firmware(&inventory, &name, catalog, table.map(|table| (table, VBT)))?,
            identity: None,
            chip: Some(platform.name),
            // As xe's (`intel_bios.c`).
            parameters: table.map(|_| format!("i915.vbt_firmware={VBT}")),
            searched,
            stolen,
        })
    },
    settle: |_, _, _| Ok(()),
};

/// The functions of the host bridge's device, with the configuration the
/// scan read, which i915 reads through `pci_get_domain_bus_and_slot(0, 0,
/// 0)` (`i915_gmch_bridge_setup` fails the probe without it).
fn host_bridge(observed: &Observed<'_>) -> Vec<Searched> {
    let address = observed.adapter.address;
    let functions: Vec<_> = observed
        .chipset
        .iter()
        .filter(|function| function.configuration.is_some())
        .map(|function| searched(function, function.configuration))
        .collect();
    if address.segment != 0 || functions.is_empty() {
        output::arguments(format_args!(
            "GPU {address}: no host bridge was read at 0000:00:00.0, so i915's probe fails at \
             its bridge setup\n"
        ));
    }
    functions
}

/// The graphics memory the firmware reserved for the GPU, by the method
/// Linux's early quirks name for the device, with one line saying what was
/// found or why none is reported.
fn graphics_stolen(observed: &mut Observed<'_>) -> Option<(u64, u64)> {
    use gpu::i915::stolen::{self, Absent};
    let address = observed.adapter.address;
    let config = observed.function.config();
    let mut read = |offset: u16| config.read32(offset).ok();
    let found = stolen::method(observed.adapter.device)
        .ok_or(Absent::NoMethod)
        .and_then(|method| stolen::find(method, &mut read));
    match found {
        Ok(found) => {
            output::arguments(format_args!(
                "GPU {address}: graphics stolen memory of {} KiB at {:#x}\n",
                found.size / 1024,
                found.base
            ));
            Some((found.base, found.size))
        }
        Err(why) => {
            output::arguments(format_args!(
                "GPU {address}: no graphics stolen memory is reported ({why:?}): i915 runs \
                 without it\n"
            ));
            None
        }
    }
}

/// The PCI-to-ISA bridges of the GPU's segment, with one line each. The
/// display code names the PCH by the bridge's IDs (`intel_pch.c` searches
/// the class) and takes its panel power sequencer and port map from that;
/// with none it detects no PCH.
fn chipset_bridges(observed: &Observed<'_>, driver: &str) -> Vec<Searched> {
    let address = observed.adapter.address;
    let bridges: Vec<_> = observed
        .chipset
        .iter()
        .filter(|bridge| {
            bridge.identity.address.segment == address.segment
                && bridge.identity.class[..2] == [0x06, 0x01]
        })
        .collect();
    if bridges.is_empty() {
        output::arguments(format_args!(
            "GPU {address}: no PCI-to-ISA bridge was found, so the display code detects no PCH\n"
        ));
    }
    bridges
        .into_iter()
        .map(|bridge| {
            output::arguments(format_args!(
                "GPU {address}: PCI-to-ISA bridge {} {:04x}:{:04x} is shown to {driver} for its \
                 chipset search\n",
                bridge.identity.address, bridge.identity.vendor, bridge.identity.device
            ));
            searched(bridge, None)
        })
        .collect()
}

/// How Linux is shown a chipset function the scan read.
fn searched(
    function: &stela_drivers::probe::ChipsetFunction,
    configuration: Option<[u8; 256]>,
) -> Searched {
    let (at, class) = (function.identity.address, function.identity.class);
    Searched {
        identity: stela_linux::function::Identity {
            bus: u32::from(at.bus),
            devfn: u32::from(at.device << 3 | at.function),
            vendor: function.identity.vendor,
            device: function.identity.device,
            subsystem_vendor: function.subsystem_vendor,
            subsystem_device: function.subsystem_device,
            class_revision: u32::from_be_bytes([class[0], class[1], class[2], function.revision]),
        },
        configuration,
    }
}

/// The name the display code asks for the PC's Video BIOS Table by.
const VBT: &str = "intel/vbt.bin";

/// The Video BIOS Table of the OpRegion the function's `ASLS` register
/// points at, as an image of `target`, and one line saying where it came
/// from or why there is none. Linux reads it through its ACPI OpRegion
/// driver, which Stela's Linux profile does not build; the OpRegion is plain
/// firmware memory, read here through its owner.
fn video_bios_table(
    observed: &mut Observed<'_>,
    target: firmware::Target<'_>,
) -> Option<Image<'static>> {
    let address = observed.adapter.address;
    let absent = |why: &dyn core::fmt::Display| {
        output::arguments(format_args!(
            "GPU {address}: no Video BIOS Table from the OpRegion ({why}): panels are found \
             by probing only\n"
        ));
    };
    let asls = match observed.function.config().read32(opregion::ASLS) {
        Ok(asls) => asls,
        Err(error) => {
            absent(&format_args!("its address could not be read: {error:?}"));
            return None;
        }
    };
    let table = match opregion::read(u64::from(asls), |physical, into| {
        observed.services.read_firmware_memory(physical, into)
    }) {
        Ok(table) => table,
        Err(why) => {
            absent(&why);
            return None;
        }
    };
    let (bytes, place, (major, minor)) = (table.bytes.len(), table.place, table.version);
    let revision = format!("{major}.{minor}");
    let catalog = Catalog::from_handoff(alloc::vec![Resource {
        name: VBT,
        target,
        provenance: Provenance {
            source: "ACPI OpRegion",
            revision: &revision,
            member: "vbt",
            license: "the PC's firmware",
        },
        expected_length: bytes,
        expected_sha256: Sha256::digest(&table.bytes).into(),
        bytes: table.bytes,
    }]);
    // The table is held for the boot, like the stored catalog's images.
    let image = catalog.ok().and_then(|catalog| {
        let held: &'static Catalog = alloc::boxed::Box::leak(alloc::boxed::Box::new(catalog));
        held.load(Requirement {
            name: VBT,
            target,
            minimum_bytes: 1,
        })
        .ok()
    });
    match image {
        Some(_) => output::arguments(format_args!(
            "GPU {address}: Video BIOS Table of {bytes} bytes from the OpRegion's {place} \
             (OpRegion {major}.{minor})\n"
        )),
        None => absent(&"it cannot be held as a firmware image"),
    }
    image
}

/// The files xe and the display code ask for on the Intel GPU with PCI
/// device ID `device`.
fn xe_files(device: u16) -> Result<Inventory, ServiceError> {
    let mut inventory = Inventory::default();
    stela_loader::host_firmware::recipes::require_xe(&mut inventory, device)
        .map_err(|_| ServiceError::Refused("the GPU has no pinned xe firmware recipe"))?;
    Ok(inventory)
}

/// The file i915's display code asks for on the Intel GPU with PCI device
/// ID `device`.
fn i915_files(device: u16) -> Result<Inventory, ServiceError> {
    let mut inventory = Inventory::default();
    stela_loader::host_firmware::recipes::require_i915(&mut inventory, device)
        .map_err(|_| ServiceError::Refused("the GPU has no pinned i915 firmware recipe"))?;
    Ok(inventory)
}
