//! The ACPI methods of AMD's graphics interface, which amdgpu calls where
//! Linux builds its ACPI support (`amdgpu_bios.c`, `amdgpu_atpx_handler.c`,
//! `amdgpu_acpi.c`). Stela's Linux profile does not build it, so the boot
//! calls the two a hybrid laptop's discrete GPU needs to be usable, as
//! amdgpu calls them, and names the rest once:
//!
//! - `ATRM` hands out the discrete GPU's video BIOS in 4 KiB pieces
//!   (`amdgpu_atrm_get_bios`); [`atrm`] reads it during the PCI census and
//!   the claim gives it to Linux as the function's platform ROM.
//! - `ATPX` function 2 powers the discrete GPU (`amdgpu_atpx_set_discrete_state`);
//!   [`atpx_power_on`] turns it on before the PCI census where it is off and
//!   its port has no power resources of its own.
//! - `ATPX`'s mux switching and runtime power-off, `ATIF` and `ATCS` are not
//!   called: [`METHODS`] says what amdgpu does without them.
//!
//! Each method is looked for where amdgpu looks: on the ACPI companion of a
//! display function of class VGA or other display, the first in census
//! order that holds it (`amdgpu_atrm_get_bios`, `amdgpu_atpx_detect`,
//! `amdgpu_acpi_detect`). The companion is the one Linux's
//! `acpi_pci_find_companion` gives the function, found once per display
//! function by the census (`Devices::gpu_companions`), which GSP-RM's ACPI
//! data is read from too. A method under a device that is no display
//! function's companion, as a firmware's node for a GPU the PC does not
//! have, is never called.
use super::d3cold::Topology;
use super::native::companion;
use crate::bus::pci::Address;
use crate::gpu::Adapter;
use crate::platform::acpi::{Argument, Error, Handle, Session, Value};
use alloc::{string::String, vec::Vec};

/// Each method the boot names once where the namespace holds it, and what
/// amdgpu does without it.
pub const METHODS: [(&str, &str); 3] = [
    (
        "ATPX",
        "the boot only powers the discrete GPU on through it when it is off; Linux's runtime \
         power-off and display mux switching through it are not done, so the discrete GPU \
         stays powered and the mux stays as firmware set it",
    ),
    (
        "ATIF",
        "the platform's display notifications go unheard and amdgpu sets the backlight itself",
    ),
    (
        "ATCS",
        "the PCIe link stays at the speed and width firmware trained",
    ),
];

/// Whether amdgpu looks for AMD's methods on a display function of `class`:
/// VGA or other display, not a 3D controller.
fn asked(class: [u8; 3]) -> bool {
    class[0] == 3 && matches!(class[1], 0 | 0x80)
}

/// The first of `adapters`, in census order, that amdgpu asks and whose
/// ACPI companion, of the `companions` the census found, holds `method`.
fn holder<'a>(
    session: &'a Session,
    adapters: &[Adapter],
    companions: &[(Address, String)],
    method: &str,
) -> Option<(Address, Handle<'a>)> {
    adapters
        .iter()
        .filter(|adapter| asked(adapter.class))
        .find_map(|adapter| {
            let (_, path) = companions
                .iter()
                .find(|(function, _)| *function == adapter.address)?;
            let device = session.handle(path).ok()?;
            device.child(method).ok()?;
            Some((adapter.address, device))
        })
}

/// The path of the display function's companion that holds each of
/// [`METHODS`], in that order.
pub fn find(
    session: &Session,
    adapters: &[Adapter],
    companions: &[(Address, String)],
) -> Vec<(&'static str, String)> {
    METHODS
        .iter()
        .filter_map(|&(method, _)| {
            let (_, device) = holder(session, adapters, companions, method)?;
            Some((method, device.path().unwrap_or_default()))
        })
        .collect()
}

/// `ATRM_BIOS_PAGE` and the 256 KiB `amdgpu_atrm_get_bios` reads at most.
const ATRM_PAGE: usize = 4096;
const ATRM_BYTES: usize = 256 * 1024;

/// A video BIOS read through `ATRM`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Atrm {
    /// The display function whose ACPI companion holds the method.
    pub owner: Address,
    /// The method's path.
    pub path: String,
    /// The pieces it returned, in order, up to the first short one.
    pub image: Vec<u8>,
}

/// `ATRM` as `amdgpu_atrm_get_bios` reads it: on the first display
/// function whose companion holds the method ([`holder`]), called for each
/// 4 KiB piece until one comes back short or the call fails, at most
/// 256 KiB.
pub(super) fn atrm(
    session: &Session,
    adapters: &[Adapter],
    companions: &[(Address, String)],
) -> Option<Atrm> {
    let (owner, method) = holder(session, adapters, companions, "ATRM")?;
    Some(Atrm {
        owner,
        path: method.path().unwrap_or_default(),
        image: read_atrm(method),
    })
}

/// The pieces `ATRM` under `device` returns, as [`atrm`] reads them.
pub fn read_atrm(device: Handle<'_>) -> Vec<u8> {
    let mut image = Vec::new();
    while image.len() < ATRM_BYTES {
        let offset = image.len() as u64;
        let piece = match device.evaluate(
            Some("ATRM"),
            &[
                Argument::Integer(offset),
                Argument::Integer(ATRM_PAGE as u64),
            ],
        ) {
            Ok(Some(Value::Buffer(piece))) => piece,
            _ => break,
        };
        let length = piece.len().min(ATRM_PAGE);
        image.extend_from_slice(&piece[..length]);
        if length < ATRM_PAGE {
            break;
        }
    }
    image
}

/// `ATPX` functions and the bits of their answers (`amd_acpi.h`).
const VERIFY_INTERFACE: u64 = 0;
const GET_PX_PARAMETERS: u64 = 1;
const POWER_CONTROL: u64 = 2;
const GET_PX_PARAMETERS_SUPPORTED: u32 = 1 << 0;
const POWER_CONTROL_SUPPORTED: u32 = 1 << 1;
const DYNAMIC_PX_SUPPORTED: u32 = 1 << 7;
const DYNAMIC_DGPU_POWER_OFF_SUPPORTED: u32 = 1 << 10;
const MS_HYBRID_GFX_SUPPORTED: u32 = 1 << 14;

/// What powering the discrete GPU on through `ATPX` did.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AtpxPower {
    /// No display function's companion, and no device below an empty
    /// port's, holds `ATPX`.
    Absent,
    /// `ATPX` at this path does not control the discrete GPU's power.
    NoPowerControl(String),
    /// `ATPX` at this path turned the discrete GPU on.
    On(String),
    /// A call failed with this ACPICA status.
    Failed(String, u32),
}

/// Call `ATPX` function `function` with `parameters` (or the fake second
/// integer amdgpu passes without), as `amdgpu_atpx_call` does.
fn atpx_call(
    device: Handle<'_>,
    function: u64,
    parameters: Option<&[u8]>,
) -> Result<Vec<u8>, Error> {
    let second = match parameters {
        Some(bytes) => Argument::Buffer(bytes),
        None => Argument::Integer(0),
    };
    match device.evaluate(Some("ATPX"), &[Argument::Integer(function), second])? {
        Some(Value::Buffer(answer)) => Ok(answer),
        _ => Ok(Vec::new()),
    }
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    bytes.get(at..at + 4).map_or(0, |word| {
        u32::from_le_bytes([word[0], word[1], word[2], word[3]])
    })
}

/// Whether the `ATPX` at `device` controls the discrete GPU's power, as
/// `amdgpu_atpx_verify_interface`, `amdgpu_atpx_parse_functions` and
/// `amdgpu_atpx_validate` decide it: the interface's power control bit, or
/// a PX parameter that implies it (dynamic PX, dynamic power-off, or
/// hybrid graphics, which amdgpu powers through `ATPX` where the port has
/// no power management of its own, the case this is called for).
pub fn atpx_controls_power(device: Handle<'_>) -> Result<bool, Error> {
    // `struct atpx_verify_interface`: size, version, supported functions.
    let interface = atpx_call(device, VERIFY_INTERFACE, None)?;
    let functions = u32_at(&interface, 4);
    let mut control = functions & POWER_CONTROL_SUPPORTED != 0;
    if functions & GET_PX_PARAMETERS_SUPPORTED != 0 {
        // `struct atpx_px_params`: size, valid flags, flags.
        let parameters = atpx_call(device, GET_PX_PARAMETERS, None)?;
        let size = parameters
            .get(..2)
            .map_or(0, |size| u16::from_le_bytes([size[0], size[1]]));
        if size >= 10 {
            let valid = u32_at(&parameters, 2) & u32_at(&parameters, 6);
            control |= valid
                & (DYNAMIC_PX_SUPPORTED
                    | DYNAMIC_DGPU_POWER_OFF_SUPPORTED
                    | MS_HYBRID_GFX_SUPPORTED)
                != 0;
        }
    }
    Ok(control)
}

/// The device holding the `ATPX` that powers a discrete GPU off the bus,
/// before the census: the companion of a display function the scan found,
/// as amdgpu takes it, else a device below the companion of one of `ports`,
/// the empty ports the GPU may be behind, where its own companion holds
/// the method and the function does not answer yet.
fn atpx_holder<'a>(
    session: &'a Session,
    topology: &Topology,
    ports: &[Address],
) -> Result<Option<Handle<'a>>, Error> {
    let bridges = topology.bridges();
    let shown = topology
        .found()
        .iter()
        .filter(|(_, class)| asked(*class))
        .find_map(|&(address, _)| {
            let device = companion(session, address, bridges)?;
            device.child("ATPX").ok()?;
            Some(device)
        });
    if shown.is_some() {
        return Ok(shown);
    }
    let devices = session.all_devices()?;
    Ok(ports.iter().find_map(|&port| {
        let mut below = companion(session, port, bridges)?.path().ok()?;
        below.push('.');
        devices.iter().copied().find(|device| {
            device.path().is_ok_and(|path| path.starts_with(&below)) && device.child("ATPX").is_ok()
        })
    }))
}

/// Turn the discrete GPU on through the `ATPX` that controls its power
/// ([`atpx_holder`]): `struct atpx_power_control` of 3 bytes with state 1,
/// as `amdgpu_atpx_set_discrete_state(atpx, ATPX_DISCRETE_GPU)`.
pub fn atpx_power_on(
    session: &Session,
    topology: &Topology,
    ports: &[Address],
) -> Result<AtpxPower, Error> {
    let Some(device) = atpx_holder(session, topology, ports)? else {
        return Ok(AtpxPower::Absent);
    };
    let path = device.path().unwrap_or_default();
    match atpx_controls_power(device) {
        Ok(true) => {}
        Ok(false) => return Ok(AtpxPower::NoPowerControl(path)),
        Err(error) => return Ok(AtpxPower::Failed(path, error.0)),
    }
    Ok(match atpx_call(device, POWER_CONTROL, Some(&[3, 0, 1])) {
        Ok(_) => AtpxPower::On(path),
        Err(error) => AtpxPower::Failed(path, error.0),
    })
}

#[cfg(test)]
mod tests;
