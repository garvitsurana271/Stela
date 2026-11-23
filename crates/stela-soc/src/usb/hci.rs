//! A native USB Bluetooth controller: the HCI USB transport, whose packets
//! the machine owner exchanges with a controller of the host, or which the
//! controller model ([`model::Model`]) answers.
//!
//! Evidence: Bluetooth Core v5.4 Vol 4 Part B 2 (HCI commands as class
//! request 0 on the control pipe, events on interrupt IN, ACL data on bulk
//! IN and OUT) and Part E 5.4 (packet lengths) and 7.7.14 (Command
//! Complete). J473 macOS binds no kernel driver: bluetoothd's USB transport
//! (bm3_usb, 26B123) opens the `IOUSBHostDevice` from user space, takes a
//! device of class 0xE0, 0xEF or 0xFF (0x100100f38), finds its interfaces
//! with subclass 1 and protocol 1 of any class (the find request built at
//! 0x1000fef04), and requires interface 0 with an interrupt IN, a bulk IN
//! and a bulk OUT pipe (0x1000ff16c..0x1000ff224).
//!
//! The function has one of two identities, chosen before the guest sees it:
//!
//! - [`Hci::new`], for a host controller lent to the guest: CSR's generic
//!   HCI identity, 0x0A12:0x0001, class 0xE0/0x01/0x01, whatever the host's
//!   controller is. That identity promises the standard HCI and no vendor
//!   protocol. A vendor command (OGF 0x3F) means something else to every
//!   other vendor's controller, so none reaches the host's: the function
//!   answers each with Command Complete, status Unknown HCI Command, and
//!   counts it.
//! - [`Hci::modeled`], for the controller model: Apple's internal Broadcom
//!   USB Bluetooth module, vendor 0x05AC, product 0x828F, bcdDevice 0x0150,
//!   product string "Bluetooth USB Host Controller". macOS on the USB
//!   transport accepts only that module: BlueTool's boot step (26B123
//!   `updateFirmwareInSerialFlash`, 0x10000e3a0..0x10000e434) waits for an
//!   IOService named exactly "Bluetooth USB Host Controller", a USB
//!   device's registry name being its product string, and for product
//!   0x828F with firmware v150 (bcdDevice 0x0150 read as BCD,
//!   0x10000e564..0x10000e58c) says "Firmware update not needed" and
//!   returns 0 (0x10000eed8..0x10000ef54); any other pair starts a DFU
//!   download. bluetoothd takes the same name as the internal controller
//!   (0x100100ffc..0x10010107c). Linux matches Apple's Broadcom modules by
//!   vendor 0x05AC and interface class 0xFF/0x01/0x01
//!   (`drivers/bluetooth/btusb.c` `btusb_table`, "Apple-specific (Broadcom)
//!   devices"), ahead of its generic class 0xE0 entries, so the module is
//!   not of class 0xE0/0x01/0x01: its device and interfaces are
//!   0xFF/0x01/0x01, which bluetoothd also takes. The manufacturer string
//!   names the vendor the ID belongs to. Linux also names a diagnostic
//!   interface at number 2 for these modules (`data->diag`); bluetoothd
//!   uses only interfaces 0 and 1 of this product (0x1000fef70) and Linux
//!   runs without it, so the function has none.
//! - [`Hci::bridged`], for a host controller lent behind that module
//!   (`--bluetooth-lend`): the same identity, with the [`bridge`] deciding
//!   for each command whether a shadow of the model answers it or the host's
//!   controller does, and falling back to the model when the controller
//!   fails or stalls. No vendor command reaches the host's controller.
//!
//! The configuration also carries the standard synchronous interface (Vol 4
//! Part B 2.1.1): interface 1, alternate settings 0-5, each with an
//! isochronous IN and OUT endpoint of 0, 9, 17, 25, 33 and 49 bytes.
//! Host stacks require it: macOS's takes a controller only when interface 0
//! has the interrupt and bulk pipes and interface 1 has both isochronous
//! pipes in each of alternate settings 0-5. Isochronous data is not
//! transported: the endpoints answer no transaction, so a SCO link carries
//! no audio. The model never forms one.
//!
//! HCI headers frame every packet, so an OUT transfer may span USB packets.
//! An IN packet that ends one on a full USB packet is followed by a
//! zero-length one, which ends the host's transfer. A packet of a length the
//! headers do not agree with is discarded and counted, never truncated. Each
//! direction's queue holds at most [`QUEUE_PACKETS`]: OUT NAKs while it is
//! full, and [`Hci::has_room`] tells the owner to leave packets with the
//! host's controller.
use super::{
    BusPeer, Function, Reply, Token,
    control::{
        self, CLASS, CLEAR_FEATURE, Control, DEVICE, DEVICE_QUALIFIER, Descriptors, ENDPOINT,
        ENDPOINT_HALT, GET_INTERFACE, GET_STATUS, INTERFACE, Outcome, RECIPIENT_DEVICE,
        RECIPIENT_ENDPOINT, RECIPIENT_INTERFACE, Requests, SET_FEATURE, SET_INTERFACE, STANDARD,
        Setup, Stall,
    },
};
use crate::Result;
use alloc::{boxed::Box, collections::VecDeque, string::String, vec, vec::Vec};
use stela_contracts::refuse;
use stela_drivers::usb::bluetooth::MAX_ACL_PAYLOAD;

pub mod bridge;
pub mod capture;
pub mod model;

const EVENTS: u8 = 0x81;
const ACL_IN: u8 = 0x82;
const ACL_OUT: u8 = 0x02;
const EVENT_PACKET: usize = 16;
const HIGH_SPEED_PACKET: u16 = 512;
const FULL_SPEED_PACKET: u16 = 64;
const INTERFACE_NUMBER: u16 = 0;
/// The synchronous interface and its endpoints.
const SCO_INTERFACE: u16 = 1;
const SCO_IN: u8 = 0x83;
const SCO_OUT: u8 = 0x03;
/// wMaxPacketSize of each alternate setting (Vol 4 Part B 2.1.1).
const SCO_PACKETS: [u8; 6] = [0, 9, 17, 25, 33, 49];
/// Packets each direction holds; the events a lent controller's guest
/// waits on go past it, to [`AWAITED_PACKETS`].
pub const QUEUE_PACKETS: usize = 64;
/// bRequest of an HCI command (Vol 4 Part B 2.2).
const HCI_COMMAND: u8 = 0;
/// A command's header: opcode and parameter length.
const COMMAND_HEADER: usize = 3;
/// An ACL packet's header: handle and flags, then data length.
const ACL_HEADER: usize = 4;
const VENDOR_GROUP: u16 = 0x3f;
const COMMAND_COMPLETE: u8 = 0x0e;
const COMMAND_STATUS: u8 = 0x0f;
const HCI_RESET: u16 = 0x0c03;
/// Commands traced after each configuration or HCI Reset, for the owner's
/// log.
pub const TRACED_COMMANDS: u16 = 48;
/// HCI Resets that start the trace again after each configuration: a
/// daemon that restarts begins each bring-up with one and need not
/// configure the function again.
const TRACED_BRING_UPS: u8 = 4;
/// Parameter bytes a traced command shows, masked as the capture masks them
/// (`capture`): an address keeps its last byte and vendor half, and names,
/// keys, vendor parameters and anything no rule names are `xx`.
pub const TRACED_BYTES: usize = 4;
/// Trace entries held until the owner takes them; later ones are dropped.
const TRACE_ENTRIES: usize = 64;
/// HCI error 0x01 (Vol 1 Part F 1.3).
const UNKNOWN_COMMAND: u8 = 0x01;

/// What the owner's log says of the function: the guest's first commands
/// after it configures the function, and its resets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Traced {
    /// A command the model answered: its opcode, parameter length and
    /// first parameter bytes, `None` where the mask hides one, and the
    /// event that answered it, if any, with its status and the length of
    /// its return parameters after the status.
    Command {
        opcode: u16,
        parameters: u8,
        first: [Option<u8>; TRACED_BYTES],
        answer: Option<Answered>,
    },
    /// The guest set the configuration to this value (0: unconfigured).
    Configured(u8),
    /// A USB bus reset.
    Reset,
    /// [`TRACED_COMMANDS`] commands were traced; an HCI Reset or the next
    /// configuration starts again.
    Ended,
    /// The capture (`capture`) took its packets for this configuration.
    CaptureEnded,
    /// The capture took its packets of the boot's last configuration it
    /// captures (`capture::CAPTURED_CONFIGURATIONS`), or another came after
    /// it.
    CaptureOver,
}

/// The event that answered a traced command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Answered {
    /// Command Complete or Command Status.
    pub event: u8,
    pub status: u8,
    pub returns: u8,
}

/// What `event` says of the command it answers, when it is a Command
/// Complete or Command Status.
fn answered(event: &[u8]) -> Option<Answered> {
    match *event {
        [COMMAND_COMPLETE, length, _, _, _, status, ..] => Some(Answered {
            event: COMMAND_COMPLETE,
            status,
            returns: length.saturating_sub(4),
        }),
        [COMMAND_STATUS, _, status, ..] => Some(Answered {
            event: COMMAND_STATUS,
            status,
            returns: 0,
        }),
        _ => None,
    }
}

/// Inquiry Complete and Remote Name Request Complete (Vol 4 Part E 7.7.1,
/// 7.7.7).
const INQUIRY_COMPLETE: u8 = 0x01;
const REMOTE_NAME_REQUEST_COMPLETE: u8 = 0x07;

/// Events the bridge queues for the guest past [`QUEUE_PACKETS`]: a full
/// queue of the controller's packets (`Hci::room_left` holds them to it),
/// then the answers of the command in flight and of a full backlog
/// (`bridge::BACKLOG`), and as many ends of the procedures they started. A
/// guest that commands on and never reads its events is still bounded.
const AWAITED_PACKETS: usize = QUEUE_PACKETS + 2 * (bridge::BACKLOG + 1);

/// Whether the guest waits on `event`, so that a full queue must not drop
/// it: a command's answer, or the end of a procedure it started (a
/// fallback's replacement events among them).
fn awaited(event: &[u8]) -> bool {
    matches!(
        event.first(),
        Some(
            &(COMMAND_COMPLETE | COMMAND_STATUS | INQUIRY_COMPLETE | REMOTE_NAME_REQUEST_COMPLETE)
        )
    )
}

/// The opcode a Command Complete or Command Status answers.
fn answered_opcode(event: &[u8]) -> Option<u16> {
    match *event {
        [COMMAND_COMPLETE, _, _, low, high, ..] | [COMMAND_STATUS, _, _, _, low, high, ..] => {
            Some(u16::from_le_bytes([low, high]))
        }
        _ => None,
    }
}

/// A command's first parameter bytes as the capture masks the whole
/// command, `None` for a hidden one or past its parameters. The raw bytes
/// are never held: the trace is in every boot's log.
fn first_bytes(packet: &[u8]) -> [Option<u8>; TRACED_BYTES] {
    let masked = capture::Captured::new(
        capture::Direction::FromGuest,
        capture::Kind::Command,
        packet,
    );
    let mut first = [None; TRACED_BYTES];
    let shown = masked.bytes.iter().zip(&masked.hidden).skip(COMMAND_HEADER);
    first
        .iter_mut()
        .zip(shown)
        .for_each(|(to, (&byte, &hidden))| *to = (!hidden).then_some(byte));
    first
}

impl core::fmt::Display for Traced {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match *self {
            Self::Command {
                opcode,
                parameters,
                first,
                answer,
            } => {
                write!(f, "command {opcode:#06x} parameters={parameters} ")?;
                let shown = &first[..usize::from(parameters).min(TRACED_BYTES)];
                if !shown.is_empty() {
                    f.write_str("[")?;
                    for (index, byte) in shown.iter().enumerate() {
                        f.write_str(if index == 0 { "" } else { " " })?;
                        match byte {
                            Some(byte) => write!(f, "{byte:02x}")?,
                            None => f.write_str("xx")?,
                        }
                    }
                    f.write_str("] ")?;
                }
                match answer {
                    Some(Answered {
                        event,
                        status,
                        returns,
                    }) => {
                        let kind = if event == COMMAND_STATUS {
                            "status"
                        } else {
                            "complete"
                        };
                        write!(f, "answered {kind} status={status:#04x} returns={returns}")
                    }
                    None => f.write_str("answered with no event"),
                }
            }
            Self::Configured(0) => f.write_str("unconfigured"),
            Self::Configured(value) => write!(f, "configured {value}"),
            Self::Reset => f.write_str("bus reset"),
            Self::Ended => write!(
                f,
                "trace ends after {TRACED_COMMANDS} commands until the next HCI Reset or configuration"
            ),
            Self::CaptureEnded => write!(
                f,
                "capture ends after {} packets until the next configuration",
                capture::CAPTURED_PACKETS
            ),
            Self::CaptureOver => write!(
                f,
                "capture ends for this boot: at most {} packets of each of its first {} \
                 configurations",
                capture::CAPTURED_PACKETS,
                capture::CAPTURED_CONFIGURATIONS
            ),
        }
    }
}

/// What the guest sent for the host's controller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outbound {
    Command(Vec<u8>),
    Acl(Vec<u8>),
}

/// Counters for the owner's diagnostics.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counters {
    pub commands: u64,
    /// Vendor commands answered Unknown HCI Command here.
    pub vendor_refused: u64,
    pub acl_to_host: u64,
    pub events_to_guest: u64,
    pub acl_to_guest: u64,
    /// Packets whose lengths disagree with their headers, discarded.
    pub malformed: u64,
    pub overflowed: u64,
    /// Commands the model answered Unknown HCI Command, vendor ones among
    /// them.
    pub unknown: u64,
    /// ACL packets the model discarded: they named no connection.
    pub acl_discarded: u64,
}

/// One IN endpoint's packets, sent in USB packets.
#[derive(Debug, Clone, Default)]
struct Stream {
    queue: VecDeque<Vec<u8>>,
    sent: usize,
    zero_length_due: bool,
}

impl Stream {
    fn clear(&mut self) {
        *self = Self::default();
    }

    fn read(&mut self, data: &mut [u8], maximum: usize) -> Reply {
        let Some(packet) = self.queue.front() else {
            return Reply::Nak;
        };
        if self.zero_length_due {
            self.zero_length_due = false;
            self.queue.pop_front();
            self.sent = 0;
            return Reply::Ack(0);
        }
        let n = (packet.len() - self.sent).min(data.len()).min(maximum);
        data[..n].copy_from_slice(&packet[self.sent..self.sent + n]);
        self.sent += n;
        if self.sent == packet.len() {
            if n == maximum {
                self.zero_length_due = true;
            } else {
                self.queue.pop_front();
                self.sent = 0;
            }
        }
        Reply::Ack(n)
    }
}

#[derive(Debug, Clone)]
struct State {
    /// Halted: EVENTS, ACL_IN, ACL_OUT.
    halted: [bool; 3],
    /// The synchronous interface's alternate setting.
    sco_setting: u8,
    outbound: VecDeque<Outbound>,
    events: Stream,
    acl: Stream,
    assembling: Vec<u8>,
    counters: Counters,
    /// The controller model, when no host controller is behind the function.
    model: Option<Box<model::Model>>,
    /// The system count of the latest transactions.
    now: u64,
    /// The owner's log, until taken.
    trace: Vec<Traced>,
    /// Commands still traced since the last configuration.
    tracing: u16,
    /// The configuration value last traced.
    configured: u8,
    /// HCI Resets that may still start the trace again.
    bring_ups: u8,
    /// Every packet, once the owner asks (`--bluetooth-trace`).
    capture: capture::Capture,
    /// The bridge, when a host controller is lent behind the module.
    bridge: Option<Box<bridge::Bridge>>,
    /// Traced commands of the bridge whose answers have not reached the
    /// guest yet: opcode, parameter length, first parameter bytes.
    awaiting: VecDeque<(u16, u8, [Option<u8>; TRACED_BYTES])>,
}

/// The function: its pipes and queues.
#[derive(Debug, Clone)]
pub struct Hci {
    control: Control,
    state: State,
}

/// What the function says it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Identity {
    /// Class, subclass and protocol of the device and its interfaces.
    class: [u8; 3],
    vendor: u16,
    product: u16,
    release: u16,
    manufacturer: &'static str,
    name: &'static str,
}

/// CSR's generic HCI identity, for a host controller.
const GENERIC: Identity = Identity {
    class: [0xe0, 0x01, 0x01],
    vendor: 0x0a12,
    product: 0x0001,
    release: 0x0100,
    manufacturer: "Stela",
    name: "Stela Bluetooth",
};

/// Apple's internal Broadcom USB Bluetooth module, for the model.
const APPLE_MODULE: Identity = Identity {
    class: [0xff, 0x01, 0x01],
    vendor: model::USB_VENDOR,
    product: model::USB_PRODUCT,
    release: 0x0150,
    manufacturer: "Apple Inc.",
    name: "Bluetooth USB Host Controller",
};

fn descriptors(identity: &Identity) -> Descriptors {
    let [class, subclass, protocol] = identity.class;
    let [vendor_low, vendor_high] = identity.vendor.to_le_bytes();
    let [product_low, product_high] = identity.product.to_le_bytes();
    let [release_low, release_high] = identity.release.to_le_bytes();
    let device = [
        18,
        DEVICE,
        0x00,
        0x02,
        class,
        subclass,
        protocol,
        control::EP0_PACKET as u8,
        vendor_low,
        vendor_high,
        product_low,
        product_high,
        release_low,
        release_high,
        1,
        2,
        0,
        1,
    ];
    let configuration = |bulk: u16, interval: u8| {
        let [low, high] = bulk.to_le_bytes();
        let mut parts = vec![
            vec![9, INTERFACE, 0, 0, 3, class, subclass, protocol, 0],
            vec![7, ENDPOINT, EVENTS, 3, EVENT_PACKET as u8, 0, interval],
            vec![7, ENDPOINT, ACL_IN, 2, low, high, 0],
            vec![7, ENDPOINT, ACL_OUT, 2, low, high, 0],
        ];
        for (setting, &size) in SCO_PACKETS.iter().enumerate() {
            parts.push(vec![
                9,
                INTERFACE,
                1,
                setting as u8,
                2,
                class,
                subclass,
                protocol,
                0,
            ]);
            for address in [SCO_IN, SCO_OUT] {
                // Isochronous, no synchronization, one packet a frame.
                parts.push(vec![7, ENDPOINT, address, 0x01, size, 0, interval]);
            }
        }
        let parts: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
        control::configuration(1, 0, &parts)
    };
    let qualifier = [
        10,
        DEVICE_QUALIFIER,
        0x00,
        0x02,
        class,
        subclass,
        protocol,
        control::EP0_PACKET as u8,
        1,
        0,
    ];
    Descriptors {
        device,
        // High speed: 2^(4-1) microframes, 1 ms; full speed: 1 frame.
        configuration: configuration(HIGH_SPEED_PACKET, 4),
        other_speed: Some((qualifier, configuration(FULL_SPEED_PACKET, 1))),
        strings: vec![
            String::from(identity.manufacturer),
            String::from(identity.name),
        ],
    }
}

impl Hci {
    /// The function for a host controller: its packets go to the owner.
    pub fn new() -> Self {
        Self::with(&GENERIC, None)
    }

    /// Apple's internal USB Bluetooth module, answered by the controller
    /// model, whose power-on BD_ADDR is `address`.
    pub fn modeled(address: [u8; 6]) -> Self {
        Self::with(&APPLE_MODULE, Some(Box::new(model::Model::new(address))))
    }

    /// Apple's internal USB Bluetooth module with a host controller lent
    /// behind it (`bridge`), whose shadow model's power-on BD_ADDR is
    /// `address`. The owner carries the commands the bridge sends the
    /// controller ([`Self::take`]) and its packets back
    /// ([`Self::deliver_event`], [`Self::deliver_acl`]), and says when it
    /// fails ([`Self::host_failed`]).
    pub fn bridged(address: [u8; 6]) -> Self {
        let mut hci = Self::with(&APPLE_MODULE, None);
        hci.state.bridge = Some(Box::new(bridge::Bridge::new(address)));
        hci
    }

    fn with(identity: &Identity, model: Option<Box<model::Model>>) -> Self {
        Self {
            control: Control::new(descriptors(identity)),
            state: State {
                halted: [false; 3],
                sco_setting: 0,
                outbound: VecDeque::new(),
                events: Stream::default(),
                acl: Stream::default(),
                assembling: Vec::new(),
                counters: Counters::default(),
                model,
                now: 0,
                trace: Vec::new(),
                tracing: 0,
                configured: 0,
                bring_ups: 0,
                capture: capture::Capture::default(),
                bridge: None,
                awaiting: VecDeque::new(),
            },
        }
    }

    /// Whether the guest configured the function.
    pub fn up(&self) -> bool {
        self.control.configuration() != 0
    }

    pub fn counters(&self) -> Counters {
        self.state.counters
    }

    /// Whether both IN queues have room for a packet from the host's
    /// controller, which may hold one of each.
    pub fn has_room(&self) -> bool {
        let state = &self.state;
        state.events.queue.len() < QUEUE_PACKETS && state.acl.queue.len() < QUEUE_PACKETS
    }

    /// How many packets from the host's controller both IN queues still
    /// take.
    pub fn room_left(&self) -> usize {
        let state = &self.state;
        let fuller = state.events.queue.len().max(state.acl.queue.len());
        QUEUE_PACKETS.saturating_sub(fuller)
    }

    /// The model's BD_ADDR now, the bridge's shadow's when bridged; `None`
    /// for a host controller's function.
    pub fn model_address(&self) -> Option<[u8; 6]> {
        let state = &self.state;
        let bridged = state.bridge.as_ref().map(|bridge| bridge.address());
        bridged.or_else(|| state.model.as_ref().map(|model| model.address()))
    }

    /// Opcodes the model (or the bridge's shadow) answered Unknown HCI
    /// Command that were not taken before, each once.
    pub fn take_unknown(&mut self) -> Vec<u16> {
        let state = &mut self.state;
        if let Some(bridge) = state.bridge.as_mut() {
            return bridge.take_unknown();
        }
        state
            .model
            .as_mut()
            .map(|model| model.take_unknown())
            .unwrap_or_default()
    }

    /// Why the bridge fell back to the model, once it has.
    pub fn fallback(&self) -> Option<bridge::Fallback> {
        self.state.bridge.as_ref()?.fallen()
    }

    /// The bridge's counters; `None` unless bridged.
    pub fn bridge_counters(&self) -> Option<bridge::Counters> {
        Some(self.state.bridge.as_ref()?.counters())
    }

    /// Whether the guest wrote a BD_ADDR the lent controller does not have
    /// (it keeps its own).
    pub fn bridged_address_differs(&self) -> bool {
        self.state
            .bridge
            .as_ref()
            .is_some_and(|bridge| bridge.address_differs())
    }

    /// The lent host controller failed: the bridge falls back to the model,
    /// which answers the command in flight.
    pub fn host_failed(&mut self) {
        let state = &mut self.state;
        let Some(bridge) = state.bridge.as_mut() else {
            return;
        };
        let steps = bridge.chip_failed(state.now);
        state.steps(steps);
    }

    /// The function's trace since it was last taken.
    pub fn take_trace(&mut self) -> Vec<Traced> {
        core::mem::take(&mut self.state.trace)
    }

    /// Capture every packet from now on, [`capture::CAPTURED_PACKETS`] per
    /// configuration (`--bluetooth-trace`).
    pub fn capture(&mut self) {
        let configured = self.up();
        self.state.capture.enable(configured);
    }

    /// The packets captured since they were last taken.
    pub fn take_captured(&mut self) -> Vec<capture::Captured> {
        self.state.capture.take()
    }

    /// The next packet the guest sent for the host's controller; none once
    /// the bridge fell back, which drops what was still queued.
    pub fn take(&mut self) -> Option<Outbound> {
        let state = &mut self.state;
        if state.bridge.as_ref().is_some_and(|b| b.fallen().is_some()) {
            state.outbound.clear();
            return None;
        }
        state.outbound.pop_front()
    }

    /// An event from the host's controller for the guest. Dropped (and
    /// counted) while the function is unconfigured or the queue is full.
    /// The bridge, when there is one, decides what the guest gets of it,
    /// and still hears a Hardware Error while the function is unconfigured.
    pub fn deliver_event(&mut self, packet: &[u8]) {
        let state = &mut self.state;
        if self.control.configuration() == 0 {
            let now = state.now;
            if let Some(bridge) = state.bridge.as_mut() {
                bridge.chip_event_unconfigured(packet, now);
            }
            return;
        }
        if state.bridge.is_some() {
            state.captured(capture::Direction::FromChip, capture::Kind::Event, packet);
        }
        let now = state.now;
        match state.bridge.as_mut() {
            Some(bridge) => {
                let steps = bridge.chip_event(packet, now);
                state.steps(steps);
            }
            None => state.event(packet.to_vec()),
        }
    }

    /// ACL data from the host's controller for the guest, dropped as an
    /// event is, and always behind a bridge, which in milestone 1 lends no
    /// connection (`bridge::Bridge::chip_acl`).
    pub fn deliver_acl(&mut self, packet: &[u8]) {
        if !self.up() {
            return;
        }
        let state = &mut self.state;
        if let Some(bridge) = state.bridge.as_mut() {
            let passes = bridge.chip_acl();
            state.captured(capture::Direction::FromChip, capture::Kind::Acl, packet);
            if !passes {
                return;
            }
        }
        if state.acl.queue.len() >= QUEUE_PACKETS {
            state.counters.overflowed += 1;
            return;
        }
        state.counters.acl_to_guest += 1;
        state.captured(capture::Direction::ToGuest, capture::Kind::Acl, packet);
        state.acl.queue.push_back(packet.to_vec());
    }
}

impl Default for Hci {
    fn default() -> Self {
        Self::new()
    }
}

impl State {
    fn trace(&mut self, entry: Traced) {
        if self.trace.len() < TRACE_ENTRIES {
            self.trace.push(entry);
        }
    }

    fn event(&mut self, packet: Vec<u8>) {
        self.queue_event(packet, QUEUE_PACKETS);
    }

    /// Queues `packet` while the queue holds fewer than `bound`.
    fn queue_event(&mut self, packet: Vec<u8>, bound: usize) {
        if self.events.queue.len() >= bound {
            self.counters.overflowed += 1;
            return;
        }
        self.counters.events_to_guest += 1;
        self.captured(capture::Direction::ToGuest, capture::Kind::Event, &packet);
        self.events.queue.push_back(packet);
    }

    fn captured(&mut self, direction: capture::Direction, kind: capture::Kind, packet: &[u8]) {
        let end = self.capture.record(direction, kind, packet);
        self.capture_ended(end);
    }

    fn capture_ended(&mut self, end: Option<capture::End>) {
        match end {
            Some(capture::End::Configuration) => self.trace(Traced::CaptureEnded),
            Some(capture::End::Boot) => self.trace(Traced::CaptureOver),
            None => {}
        }
    }

    fn modeled_command(&mut self, packet: &[u8]) {
        let Some(model) = self.model.as_mut() else {
            return;
        };
        self.counters.commands += 1;
        if !model::implements(u16::from_le_bytes([packet[0], packet[1]])) {
            self.counters.unknown += 1;
        }
        let opcode = u16::from_le_bytes([packet[0], packet[1]]);
        if opcode == HCI_RESET && self.bring_ups > 0 {
            self.bring_ups -= 1;
            self.tracing = TRACED_COMMANDS;
        }
        let events = model.command(packet, self.now);
        if self.tracing > 0 {
            self.tracing -= 1;
            // The first event answers the command; a procedure's later
            // ones follow it.
            let answer = events.first().and_then(|e| answered(e));
            self.trace(Traced::Command {
                opcode,
                parameters: packet[2],
                first: first_bytes(packet),
                answer,
            });
            if self.tracing == 0 {
                self.trace(Traced::Ended);
            }
        }
        events.into_iter().for_each(|e| self.event(e));
    }

    /// A command for the bridge, traced once its answer reaches the guest.
    fn bridged_command(&mut self, packet: &[u8]) {
        let Some(lent) = self.bridge.as_mut() else {
            return;
        };
        let opcode = u16::from_le_bytes([packet[0], packet[1]]);
        self.counters.commands += 1;
        self.counters.unknown += u64::from(!model::implements(opcode));
        if opcode == HCI_RESET && self.bring_ups > 0 {
            self.bring_ups -= 1;
            self.tracing = TRACED_COMMANDS;
        }
        let traced = self.tracing > 0;
        if traced {
            self.tracing -= 1;
            self.awaiting
                .push_back((opcode, packet[2], first_bytes(packet)));
        }
        let waits = lent.busy();
        let steps = lent.command(packet, self.now);
        // In flight now, sent or held for the controller's credit, its
        // answer comes later; else the shadow's came with it, or none does.
        let answers = lent.busy()
            || steps.iter().any(|step| {
                matches!(step, bridge::Step::Guest(event) if answered_opcode(event) == Some(opcode))
            });
        self.steps(steps);
        // Host Number Of Completed Packets, answered by no event.
        if traced && !waits && !answers {
            self.awaiting.pop_back();
            self.trace(Traced::Command {
                opcode,
                parameters: packet[2],
                first: first_bytes(packet),
                answer: None,
            });
            self.trace_ended();
        }
    }

    /// What the bridge decided: events for the guest, commands for the
    /// owner to carry to the host's controller.
    fn steps(&mut self, steps: Vec<bridge::Step>) {
        for step in steps {
            match step {
                bridge::Step::Guest(event) => {
                    self.trace_answer(&event);
                    // The controller's packets are held to `room_left`, so
                    // a queue it filled still takes what the guest waits on.
                    let bound = if awaited(&event) {
                        AWAITED_PACKETS
                    } else {
                        QUEUE_PACKETS
                    };
                    self.queue_event(event, bound);
                }
                bridge::Step::Chip(command) => {
                    let command = command.into_bytes();
                    self.captured(capture::Direction::ToChip, capture::Kind::Command, &command);
                    self.outbound.push_back(Outbound::Command(command));
                }
            }
        }
    }

    /// Traces the bridged command `event` answers.
    fn trace_answer(&mut self, event: &[u8]) {
        let (Some(opcode), Some(answer)) = (answered_opcode(event), answered(event)) else {
            return;
        };
        let Some(&(_, parameters, first)) = self.awaiting.front().filter(|a| a.0 == opcode) else {
            return;
        };
        self.awaiting.pop_front();
        self.trace(Traced::Command {
            opcode,
            parameters,
            first,
            answer: Some(answer),
        });
        self.trace_ended();
    }

    fn trace_ended(&mut self) {
        if self.tracing == 0 && self.awaiting.is_empty() {
            self.trace(Traced::Ended);
        }
    }

    fn modeled_acl(&mut self, packet: &[u8]) {
        let Some(model) = self.model.as_mut() else {
            return;
        };
        self.counters.acl_discarded += 1;
        let events = model.acl(packet);
        events.into_iter().for_each(|e| self.event(e));
    }

    fn halt(&mut self, endpoint: u8) -> Option<&mut bool> {
        match endpoint {
            EVENTS => Some(&mut self.halted[0]),
            ACL_IN => Some(&mut self.halted[1]),
            ACL_OUT => Some(&mut self.halted[2]),
            _ => None,
        }
    }

    /// One HCI command from the control pipe.
    fn command(&mut self, packet: &[u8]) -> Result<(), Stall> {
        let &[low, high, length, ..] = packet else {
            return Err(Stall);
        };
        if packet.len() != COMMAND_HEADER + usize::from(length) {
            self.counters.malformed += 1;
            return Err(Stall);
        }
        if self
            .bridge
            .as_ref()
            .is_some_and(|bridge| !bridge.has_room())
        {
            return Err(Stall);
        }
        self.captured(
            capture::Direction::FromGuest,
            capture::Kind::Command,
            packet,
        );
        let opcode = u16::from_le_bytes([low, high]);
        let vendor = opcode >> 10 == VENDOR_GROUP;
        if self.model.is_some() {
            // The module answers the Broadcom commands it models.
            self.counters.vendor_refused += u64::from(vendor && !model::implements(opcode));
            self.modeled_command(packet);
            return Ok(());
        }
        if self.bridge.is_some() {
            // The shadow answers the vendor commands it models.
            self.counters.vendor_refused += u64::from(vendor && !model::implements(opcode));
            self.bridged_command(packet);
            return Ok(());
        }
        if vendor {
            self.counters.vendor_refused += 1;
            self.event(vec![COMMAND_COMPLETE, 4, 1, low, high, UNKNOWN_COMMAND]);
            return Ok(());
        }
        if self.outbound.len() >= QUEUE_PACKETS {
            return Err(Stall);
        }
        self.counters.commands += 1;
        self.outbound.push_back(Outbound::Command(packet.to_vec()));
        Ok(())
    }

    fn acl_out(&mut self, packet: &[u8]) -> Reply {
        if self.outbound.len() >= QUEUE_PACKETS {
            // The host retries until the owner drains the queue.
            return Reply::Nak;
        }
        self.assembling.extend_from_slice(packet);
        let Some(&[_, _, low, high]) = self.assembling.first_chunk() else {
            return Reply::Ack(packet.len());
        };
        let data = usize::from(u16::from_le_bytes([low, high]));
        if data > MAX_ACL_PAYLOAD {
            self.counters.malformed += 1;
            self.assembling.clear();
        } else if self.assembling.len() == ACL_HEADER + data {
            let packet = core::mem::take(&mut self.assembling);
            self.captured(capture::Direction::FromGuest, capture::Kind::Acl, &packet);
            if self.model.is_some() {
                self.modeled_acl(&packet);
            } else if let Some(bridge) = self.bridge.as_mut() {
                // Milestone 1 lends no connection: the shadow owns every
                // one, so it returns the buffer as the model does.
                self.counters.acl_discarded += 1;
                let steps = bridge.acl(&packet);
                self.steps(steps);
            } else {
                self.counters.acl_to_host += 1;
                self.outbound.push_back(Outbound::Acl(packet));
            }
        } else if self.assembling.len() > ACL_HEADER + data {
            self.counters.malformed += 1;
            self.assembling.clear();
        }
        Reply::Ack(packet.len())
    }
}

impl Requests for State {
    fn configure(&mut self, value: u8) -> Result<(), Stall> {
        if value != self.configured {
            self.configured = value;
            self.trace(Traced::Configured(value));
        }
        (self.tracing, self.bring_ups) = if value == 0 {
            (0, 0)
        } else {
            (TRACED_COMMANDS, TRACED_BRING_UPS)
        };
        let end = self.capture.configured(value);
        self.capture_ended(end);
        self.halted = [false; 3];
        self.sco_setting = 0;
        self.outbound.clear();
        self.events.clear();
        self.acl.clear();
        self.assembling.clear();
        // A command the queues dropped is no longer in flight.
        if let Some(bridge) = self.bridge.as_mut() {
            bridge.resync();
        }
        self.awaiting.clear();
        Ok(())
    }

    fn request(&mut self, setup: Setup, data: &[u8]) -> Result<Outcome, Stall> {
        let read = setup.device_to_host();
        match (setup.kind(), setup.recipient(), setup.request, read) {
            // Vol 4 Part B 2.2 names the device; hosts also send it to the
            // interface.
            (CLASS, RECIPIENT_DEVICE | RECIPIENT_INTERFACE, HCI_COMMAND, false) => {
                self.command(data).map(|()| Outcome::Done)
            }
            (STANDARD, RECIPIENT_INTERFACE, GET_STATUS, true)
                if setup.index == INTERFACE_NUMBER || setup.index == SCO_INTERFACE =>
            {
                Ok(Outcome::Data(vec![0, 0]))
            }
            (STANDARD, RECIPIENT_INTERFACE, GET_INTERFACE, true)
                if setup.index == SCO_INTERFACE =>
            {
                Ok(Outcome::Data(vec![self.sco_setting]))
            }
            (STANDARD, RECIPIENT_INTERFACE, SET_INTERFACE, false)
                if setup.index == SCO_INTERFACE && usize::from(setup.value) < SCO_PACKETS.len() =>
            {
                self.sco_setting = setup.value as u8;
                Ok(Outcome::Done)
            }
            (STANDARD, RECIPIENT_INTERFACE, GET_INTERFACE, true)
                if setup.index == INTERFACE_NUMBER =>
            {
                Ok(Outcome::Data(vec![0]))
            }
            (STANDARD, RECIPIENT_INTERFACE, SET_INTERFACE, false)
                if setup.index == INTERFACE_NUMBER && setup.value == 0 =>
            {
                Ok(Outcome::Done)
            }
            (STANDARD, RECIPIENT_ENDPOINT, GET_STATUS, true) => {
                let halted = *self.halt(setup.index as u8).ok_or(Stall)?;
                Ok(Outcome::Data(vec![u8::from(halted), 0]))
            }
            (STANDARD, RECIPIENT_ENDPOINT, CLEAR_FEATURE | SET_FEATURE, false)
                if setup.value == ENDPOINT_HALT =>
            {
                *self.halt(setup.index as u8).ok_or(Stall)? = setup.request == SET_FEATURE;
                Ok(Outcome::Done)
            }
            _ => Err(Stall),
        }
    }
}

impl BusPeer for Hci {
    fn reset(&mut self) -> Result<()> {
        self.state.trace(Traced::Reset);
        // A bus reset powers the controller up again.
        if let Some(model) = self.state.model.as_mut() {
            model.power_on();
        }
        // The shadow's, not the host's: a USB reset of the real controller
        // would lose its patched firmware.
        if let Some(bridge) = self.state.bridge.as_mut() {
            bridge.reset();
        }
        self.control.reset(&mut self.state)?;
        self.state
            .configure(0)
            .map_err(|Stall| refuse!("USB Bluetooth reset stalled"))
    }

    fn packet(&mut self, endpoint: u8, token: Token, data: &mut [u8]) -> Result<Reply> {
        if endpoint == 0 {
            return self.control.packet(token, data, &mut self.state);
        }
        if token == Token::Setup {
            return Err(refuse!("SETUP to a non-control endpoint"));
        }
        let configured = self.up();
        let state = &mut self.state;
        if state.halt(endpoint).is_some_and(|halted| *halted) {
            return Ok(Reply::Stall);
        }
        Ok(match (endpoint, token) {
            (EVENTS, Token::In) if configured => state.events.read(data, EVENT_PACKET),
            // The controller sits on a high-speed port.
            (ACL_IN, Token::In) if configured => {
                state.acl.read(data, usize::from(HIGH_SPEED_PACKET))
            }
            (ACL_OUT, Token::Out) if configured => state.acl_out(data),
            _ => Reply::Stall,
        })
    }
}

impl Function for Hci {
    fn set_time(&mut self, now: u64) {
        let state = &mut self.state;
        state.now = now;
        if let Some(model) = state.model.as_mut() {
            let events = model.advance(now);
            events.into_iter().for_each(|e| state.event(e));
        }
        if let Some(bridge) = state.bridge.as_mut() {
            let steps = bridge.advance(now);
            state.steps(steps);
        }
    }

    fn ready_at(&self, endpoint: u8) -> Option<u64> {
        if !self.up() {
            return None;
        }
        let state = &self.state;
        match endpoint {
            EVENTS if !state.events.queue.is_empty() => Some(0),
            // A procedure's end is an event then, and a forwarded command's
            // deadline the model's answer.
            EVENTS => match state.bridge.as_ref() {
                Some(bridge) => bridge.deadline(),
                None => state.model.as_ref().and_then(|model| model.deadline()),
            },
            ACL_IN => (!state.acl.queue.is_empty()).then_some(0),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usb::control::{
        SET_ADDRESS, SET_CONFIGURATION,
        tests::{setup, transfer},
    };

    fn configured(mut hci: Hci) -> Hci {
        transfer(&mut hci, setup(0, SET_ADDRESS, 1, 0, 0), &[]).unwrap();
        transfer(&mut hci, setup(0, SET_CONFIGURATION, 1, 0, 0), &[]).unwrap();
        hci
    }

    fn controller() -> Hci {
        configured(Hci::new())
    }

    const ADDRESS: [u8; 6] = [1, 2, 3, 4, 5, 0x02];

    fn modeled() -> Hci {
        configured(Hci::modeled(ADDRESS))
    }

    /// A string descriptor's text (UTF-16LE after its two-byte header).
    fn string(hci: &mut Hci, index: u8) -> String {
        let bytes = transfer(
            hci,
            setup(0x80, 6, 0x0300 | u16::from(index), 0x0409, 255),
            &[],
        )
        .unwrap();
        assert_eq!(usize::from(bytes[0]), bytes.len());
        let units: Vec<u16> = bytes[2..]
            .chunks(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16(&units).unwrap()
    }

    fn command(hci: &mut Hci, packet: &[u8]) -> Result<(), ()> {
        let request = setup(0x20, HCI_COMMAND, 0, 0, packet.len() as u16);
        transfer(hci, request, packet).map(|_| ()).map_err(|_| ())
    }

    /// An IN transfer's packets until one is short.
    fn read(hci: &mut Hci, endpoint: u8, maximum: usize) -> Vec<Vec<u8>> {
        let mut packets = Vec::new();
        loop {
            let mut buffer = vec![0; maximum];
            let Reply::Ack(n) = hci.packet(endpoint, Token::In, &mut buffer).unwrap() else {
                panic!("the endpoint had a packet to send");
            };
            packets.push(buffer[..n].to_vec());
            if n < maximum {
                return packets;
            }
        }
    }

    #[test]
    fn the_identity_is_a_generic_bluetooth_controller_macos_accepts() {
        let mut hci = Hci::new();
        let device = hci.control.descriptors.device;
        assert_eq!(device[4..7], [0xe0, 0x01, 0x01]);
        assert_eq!(device[8..12], [0x12, 0x0a, 0x01, 0x00]);
        let configuration = hci.control.descriptors.configuration.clone();
        assert_eq!(configuration[9 + 4], 3, "interrupt, bulk IN and bulk OUT");
        assert_eq!(configuration[9 + 5..9 + 8], [0xe0, 0x01, 0x01]);
        assert_eq!(hci.model_address(), None);
        transfer(&mut hci, setup(0, SET_ADDRESS, 1, 0, 0), &[]).unwrap();
        assert_eq!(string(&mut hci, 2), "Stela Bluetooth");
    }

    /// The identity BlueTool and bluetoothd take on the USB transport:
    /// the name they match, the product and firmware version BlueTool needs
    /// no update for, and the class triple Linux matches Apple's modules by.
    #[test]
    fn the_modeled_function_is_apples_internal_usb_module() {
        let mut hci = Hci::modeled(ADDRESS);
        let device = hci.control.descriptors.device;
        assert_eq!(device[..4], [18, DEVICE, 0x00, 0x02], "USB 2.0");
        assert_eq!(device[4..7], [0xff, 0x01, 0x01]);
        // idVendor 0x05AC, idProduct 0x828F, bcdDevice 0x0150 (v150).
        assert_eq!(device[8..14], [0xac, 0x05, 0x8f, 0x82, 0x50, 0x01]);
        assert_eq!(device[14..18], [1, 2, 0, 1], "strings, one configuration");
        let (qualifier, _) = hci.control.descriptors.other_speed.clone().unwrap();
        assert_eq!(qualifier[4..7], [0xff, 0x01, 0x01]);
        for configuration in [
            hci.control.descriptors.configuration.clone(),
            hci.control.descriptors.other_speed.clone().unwrap().1,
        ] {
            let mut at = 9;
            while at < configuration.len() {
                let d = &configuration[at..at + usize::from(configuration[at])];
                if d[1] == INTERFACE {
                    assert_eq!(d[5..8], [0xff, 0x01, 0x01], "interface {}", d[2]);
                    assert!(d[2] <= 1, "interfaces 0 and 1 only");
                }
                at += d.len();
            }
        }
        assert_eq!(hci.model_address(), Some(ADDRESS));
        transfer(&mut hci, setup(0, SET_ADDRESS, 1, 0, 0), &[]).unwrap();
        assert_eq!(string(&mut hci, 1), "Apple Inc.");
        assert_eq!(string(&mut hci, 2), "Bluetooth USB Host Controller");
    }

    /// The walk a host stack makes before it takes the controller:
    /// interface 0 with interrupt IN, bulk IN and bulk OUT; interface 1 with
    /// an isochronous IN and OUT pipe in each of alternate settings 0-5.
    #[test]
    fn the_configuration_has_the_interfaces_bluetoothd_requires() {
        for configuration in [GENERIC, APPLE_MODULE].iter().flat_map(|identity| {
            let descriptors = descriptors(identity);
            [
                descriptors.configuration,
                descriptors.other_speed.unwrap().1,
            ]
        }) {
            assert_eq!(configuration[4], 2, "two interfaces");
            assert_eq!(
                usize::from(u16::from_le_bytes([configuration[2], configuration[3]])),
                configuration.len()
            );
            let mut at = 9;
            let mut interface = (0, 0);
            let mut pipes: Vec<((u8, u8), u8, u8, u16)> = Vec::new();
            while at < configuration.len() {
                let d = &configuration[at..at + usize::from(configuration[at])];
                match d[1] {
                    INTERFACE => interface = (d[2], d[3]),
                    ENDPOINT => {
                        pipes.push((interface, d[2], d[3] & 3, u16::from_le_bytes([d[4], d[5]])))
                    }
                    _ => panic!("unexpected descriptor"),
                }
                at += d.len();
            }
            let kinds = |i| -> Vec<(u8, u8)> {
                pipes
                    .iter()
                    .filter(|p| p.0 == i)
                    .map(|p| (p.1, p.2))
                    .collect()
            };
            assert_eq!(kinds((0, 0)), [(EVENTS, 3), (ACL_IN, 2), (ACL_OUT, 2)]);
            for (setting, size) in SCO_PACKETS.into_iter().enumerate() {
                assert_eq!(kinds((1, setting as u8)), [(SCO_IN, 1), (SCO_OUT, 1)]);
                assert!(
                    pipes
                        .iter()
                        .filter(|p| p.0 == (1, setting as u8))
                        .all(|p| p.3 == u16::from(size))
                );
            }
        }
        let mut hci = controller();
        let get = setup(0x81, GET_INTERFACE, 0, SCO_INTERFACE, 1);
        assert_eq!(transfer(&mut hci, get, &[]).unwrap(), [0]);
        transfer(
            &mut hci,
            setup(0x01, SET_INTERFACE, 5, SCO_INTERFACE, 0),
            &[],
        )
        .unwrap();
        assert_eq!(transfer(&mut hci, get, &[]).unwrap(), [5]);
        assert!(
            transfer(
                &mut hci,
                setup(0x01, SET_INTERFACE, 6, SCO_INTERFACE, 0),
                &[]
            )
            .is_err()
        );
    }

    #[test]
    fn commands_reach_the_owner_and_vendor_commands_stop_here() {
        let mut hci = controller();
        // HCI_Reset.
        command(&mut hci, &[0x03, 0x0c, 0]).unwrap();
        assert_eq!(
            hci.take(),
            Some(Outbound::Command(vec![0x03, 0x0c, 0])),
            "a standard command"
        );
        // An opcode in OGF 0x3F, with one parameter.
        command(&mut hci, &[0x01, 0xfc, 1, 9]).unwrap();
        assert_eq!(hci.take(), None, "a vendor command stays here");
        assert_eq!(
            read(&mut hci, EVENTS, EVENT_PACKET),
            [vec![COMMAND_COMPLETE, 4, 1, 0x01, 0xfc, UNKNOWN_COMMAND]]
        );
        assert_eq!(hci.counters().commands, 1);
        assert_eq!(hci.counters().vendor_refused, 1);
        // Its length disagrees with its header.
        assert!(command(&mut hci, &[0x03, 0x0c, 2, 0]).is_err());
        assert_eq!(hci.counters().malformed, 1);
    }

    #[test]
    fn an_event_on_a_full_packet_ends_with_a_zero_length_one() {
        let mut hci = controller();
        assert_eq!(hci.ready_at(EVENTS), None);
        let event = [0x3e; 32];
        hci.deliver_event(&event);
        assert_eq!(hci.ready_at(EVENTS), Some(0));
        let packets = read(&mut hci, EVENTS, EVENT_PACKET);
        assert_eq!(packets.len(), 3);
        assert_eq!(packets[..2].concat(), event);
        assert!(packets[2].is_empty());
        assert_eq!(hci.ready_at(EVENTS), None);
        let mut buffer = [0; EVENT_PACKET];
        assert_eq!(
            hci.packet(EVENTS, Token::In, &mut buffer).unwrap(),
            Reply::Nak
        );
    }

    #[test]
    fn acl_data_moves_both_ways_in_usb_packets() {
        let mut hci = controller();
        let mut outbound = vec![1, 0, 0, 2];
        outbound.resize(ACL_HEADER + 512, 7);
        let (first, rest) = outbound.split_at(512);
        hci.packet(ACL_OUT, Token::Out, &mut first.to_vec())
            .unwrap();
        assert_eq!(hci.take(), None, "the header says more follows");
        hci.packet(ACL_OUT, Token::Out, &mut rest.to_vec()).unwrap();
        assert_eq!(hci.take(), Some(Outbound::Acl(outbound.clone())));
        hci.deliver_acl(&outbound);
        assert_eq!(read(&mut hci, ACL_IN, 512).concat(), outbound);
        // Longer than the transport carries: discarded, not truncated.
        hci.packet(ACL_OUT, Token::Out, &mut [1, 0, 0xff, 0xff])
            .unwrap();
        assert_eq!(hci.take(), None);
        assert_eq!(hci.counters().malformed, 1);
    }

    #[test]
    fn a_full_queue_pushes_back_and_a_reset_empties_them() {
        let mut hci = controller();
        for _ in 0..QUEUE_PACKETS {
            hci.deliver_event(&[0x0e, 0]);
        }
        assert!(!hci.has_room());
        hci.deliver_event(&[0x0e, 0]);
        assert_eq!(hci.counters().overflowed, 1);
        for _ in 0..QUEUE_PACKETS {
            hci.packet(ACL_OUT, Token::Out, &mut [1, 0, 0, 0]).unwrap();
        }
        assert_eq!(
            hci.packet(ACL_OUT, Token::Out, &mut [1, 0, 0, 0]).unwrap(),
            Reply::Nak
        );
        hci.reset().unwrap();
        assert!(hci.has_room());
        assert!(!hci.up());
        assert_eq!(hci.take(), None);
        hci.deliver_event(&[0x0e, 0]);
        assert_eq!(hci.ready_at(EVENTS), None, "unconfigured, it takes nothing");
    }

    #[test]
    fn the_model_answers_through_the_same_pipes() {
        let mut hci = modeled();
        command(&mut hci, &[0x03, 0x0c, 0]).unwrap();
        assert_eq!(hci.take(), None, "nothing reaches a host controller");
        assert_eq!(
            read(&mut hci, EVENTS, EVENT_PACKET),
            [vec![COMMAND_COMPLETE, 4, 1, 0x03, 0x0c, 0]]
        );
        // Read BD_ADDR.
        command(&mut hci, &[0x09, 0x10, 0]).unwrap();
        assert_eq!(
            read(&mut hci, EVENTS, EVENT_PACKET).concat(),
            [COMMAND_COMPLETE, 10, 1, 0x09, 0x10, 0, 1, 2, 3, 4, 5, 0x02]
        );
        // A vendor command the model does not know answers Unknown HCI
        // Command each time, and is reported once.
        command(&mut hci, &[0x4c, 0xfc, 1, 9]).unwrap();
        command(&mut hci, &[0x4c, 0xfc, 1, 9]).unwrap();
        assert_eq!(read(&mut hci, EVENTS, EVENT_PACKET)[0][5], UNKNOWN_COMMAND);
        assert_eq!(read(&mut hci, EVENTS, EVENT_PACKET)[0][5], UNKNOWN_COMMAND);
        assert_eq!(hci.take_unknown(), [0xfc4c]);
        assert_eq!(hci.counters().vendor_refused, 2);
        assert_eq!(hci.counters().unknown, 2);
        // Write BD_ADDR is the module's own, and a bus reset undoes it.
        command(&mut hci, &[0x01, 0xfc, 6, 9, 8, 7, 6, 5, 0x42]).unwrap();
        assert_eq!(
            read(&mut hci, EVENTS, EVENT_PACKET)[0][3..6],
            [0x01, 0xfc, 0]
        );
        assert_eq!(hci.model_address(), Some([9, 8, 7, 6, 5, 0x42]));
        assert_eq!(hci.counters().unknown, 2);
        assert_eq!(hci.counters().vendor_refused, 2);
        hci.reset().unwrap();
        assert_eq!(hci.model_address(), Some(ADDRESS));
        let mut hci = configured(hci);
        // ACL data gets its buffer back.
        hci.packet(ACL_OUT, Token::Out, &mut [1, 0, 1, 0, 7])
            .unwrap();
        assert_eq!(
            read(&mut hci, EVENTS, EVENT_PACKET),
            [vec![0x13, 5, 1, 1, 0, 1, 0]]
        );
        assert_eq!(hci.counters().acl_discarded, 1);
    }

    /// The owner's log: the first commands after each configuration with
    /// their answers, then one end line, and each reset and configuration.
    #[test]
    fn the_trace_names_the_first_commands_after_each_configuration() {
        let mut hci = modeled();
        assert_eq!(hci.take_trace(), [Traced::Configured(1)]);
        command(&mut hci, &[0x03, 0x0c, 0]).unwrap();
        command(&mut hci, &[0x04, 0x10, 1, 2]).unwrap();
        command(&mut hci, &[0x4c, 0xfc, 1, 9]).unwrap();
        command(&mut hci, &[0x01, 0x04, 5, 0x33, 0x8b, 0x9e, 1, 0]).unwrap();
        command(&mut hci, &[0x35, 0x0c, 5, 1, 0x40, 0, 1, 0]).unwrap();
        let complete = |status, returns| {
            Some(Answered {
                event: COMMAND_COMPLETE,
                status,
                returns,
            })
        };
        let traced = hci.take_trace();
        assert_eq!(
            traced,
            [
                Traced::Command {
                    opcode: 0x0c03,
                    parameters: 0,
                    first: [None; 4],
                    answer: complete(0, 0),
                },
                Traced::Command {
                    opcode: 0x1004,
                    parameters: 1,
                    first: [Some(2), None, None, None],
                    answer: complete(0, 10),
                },
                // A vendor command's parameters are hidden.
                Traced::Command {
                    opcode: 0xfc4c,
                    parameters: 1,
                    first: [None; 4],
                    answer: complete(UNKNOWN_COMMAND, 0),
                },
                Traced::Command {
                    opcode: 0x0401,
                    parameters: 5,
                    first: [Some(0x33), Some(0x8b), Some(0x9e), Some(1)],
                    answer: Some(Answered {
                        event: COMMAND_STATUS,
                        status: 0,
                        returns: 0,
                    }),
                },
                Traced::Command {
                    opcode: 0x0c35,
                    parameters: 5,
                    first: [Some(1), Some(0x40), Some(0), Some(1)],
                    answer: None,
                },
            ]
        );
        assert_eq!(
            traced[0].to_string(),
            "command 0x0c03 parameters=0 answered complete status=0x00 returns=0"
        );
        assert_eq!(
            traced[1].to_string(),
            "command 0x1004 parameters=1 [02] answered complete status=0x00 returns=10"
        );
        assert_eq!(
            traced[2].to_string(),
            "command 0xfc4c parameters=1 [xx] answered complete status=0x01 returns=0"
        );
        assert_eq!(
            traced[4].to_string(),
            "command 0x0c35 parameters=5 [01 40 00 01] answered with no event"
        );
        // The trace ends after its commands, once.
        let read_address = |hci: &mut Hci, count: u16| {
            for _ in 0..count {
                command(hci, &[0x09, 0x10, 0]).unwrap();
                read(hci, EVENTS, EVENT_PACKET);
            }
        };
        read_address(&mut hci, TRACED_COMMANDS - 1);
        let traced = hci.take_trace();
        assert_eq!(traced.len(), usize::from(TRACED_COMMANDS) - 5 + 1);
        assert_eq!(traced.last(), Some(&Traced::Ended));
        // An HCI Reset starts it again, a bounded number of times: the
        // first command above was one.
        for _ in 1..TRACED_BRING_UPS {
            command(&mut hci, &[0x03, 0x0c, 0]).unwrap();
            read(&mut hci, EVENTS, EVENT_PACKET);
            read_address(&mut hci, TRACED_COMMANDS - 1);
            assert_eq!(hci.take_trace().len(), usize::from(TRACED_COMMANDS) + 1);
        }
        command(&mut hci, &[0x03, 0x0c, 0]).unwrap();
        assert!(hci.take_trace().is_empty());
        // A bus reset and a new configuration start it again.
        hci.reset().unwrap();
        assert_eq!(hci.take_trace(), [Traced::Reset, Traced::Configured(0)]);
        let mut hci = configured(hci);
        command(&mut hci, &[0x03, 0x0c, 0]).unwrap();
        let traced = hci.take_trace();
        assert_eq!(traced[0], Traced::Configured(1));
        assert_eq!(traced.len(), 2);
        assert_eq!(Traced::Reset.to_string(), "bus reset");
    }

    /// The trace is in every boot's log, so its bytes are the capture's
    /// masked ones: the Mac's name, the middle of the address bluetoothd
    /// writes and of a neighbour's, and LE Encrypt's key show as `xx`, for
    /// the model and for a lent controller alike.
    #[test]
    fn the_always_on_trace_shows_no_name_address_middle_or_key() {
        // A synthetic name and addresses, in HCI order.
        const NAME: &[u8] = b"Tester's Mac";
        const PLATFORM: [u8; 6] = [0xa0, 0xa1, 0xa2, 0xa3, 0xa4, 0xa5];
        const NEIGHBOUR: [u8; 6] = [0xb0, 0xb1, 0xb2, 0xb3, 0xb4, 0xb5];
        let with = |opcode: u16, parameters: &[u8]| {
            let mut packet = opcode.to_le_bytes().to_vec();
            packet.push(parameters.len() as u8);
            packet.extend_from_slice(parameters);
            packet
        };
        let mut name = NAME.to_vec();
        name.resize(248, 0);
        let mut request = NEIGHBOUR.to_vec();
        request.extend_from_slice(&[1, 0, 0, 0]);
        let mut eir = vec![0, NAME.len() as u8 + 1, 0x09];
        eir.extend_from_slice(NAME);
        eir.resize(241, 0);
        let commands = [
            (with(0x0c13, &name), "parameters=248 [xx xx xx xx] "),
            (with(0xfc01, &PLATFORM), "parameters=6 [a0 xx xx a3] "),
            (with(0x0419, &request), "parameters=10 [b0 xx xx b3] "),
            (with(0x2017, &[0x5a; 32]), "parameters=32 [xx xx xx xx] "),
            (with(0x0c52, &eir), "parameters=241 [00 0d 09 xx] "),
        ];
        for mut hci in [modeled(), bridged()] {
            hci.take_trace();
            for (packet, shown) in &commands {
                command(&mut hci, packet).unwrap();
                // A lent controller answers what reaches it.
                while let Some(Outbound::Command(sent)) = hci.take() {
                    hci.deliver_event(&[COMMAND_STATUS, 4, 0, 1, sent[0], sent[1]]);
                }
                while hci.ready_at(EVENTS) == Some(0) {
                    read(&mut hci, EVENTS, EVENT_PACKET);
                }
                let traced = hci.take_trace();
                let [Traced::Command { first, .. }] = traced[..] else {
                    panic!("one traced command: {traced:?}");
                };
                let line = traced[0].to_string();
                assert!(line.contains(shown), "{line}");
                for secret in [&NAME[..2], &PLATFORM[1..3], &NEIGHBOUR[1..3], &[0x5a, 0x5a]] {
                    let hex = alloc::format!("{:02x} {:02x}", secret[0], secret[1]);
                    assert!(!line.contains(&hex), "{line}");
                    assert!(
                        !first
                            .windows(2)
                            .any(|w| w == [Some(secret[0]), Some(secret[1])])
                    );
                }
            }
        }
    }

    /// `--bluetooth-trace`: once the owner asks, every packet after each
    /// configuration, both ways, then one end line until the next
    /// configuration; nothing before it asks.
    #[test]
    fn the_capture_holds_every_packet_after_each_configuration() {
        use capture::{CAPTURED_PACKETS, Captured, Direction, Kind};
        let mut hci = modeled();
        command(&mut hci, &[0x03, 0x0c, 0]).unwrap();
        read(&mut hci, EVENTS, EVENT_PACKET);
        assert!(hci.take_captured().is_empty(), "nothing until asked");
        hci.capture();
        command(&mut hci, &[0x09, 0x10, 0]).unwrap();
        let answer = read(&mut hci, EVENTS, EVENT_PACKET).concat();
        assert_eq!(
            hci.take_captured(),
            [
                Captured::new(Direction::FromGuest, Kind::Command, &[0x09, 0x10, 0]),
                Captured::new(Direction::ToGuest, Kind::Event, &answer),
            ]
        );
        // ACL data from the guest, and the model's return of its buffer.
        hci.packet(ACL_OUT, Token::Out, &mut [1, 0, 1, 0, 7])
            .unwrap();
        let completed = read(&mut hci, EVENTS, EVENT_PACKET).concat();
        assert_eq!(
            hci.take_captured(),
            [
                Captured::new(Direction::FromGuest, Kind::Acl, &[1, 0, 1, 0, 7]),
                Captured::new(Direction::ToGuest, Kind::Event, &completed),
            ]
        );
        hci.take_trace();
        // The bound counts every packet of the configuration.
        let left = usize::from(CAPTURED_PACKETS) - 4;
        for _ in 0..left / 2 {
            command(&mut hci, &[0x09, 0x10, 0]).unwrap();
            read(&mut hci, EVENTS, EVENT_PACKET);
        }
        assert_eq!(hci.take_captured().len(), left);
        assert_eq!(hci.take_trace().last(), Some(&Traced::CaptureEnded));
        command(&mut hci, &[0x09, 0x10, 0]).unwrap();
        read(&mut hci, EVENTS, EVENT_PACKET);
        assert!(hci.take_captured().is_empty());
        assert_eq!(
            Traced::CaptureEnded.to_string(),
            "capture ends after 1024 packets until the next configuration"
        );
        // A new configuration starts it again; a bus reset stops it.
        transfer(&mut hci, setup(0, SET_CONFIGURATION, 1, 0, 0), &[]).unwrap();
        command(&mut hci, &[0x09, 0x10, 0]).unwrap();
        read(&mut hci, EVENTS, EVENT_PACKET);
        assert_eq!(hci.take_captured().len(), 2);
        hci.reset().unwrap();
        let mut hci = configured(hci);
        command(&mut hci, &[0x09, 0x10, 0]).unwrap();
        assert_eq!(hci.take_captured().len(), 2, "still asked for");
        read(&mut hci, EVENTS, EVENT_PACKET);
        // That was the third configuration captured; the fourth is the
        // boot's last, and the fifth says so.
        hci.take_trace();
        for _ in 0..3 {
            transfer(&mut hci, setup(0, SET_CONFIGURATION, 1, 0, 0), &[]).unwrap();
        }
        let traced = hci.take_trace();
        assert_eq!(
            traced.iter().filter(|&&t| t == Traced::CaptureOver).count(),
            1,
            "{traced:?}"
        );
        command(&mut hci, &[0x09, 0x10, 0]).unwrap();
        assert!(hci.take_captured().is_empty());
        assert_eq!(
            Traced::CaptureOver.to_string(),
            "capture ends for this boot: at most 1024 packets of each of its first 4 \
             configurations"
        );
    }

    /// A packet longer than the capture shows keeps its first bytes and
    /// counts the rest; a host controller's packets are captured as they
    /// reach the guest.
    #[test]
    fn a_captured_packet_keeps_its_first_bytes_and_counts_the_rest() {
        use capture::{CAPTURED_BYTES, Captured, Direction, Kind};
        let mut hci = controller();
        hci.capture();
        let mut long = vec![1, 0, 0, 2];
        long.resize(ACL_HEADER + 512, 7);
        let (first, rest) = long.split_at(512);
        hci.packet(ACL_OUT, Token::Out, &mut first.to_vec())
            .unwrap();
        hci.packet(ACL_OUT, Token::Out, &mut rest.to_vec()).unwrap();
        hci.deliver_acl(&long);
        hci.deliver_event(&[0x0e, 4, 1, 0x03, 0x0c, 0]);
        let captured = hci.take_captured();
        assert_eq!(captured.len(), 3);
        for (packet, direction) in captured[..2]
            .iter()
            .zip([Direction::FromGuest, Direction::ToGuest])
        {
            assert_eq!((packet.direction, packet.kind), (direction, Kind::Acl));
            assert_eq!(packet.bytes.len(), CAPTURED_BYTES);
            assert_eq!(packet.cut, long.len() - CAPTURED_BYTES);
            // The HCI and L2CAP headers, then the payload hidden.
            assert_eq!(packet.bytes[..8], long[..8]);
            assert!(packet.hidden[8..].iter().all(|&h| h));
        }
        assert_eq!(
            captured[2],
            Captured::new(
                Direction::ToGuest,
                Kind::Event,
                &[0x0e, 4, 1, 0x03, 0x0c, 0]
            )
        );
    }

    fn bridged() -> Hci {
        configured(Hci::bridged(ADDRESS))
    }

    /// A host controller lent behind the module: the guest sees Apple's
    /// module; its listed standard commands go to the owner for the host's
    /// controller and their answers come back with one credit; vendor
    /// commands and ACL data stay with the shadow.
    #[test]
    fn the_bridged_function_is_apples_module_with_the_hosts_controller_behind_it() {
        use capture::Direction::{FromChip, FromGuest, ToChip, ToGuest};
        let hci = Hci::bridged(ADDRESS);
        let device = hci.control.descriptors.device;
        assert_eq!(
            device[4..14],
            [0xff, 1, 1, 64, 0xac, 0x05, 0x8f, 0x82, 0x50, 0x01]
        );
        assert_eq!(hci.model_address(), Some(ADDRESS));
        let mut hci = configured(hci);
        hci.capture();
        command(&mut hci, &[0x03, 0x0c, 0]).unwrap();
        assert_eq!(hci.take(), Some(Outbound::Command(vec![0x03, 0x0c, 0])));
        let mut buffer = [0; EVENT_PACKET];
        assert_eq!(
            hci.packet(EVENTS, Token::In, &mut buffer).unwrap(),
            Reply::Nak,
            "no answer before the controller's"
        );
        hci.deliver_event(&[COMMAND_COMPLETE, 3, 1, 0, 0]);
        hci.deliver_event(&[COMMAND_COMPLETE, 4, 5, 0x03, 0x0c, 0]);
        assert_eq!(
            read(&mut hci, EVENTS, EVENT_PACKET),
            [vec![COMMAND_COMPLETE, 4, 1, 0x03, 0x0c, 0]]
        );
        // A vendor command stays here.
        command(&mut hci, &[0x4c, 0xfc, 1, 9]).unwrap();
        assert_eq!(hci.take(), None);
        assert_eq!(read(&mut hci, EVENTS, EVENT_PACKET)[0][5], UNKNOWN_COMMAND);
        assert_eq!(hci.take_unknown(), [0xfc4c]);
        assert_eq!(hci.counters().commands, 2);
        assert_eq!(hci.counters().vendor_refused, 1);
        let directions: Vec<_> = hci.take_captured().iter().map(|c| c.direction).collect();
        assert_eq!(
            directions,
            [
                FromGuest, ToChip, FromChip, FromChip, ToGuest, FromGuest, ToGuest
            ]
        );
        // ACL data from the guest names none of the controller's
        // connections: the shadow returns its buffer.
        hci.packet(ACL_OUT, Token::Out, &mut [1, 0, 1, 0, 7])
            .unwrap();
        assert_eq!(hci.take(), None);
        assert_eq!(
            read(&mut hci, EVENTS, EVENT_PACKET),
            [vec![0x13, 5, 1, 1, 0, 1, 0]]
        );
        // The controller's ACL data names no connection the guest knows:
        // it is captured and dropped.
        let dropped = hci.bridge_counters().unwrap().dropped;
        hci.deliver_acl(&[2, 0, 1, 0, 8]);
        assert_eq!(hci.ready_at(ACL_IN), None);
        assert_eq!(hci.counters().acl_to_guest, 0);
        assert_eq!(hci.bridge_counters().unwrap().dropped, dropped + 1);
        let captured = hci.take_captured();
        assert_eq!(captured.last().map(|c| c.direction), Some(FromChip));
        assert_eq!(hci.fallback(), None);
        // The trace names each command with its answer, whoever gave it.
        let traced = hci.take_trace();
        assert_eq!(traced[0], Traced::Configured(1));
        assert_eq!(
            traced[1].to_string(),
            "command 0x0c03 parameters=0 answered complete status=0x00 returns=0"
        );
        assert_eq!(
            traced[2].to_string(),
            "command 0xfc4c parameters=1 [xx] answered complete status=0x01 returns=0"
        );
    }

    /// The fallback: a forwarded command not answered by its deadline, or a
    /// failed host controller, gets the model's answer, and the model
    /// answers from then on.
    #[test]
    fn a_bridged_command_left_unanswered_gets_the_models_answer() {
        let mut hci = bridged();
        hci.set_time(100);
        command(&mut hci, &[0x09, 0x10, 0]).unwrap();
        assert!(hci.take().is_some());
        assert_eq!(hci.ready_at(EVENTS), Some(100 + bridge::DEADLINE));
        hci.set_time(100 + bridge::DEADLINE);
        assert_eq!(hci.ready_at(EVENTS), Some(0));
        assert_eq!(
            read(&mut hci, EVENTS, EVENT_PACKET).concat(),
            [COMMAND_COMPLETE, 10, 1, 0x09, 0x10, 0, 1, 2, 3, 4, 5, 0x02]
        );
        let fallback = hci.fallback().expect("fell back");
        assert_eq!(fallback.to_string(), "0x1009 no answer in 1000 ms");
        command(&mut hci, &[0x03, 0x0c, 0]).unwrap();
        assert_eq!(hci.take(), None, "nothing goes out again");
        assert_eq!(
            read(&mut hci, EVENTS, EVENT_PACKET)[0][3..6],
            [0x03, 0x0c, 0]
        );
        // A host controller that fails.
        let mut hci = bridged();
        command(&mut hci, &[0x03, 0x0c, 0]).unwrap();
        hci.take();
        hci.host_failed();
        assert_eq!(
            read(&mut hci, EVENTS, EVENT_PACKET)[0][3..6],
            [0x03, 0x0c, 0]
        );
        assert_eq!(
            hci.fallback().map(|f| f.cause),
            Some(bridge::Cause::HostFailed)
        );
    }

    /// A bus reset drops the command in flight; the controller's events stay
    /// out until the guest's next HCI Reset is answered.
    #[test]
    fn a_bus_reset_of_a_bridged_function_keeps_the_controller_out_until_its_reset() {
        let result = [&[0x2f, 255, 1][..], &[7; 254]].concat();
        let mut hci = bridged();
        command(&mut hci, &[0x14, 0x0c, 0]).unwrap();
        hci.take();
        hci.reset().unwrap();
        let mut hci = configured(hci);
        hci.deliver_event(&[&[COMMAND_COMPLETE, 252, 1, 0x14, 0x0c, 0][..], &[0; 248]].concat());
        hci.deliver_event(&result);
        assert_eq!(hci.ready_at(EVENTS), None);
        command(&mut hci, &[0x03, 0x0c, 0]).unwrap();
        hci.take();
        hci.deliver_event(&[COMMAND_COMPLETE, 4, 1, 0x03, 0x0c, 0]);
        read(&mut hci, EVENTS, EVENT_PACKET);
        hci.deliver_event(&result);
        assert_eq!(read(&mut hci, EVENTS, EVENT_PACKET).concat(), result);
        // The shadow powers on again with a bus reset, as the model does.
        command(&mut hci, &[0x01, 0xfc, 6, 9, 8, 7, 6, 5, 0x42]).unwrap();
        read(&mut hci, EVENTS, EVENT_PACKET);
        assert_eq!(hci.model_address(), Some([9, 8, 7, 6, 5, 0x42]));
        hci.reset().unwrap();
        assert_eq!(hci.model_address(), Some(ADDRESS));
        // A new configuration alone drops the command in flight too.
        let mut hci = configured(hci);
        command(&mut hci, &[0x14, 0x0c, 0]).unwrap();
        hci.take();
        transfer(&mut hci, setup(0, SET_CONFIGURATION, 1, 0, 0), &[]).unwrap();
        assert_eq!(hci.ready_at(EVENTS), None, "no deadline left");
        hci.deliver_event(&[&[COMMAND_COMPLETE, 252, 1, 0x14, 0x0c, 0][..], &[0; 248]].concat());
        assert_eq!(hci.ready_at(EVENTS), None, "the stale answer is dropped");
        assert_eq!(hci.fallback(), None);
    }

    /// A guest that stalls with its event queue full of the controller's
    /// inquiry results still gets the fallback's answer to the command in
    /// flight, after them; what the controller sends then is dropped.
    #[test]
    fn a_full_event_queue_still_takes_the_fallbacks_answer() {
        let mut hci = bridged();
        hci.set_time(0);
        command(&mut hci, &[0x03, 0x0c, 0]).unwrap();
        hci.take();
        hci.deliver_event(&[COMMAND_COMPLETE, 4, 1, 0x03, 0x0c, 0]);
        read(&mut hci, EVENTS, EVENT_PACKET);
        let result = [0x02, 15, 1, 1, 2, 3, 4, 5, 6, 1, 0, 0, 0, 0, 0, 0, 0];
        for _ in 0..QUEUE_PACKETS {
            hci.deliver_event(&result);
        }
        assert_eq!(hci.room_left(), 0);
        command(&mut hci, &[0x09, 0x10, 0]).unwrap();
        assert!(hci.take().is_some());
        hci.set_time(bridge::DEADLINE);
        assert!(hci.fallback().is_some());
        hci.deliver_event(&result);
        assert_eq!(hci.counters().overflowed, 0, "dropped by the fallback");
        for _ in 0..QUEUE_PACKETS {
            assert_eq!(read(&mut hci, EVENTS, EVENT_PACKET).concat(), result);
        }
        assert_eq!(
            read(&mut hci, EVENTS, EVENT_PACKET).concat(),
            [COMMAND_COMPLETE, 10, 1, 0x09, 0x10, 0, 1, 2, 3, 4, 5, 0x02]
        );
        assert_eq!(hci.ready_at(EVENTS), None);
        // Bounded all the same: a guest that commands on and never reads.
        for _ in 0..2 * AWAITED_PACKETS {
            command(&mut hci, &[0x09, 0x10, 0]).unwrap();
        }
        assert_eq!(hci.state.events.queue.len(), AWAITED_PACKETS);
    }

    /// A Hardware Error while the guest re-enumerates the function falls
    /// back at once, so the next command is the model's and waits for no
    /// deadline; the controller's other events then are dropped.
    #[test]
    fn a_hardware_error_while_unconfigured_falls_back() {
        let mut hci = bridged();
        hci.reset().unwrap();
        hci.deliver_event(&[0x13, 5, 1, 1, 0, 1, 0]);
        assert_eq!(hci.fallback(), None);
        assert_eq!(hci.bridge_counters().unwrap().dropped, 1);
        hci.deliver_event(&[0x10, 1, 0x2a]);
        assert_eq!(
            hci.fallback().map(|f| (f.opcode, f.cause)),
            Some((None, bridge::Cause::HardwareError(0x2a)))
        );
        let mut hci = configured(hci);
        assert_eq!(hci.ready_at(EVENTS), None, "the queues start empty");
        command(&mut hci, &[0x03, 0x0c, 0]).unwrap();
        assert_eq!(hci.take(), None, "nothing goes to the controller");
        assert_eq!(
            read(&mut hci, EVENTS, EVENT_PACKET),
            [vec![COMMAND_COMPLETE, 4, 1, 0x03, 0x0c, 0]]
        );
    }

    /// A command held for the controller's credit is traced with the answer
    /// it gets once the credit comes back, not as answered with no event.
    #[test]
    fn a_command_held_for_a_credit_is_traced_with_its_later_answer() {
        let mut hci = bridged();
        command(&mut hci, &[0x03, 0x0c, 0]).unwrap();
        hci.take();
        // Answered with no credit left.
        hci.deliver_event(&[COMMAND_COMPLETE, 4, 0, 0x03, 0x0c, 0]);
        read(&mut hci, EVENTS, EVENT_PACKET);
        hci.take_trace();
        // Set Event Mask waits for a credit: nothing is traced yet.
        let mask = [
            0x01, 0x0c, 8, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        ];
        command(&mut hci, &mask).unwrap();
        assert_eq!(hci.take(), None, "held for a credit");
        assert!(hci.take_trace().is_empty(), "not answered yet");
        // Credits alone send it; its answer is the one traced.
        hci.deliver_event(&[COMMAND_COMPLETE, 3, 1, 0, 0]);
        assert_eq!(hci.take(), Some(Outbound::Command(mask.to_vec())));
        hci.deliver_event(&[COMMAND_COMPLETE, 4, 1, 0x01, 0x0c, 0]);
        let traced = hci.take_trace();
        assert_eq!(traced.len(), 1, "{traced:?}");
        assert_eq!(
            traced[0].to_string(),
            "command 0x0c01 parameters=8 [ff ff ff ff] answered complete status=0x00 returns=0"
        );
        // A command no event answers is still traced so.
        command(&mut hci, &[0x35, 0x0c, 5, 1, 0x40, 0, 1, 0]).unwrap();
        assert_eq!(
            hci.take_trace()[0].to_string(),
            "command 0x0c35 parameters=5 [01 40 00 01] answered with no event"
        );
    }

    /// After the fallback nothing goes to the host's controller, not even a
    /// command queued for it before.
    #[test]
    fn nothing_goes_to_the_controller_after_the_fallback() {
        let mut hci = bridged();
        hci.set_time(0);
        command(&mut hci, &[0x09, 0x10, 0]).unwrap();
        hci.set_time(bridge::DEADLINE);
        assert!(hci.fallback().is_some());
        assert_eq!(hci.take(), None, "the command queued before is dropped");
        command(&mut hci, &[0x03, 0x0c, 0]).unwrap();
        assert_eq!(hci.take(), None);
    }

    #[test]
    fn the_room_left_is_the_fuller_queues() {
        let mut hci = controller();
        assert_eq!(hci.room_left(), QUEUE_PACKETS);
        for _ in 0..60 {
            hci.deliver_event(&[0x13, 0]);
        }
        hci.deliver_acl(&[1, 0, 0, 0]);
        assert_eq!(hci.room_left(), QUEUE_PACKETS - 60);
        for _ in 0..10 {
            hci.deliver_event(&[0x13, 0]);
        }
        assert_eq!(hci.room_left(), 0);
    }

    #[test]
    fn a_procedure_ends_when_the_system_count_reaches_it() {
        let mut hci = modeled();
        hci.set_time(100);
        // Inquiry for 1.28 s.
        command(&mut hci, &[0x01, 0x04, 5, 0x33, 0x8b, 0x9e, 1, 0]).unwrap();
        assert_eq!(read(&mut hci, EVENTS, EVENT_PACKET)[0][0], 0x0f);
        let end = 100 + crate::counter::FREQUENCY_HZ * 128 / 100;
        assert_eq!(hci.ready_at(EVENTS), Some(end));
        hci.set_time(end - 1);
        assert_eq!(hci.ready_at(EVENTS), Some(end));
        hci.set_time(end);
        assert_eq!(hci.ready_at(EVENTS), Some(0));
        assert_eq!(read(&mut hci, EVENTS, EVENT_PACKET), [vec![0x01, 1, 0]]);
        assert_eq!(hci.ready_at(EVENTS), None);
    }

    #[test]
    fn a_halted_endpoint_stalls_until_cleared() {
        let mut hci = controller();
        let halt = setup(0x02, SET_FEATURE, ENDPOINT_HALT, u16::from(ACL_IN), 0);
        transfer(&mut hci, halt, &[]).unwrap();
        let mut buffer = [0; 512];
        assert_eq!(
            hci.packet(ACL_IN, Token::In, &mut buffer).unwrap(),
            Reply::Stall
        );
        let clear = setup(0x02, CLEAR_FEATURE, ENDPOINT_HALT, u16::from(ACL_IN), 0);
        transfer(&mut hci, clear, &[]).unwrap();
        assert_eq!(
            hci.packet(ACL_IN, Token::In, &mut buffer).unwrap(),
            Reply::Nak
        );
    }
}
