//! The host's Bluetooth controller behind the guest's USB one
//! (`stela_soc::usb::hci`): HCI commands and ACL data the guest queued go to
//! the first controller probe left healthy, and its events and ACL data come
//! back as far as the guest's queues have room. The rest wait in the host
//! controller, or in the link (`stela_soc::usb::hci::bridge::Link`) when one
//! poll gave more, as they would for a host that reads slowly.
//!
//! The guest is given the controller model (`stela_soc::usb::hci::model`),
//! presented as Apple's internal USB module, which answers inside the
//! guest's function. With `--bluetooth-lend` the host's controller is lent
//! behind that module instead (`stela_soc::usb::hci::bridge`): the module's
//! shadow model answers every vendor command and the controller the
//! standard ones listed, until the controller fails or stalls and the model
//! answers everything (`BLUETOOTH LEND FELL BACK`). This reports the
//! model's unknown commands, each once, how many commands the guest sent,
//! at each doubling, and the function's trace: each configuration and
//! reset, and the first commands after each configuration with their
//! answers. With `--bluetooth-trace` it also logs every packet the function
//! captures (`GUEST BLUETOOTH RAW`, at most
//! `stela_soc::usb::hci::capture::CAPTURED_PACKETS` per configuration).
//!
//! One controller serves the guest; others are reported and left idle. A
//! controller that fails is reported once, and told to the guest's function:
//! a raw function's commands then get no answer, as from a controller that
//! stopped responding.
//!
//! The controllers' registers and DMA pages are reached through the loader's
//! identity root, so each exchange runs in it (`storage::RestoreRoot`). An
//! exchange does not wait for the host drivers' lock, which another
//! processor can hold for longer than a PE's exit may take: while it is
//! busy the guest's packets wait here and the controller's in the
//! controller, for the next service turn.
use super::{output, storage::RestoreRoot};
use alloc::{boxed::Box, format, string::String, vec::Vec};
use stela_boot_core::options::System;
use stela_drivers::{
    bluetooth::{bcm4377, intel_pcie},
    probe::native::{Bcm4377, IntelBluetooth, PcieController, PcieHci},
    usb::bluetooth::{Packet, UsbHci},
};
use stela_soc::{
    machine::{
        AppleDevices, Outbound, UsbBluetooth, UsbBluetoothCaptured, UsbBluetoothCounters,
        UsbBluetoothFallback, UsbBluetoothLendCounters, UsbBluetoothTrace,
    },
    usb::hci::{
        bridge::{HostController, Link},
        capture::LOG_PREFIX,
    },
};

enum Controller {
    Usb(Box<UsbHci>),
    Intel(Box<IntelBluetooth>),
    Broadcom(Box<Bcm4377>),
    /// The guest's function answers itself.
    Modeled,
}

/// The host drivers' transports, as a service turn reaches them.
impl HostController for Controller {
    fn send(&mut self, packet: &Outbound) -> Result<bool, String> {
        match (self, packet) {
            (Controller::Usb(hci), Outbound::Command(bytes)) => hci
                .submit_command(bytes)
                .map(|()| true)
                .map_err(|error| format!("{error}")),
            (Controller::Usb(hci), Outbound::Acl(bytes)) => hci
                .submit_acl(bytes)
                .map(|()| true)
                .map_err(|error| format!("{error}")),
            (Controller::Intel(transport), packet) => {
                let (kind, bytes) = match packet {
                    Outbound::Command(bytes) => (intel_pcie::Kind::Command, bytes),
                    Outbound::Acl(bytes) => (intel_pcie::Kind::Acl, bytes),
                };
                if !transport.ready() {
                    return Ok(false);
                }
                transport
                    .send(kind, bytes)
                    .map(|_| true)
                    .map_err(|error| format!("{error}"))
            }
            (Controller::Modeled, _) => Ok(true),
            (Controller::Broadcom(transport), packet) => {
                let (kind, bytes) = match packet {
                    Outbound::Command(bytes) => (bcm4377::Kind::Command, bytes),
                    Outbound::Acl(bytes) => (bcm4377::Kind::Acl, bytes),
                };
                match transport.send(kind, bytes) {
                    Ok(_) => Ok(true),
                    Err(bcm4377::Error::QueueFull(_)) => Ok(false),
                    Err(error) => Err(format!("{error}")),
                }
            }
        }
    }

    fn poll(&mut self) -> Result<Vec<Packet>, String> {
        match self {
            Controller::Modeled => Ok(Vec::new()),
            Controller::Usb(hci) => hci.poll().map_err(|error| format!("{error}")),
            Controller::Intel(transport) => transport
                .poll()
                .map(|packets| {
                    let useful = packets.into_iter().filter(|packet| !packet.diagnostic);
                    useful
                        .filter_map(|packet| match packet.kind {
                            intel_pcie::Kind::Event => Some(Packet::Event(packet.bytes)),
                            intel_pcie::Kind::Acl => Some(Packet::Acl(packet.bytes)),
                            _ => None,
                        })
                        .collect()
                })
                .map_err(|error| format!("{error}")),
            Controller::Broadcom(transport) => transport
                .poll()
                .map(|observations| {
                    let packets = observations.packets.into_iter();
                    packets
                        .filter_map(|packet| match packet.kind {
                            bcm4377::Kind::Event => Some(Packet::Event(packet.bytes)),
                            bcm4377::Kind::Acl => Some(Packet::Acl(packet.bytes)),
                            _ => None,
                        })
                        .collect()
                })
                .map_err(|error| format!("{error}")),
        }
    }
}

pub struct HostBluetooth {
    controller: Controller,
    location: String,
    root: u64,
    /// What the guest sent that the controller has no room for yet, and
    /// what it gave that the guest has none for.
    link: Link,
    up: Option<bool>,
    failed: bool,
    /// Whether the failure is still to be told to the guest's function.
    failure_due: bool,
    /// Commands the model answered Unknown HCI Command, to report.
    unknown: Vec<u16>,
    /// The command count reported last.
    reported_commands: u64,
    /// The guest function's trace, to report.
    trace: Vec<UsbBluetoothTrace>,
    /// Whether the function is still to be asked for its capture.
    capture: bool,
    /// The packets it captured, to report.
    captured: Vec<UsbBluetoothCaptured>,
    /// Whether the controller is lent behind Apple's module.
    lent: bool,
    /// What its bridge reported last: the fallback, its counters, and
    /// whether the guest wrote another address than the controller's.
    lend: Option<(Option<UsbBluetoothFallback>, UsbBluetoothLendCounters, bool)>,
    /// Whether the fallback and the address were reported, and the
    /// restarts reported.
    fell_back: bool,
    address_differs: bool,
    restarts: u64,
}

/// The controller is dropped in the loader's root. An install runs without
/// host input and drops the controller it found right after the device
/// start; a USB controller's drop closes its endpoints through the xHCI,
/// whose registers only that root maps.
impl Drop for HostBluetooth {
    fn drop(&mut self) {
        let controller = core::mem::replace(&mut self.controller, Controller::Modeled);
        super::network::in_loader_root(self.root, || drop(controller));
    }
}

impl HostBluetooth {
    /// The first controller `usb` and `pcie` hold that initialized.
    pub fn first(usb: Vec<UsbHci>, pcie: Vec<PcieHci>, root: u64) -> Option<Self> {
        let usb = usb.into_iter().filter_map(|hci| {
            let healthy = matches!(hci.initialization(), Some(Ok(_)) | None);
            let location = hci.binding().device().location.clone();
            healthy.then_some((Controller::Usb(Box::new(hci)), location))
        });
        let pcie = pcie.into_iter().filter_map(|hci| match hci.controller {
            PcieController::Intel {
                transport,
                initialization: Ok(_),
            } => Some((Controller::Intel(Box::new(transport)), hci.location)),
            PcieController::Broadcom {
                transport,
                initialization: Ok(_),
            } => Some((Controller::Broadcom(Box::new(transport)), hci.location)),
            _ => None,
        });
        let mut healthy = usb.chain(pcie);
        let (controller, location) = healthy.next()?;
        output::arguments(format_args!("BLUETOOTH guest uses {location}\n"));
        for (_, unused) in healthy {
            output::arguments(format_args!("BLUETOOTH unused {unused}\n"));
        }
        Some(Self {
            controller,
            location,
            root,
            link: Link::default(),
            up: None,
            failed: false,
            failure_due: false,
            unknown: Vec::new(),
            reported_commands: 0,
            trace: Vec::new(),
            capture: false,
            captured: Vec::new(),
            lent: false,
            lend: None,
            fell_back: false,
            address_differs: false,
            restarts: 0,
        })
    }

    /// The controller model in the guest's function, with no host
    /// controller behind it.
    pub fn modeled(root: u64) -> Self {
        Self {
            controller: Controller::Modeled,
            location: String::from("model"),
            root,
            link: Link::default(),
            up: None,
            failed: false,
            failure_due: false,
            unknown: Vec::new(),
            reported_commands: 0,
            trace: Vec::new(),
            capture: false,
            captured: Vec::new(),
            lent: false,
            lend: None,
            fell_back: false,
            address_differs: false,
            restarts: 0,
        }
    }

    /// The guest's controller for a boot with `options`, said once in the
    /// log: the model of Apple's internal USB module for every boot with
    /// host input. The host's `controller` is never lent raw: the guest's
    /// function would carry the host's vendor commands and replies, which
    /// BlueTool's chip boot and bluetoothd's bring-up do not take. With
    /// `--bluetooth-lend` it is lent behind the module, which answers those
    /// itself and falls back to the model on any failure, unless the boot
    /// has no host input or controller to lend, or leaps the guest's clock
    /// (`--idle-skip`), which the log then says. One not kept is released
    /// in the loader's root by its drop.
    pub fn guest(controller: Option<Self>, options: &System, root: u64) -> Option<Self> {
        let refused = options
            .bluetooth_lend
            .then(|| {
                stela_boot_core::guest_bluetooth::lend_refusal(
                    options.input,
                    controller.is_some(),
                    options.idle_skip,
                )
            })
            .flatten();
        if let Some(line) = refused {
            output::arguments(format_args!("{line}"));
        }
        let lend = options.bluetooth_lend && refused.is_none();
        let mut guest = match controller {
            Some(mut lent) if lend => {
                let line = stela_boot_core::guest_bluetooth::lend_line(&lent.location);
                output::arguments(format_args!("{line}"));
                lent.lent = true;
                lent
            }
            controller => {
                let line = stela_boot_core::guest_bluetooth::model_line(
                    controller.is_some(),
                    options.input,
                )?;
                output::arguments(format_args!("{line}"));
                Self::modeled(root)
            }
        };
        guest.capture = options.bluetooth_trace;
        if guest.capture {
            let line = stela_boot_core::guest_bluetooth::TRACE_LINE;
            output::arguments(format_args!("{line}"));
        }
        Some(guest)
    }

    /// The guest's USB function for this controller.
    pub fn kind(&self) -> UsbBluetooth {
        match self.controller {
            _ if self.lent => UsbBluetooth::Bridged,
            Controller::Modeled => UsbBluetooth::Model,
            _ => UsbBluetooth::Host,
        }
    }

    /// What the guest queued, how many packets it takes, and its status,
    /// taken from its devices for the exchange to run without them.
    pub fn queued(
        &mut self,
        devices: &mut AppleDevices,
    ) -> (Vec<Outbound>, usize, Option<(bool, UsbBluetoothCounters)>) {
        if self.lent || matches!(self.controller, Controller::Modeled) {
            self.unknown.extend(devices.bluetooth_unknown());
        }
        if self.lent {
            self.lend = devices.bluetooth_lend();
        }
        // Asked at the first service turn, before the guest's USB stack
        // can configure the function.
        if core::mem::take(&mut self.capture) {
            devices.bluetooth_capture();
        }
        self.trace.extend(devices.bluetooth_trace());
        self.captured.extend(devices.bluetooth_captured());
        let packets = core::iter::from_fn(|| devices.bluetooth_take()).collect();
        (
            packets,
            devices.bluetooth_room_left(),
            devices.bluetooth_status(),
        )
    }

    fn refuse(&mut self, what: &str, error: impl core::fmt::Display) {
        output::arguments(format_args!(
            "BLUETOOTH FAILED {} {what}: {error}\n",
            self.location
        ));
        self.failed = true;
        self.failure_due = true;
        self.link.clear();
    }

    /// Reports the guest's controller going up or down.
    pub fn status(&mut self, status: Option<(bool, UsbBluetoothCounters)>) {
        // Bounded by the function: its first commands after each
        // configuration, and its resets.
        for entry in self.trace.drain(..) {
            output::arguments(format_args!("GUEST BLUETOOTH {entry}\n"));
        }
        for packet in self.captured.drain(..) {
            output::arguments(format_args!("{LOG_PREFIX}{packet}\n"));
        }
        let (fallback, lend, differs) = self.lend.unwrap_or_default();
        if lend.restarts > self.restarts {
            use stela_soc::usb::hci::bridge::{RESTARTS, SETTLED_S};
            self.restarts = lend.restarts;
            output::arguments(format_args!(
                "BLUETOOTH LEND bluetoothd reset the guest's controller after its start-up \
                 (restart {}, {} of {RESTARTS} in a row); at {RESTARTS} in a row, each within \
                 {SETTLED_S} s of the start before it, the model takes over\n",
                lend.restarts, lend.streak,
            ));
        }
        if let Some(fallback) = fallback.filter(|_| !self.fell_back) {
            self.fell_back = true;
            output::arguments(format_args!("BLUETOOTH LEND FELL BACK {fallback}\n"));
        }
        if differs && !self.address_differs {
            self.address_differs = true;
            output::arguments(format_args!(
                "BLUETOOTH LEND the guest wrote a BD_ADDR its controller does not have, \
                 which keeps its own\n"
            ));
        }
        for opcode in self.unknown.drain(..) {
            output::arguments(format_args!(
                "BLUETOOTH model: unknown command {opcode:#06x} (OGF {:#04x} OCF {:#05x}) \
                 answered Unknown HCI Command\n",
                opcode >> 10,
                opcode & 0x3ff
            ));
        }
        let (up, counters) = status.unwrap_or_default();
        if (self.lent || matches!(self.controller, Controller::Modeled))
            && counters.commands > 0
            && counters.commands >= 2 * self.reported_commands
        {
            self.reported_commands = counters.commands;
            output::arguments(format_args!(
                "GUEST BLUETOOTH commands={} unknown={}\n",
                counters.commands, counters.unknown
            ));
            if self.lent {
                output::arguments(format_args!(
                    "GUEST BLUETOOTH LEND forwarded={} local={} passed={} dropped={}\n",
                    lend.forwarded, lend.local, lend.passed, lend.dropped
                ));
            }
        }
        if self.up != Some(up) {
            self.up = Some(up);
            output::arguments(format_args!(
                "GUEST BLUETOOTH {} {counters:?}\n",
                if up { "up" } else { "down" }
            ));
        }
    }

    /// Whether the lent controller's bridge fell back to the model, after
    /// which nothing goes to the controller and nothing is taken from it.
    fn fallen(&self) -> bool {
        self.lend.is_some_and(|(fallback, _, _)| fallback.is_some())
    }

    pub fn transmit(&mut self, packets: Vec<Outbound>) {
        // The model answers inside the guest's function.
        if self.failed || matches!(self.controller, Controller::Modeled) {
            return;
        }
        if self.fallen() {
            self.link.clear();
            return;
        }
        self.link.queue(packets);
        // SAFETY: the loader root `storage::open` captured, which maps the
        // controllers' registers and DMA pages for the whole run.
        let Some(_root) = (unsafe { RestoreRoot::try_enter(self.root) }) else {
            return;
        };
        if let Err(error) = self.link.send(&mut self.controller) {
            self.refuse("sending", error);
        }
    }

    /// What the controller has for the guest, at most `room` packets; the
    /// rest waits in the link for a later turn.
    pub fn receive(&mut self, room: usize) -> Vec<Packet> {
        if self.failed
            || room == 0
            || self.fallen()
            || matches!(self.controller, Controller::Modeled)
        {
            return Vec::new();
        }
        // SAFETY: as in `transmit`.
        let Some(_root) = (unsafe { RestoreRoot::try_enter(self.root) }) else {
            return Vec::new();
        };
        let received = self.link.receive(&mut self.controller, room);
        received.unwrap_or_else(|error| {
            self.refuse("receiving", error);
            Vec::new()
        })
    }

    /// Whether the controller's failure is still to be told to the guest's
    /// function.
    pub fn failure_due(&self) -> bool {
        self.failure_due
    }

    /// Hands `packets` to the guest's controller, then the host
    /// controller's failure, once. A vendor diagnostic frame is not ACL data
    /// and stays here.
    pub fn deliver(&mut self, devices: &mut AppleDevices, packets: Vec<Packet>) {
        for packet in packets {
            match packet {
                Packet::Event(bytes) => devices.bluetooth_event(&bytes),
                Packet::Acl(bytes) => devices.bluetooth_acl(&bytes),
                Packet::Diagnostic(_) => {}
            }
        }
        if core::mem::take(&mut self.failure_due) {
            devices.bluetooth_host_failed();
        }
    }
}
