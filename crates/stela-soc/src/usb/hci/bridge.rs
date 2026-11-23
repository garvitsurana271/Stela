//! A host controller lent to the guest behind Apple's module
//! (`--bluetooth-lend`): the function keeps the module's identity, a shadow
//! of the controller model ([`Model`]) answers every vendor command and
//! every command the milestone keeps local, and only the standard commands
//! on [`policy`]'s list go to the host's controller, whose answers the guest
//! gets.
//!
//! The bridge decides, packet by packet, and does no I/O: [`Bridge::command`]
//! and [`Bridge::chip_event`] return [`Step`]s, an event for the guest or a
//! command for the host's controller, which the function queues for its
//! owner to carry.
//!
//! - The shadow executes every command, so it always holds the state the
//!   guest set. Its own later events (an inquiry or page that ends) are
//!   discarded while the bridge forwards.
//! - One forwarded command is in flight. The guest always sees one command
//!   credit, as from the model; later commands wait their turn.
//! - Every forwarded command is answered by [`DEADLINE`]. If the host's
//!   controller has not answered by then, fails, or answers with an error the
//!   model would not give or of another kind or length than the model's, the
//!   bridge falls back: the guest gets the shadow's answer to that command
//!   and the shadow answers everything after it, for the rest of the boot.
//!   Nothing goes to the host's controller again. So bluetoothd's bring-up,
//!   and Setup's first screen, which waits for it, never wait on a stalled
//!   controller.
//! - The controller's events reach the guest as they are, but for its
//!   vendor events, credits alone and answers to commands no longer in
//!   flight, which are dropped, and a Hardware Error, which falls back. Its
//!   ACL data is dropped: milestone 1 lends no connection. After a guest
//!   bus reset or a new configuration, which reach the shadow but never the
//!   controller (a USB reset would lose its patched firmware), its events
//!   stay out until the guest's next HCI Reset is answered.
//! - The owner carries packets to and from the controller through a
//!   [`Link`] and the [`HostController`] trait.
use super::model::Model;
use crate::counter::FREQUENCY_HZ;
use alloc::{collections::VecDeque, vec, vec::Vec};
use core::fmt;

pub mod link;
pub mod policy;

pub use link::{HostController, Link};
pub use policy::ChipCommand;

/// System counts the host's controller has to answer a forwarded command:
/// one second.
pub const DEADLINE: u64 = FREQUENCY_HZ;

/// Commands that wait behind the one in flight; more stall the guest's
/// control transfer until one is answered.
pub const BACKLOG: usize = 64;

// Events (Bluetooth Core v5.4 Vol 4 Part E 7.7).
const COMMAND_COMPLETE: u8 = 0x0e;
const COMMAND_STATUS: u8 = 0x0f;

/// What the bridge does with a packet: an event for the guest, or a command
/// for the host's controller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Guest(Vec<u8>),
    Chip(ChipCommand),
}

/// Why the bridge fell back to the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cause {
    /// No answer by [`DEADLINE`].
    Timeout,
    /// This error status where the model answers success.
    Refused(u8),
    /// An answer of another kind or length than the model's.
    Malformed,
    /// The host's controller failed.
    HostFailed,
    /// The host's controller reported a Hardware Error with this code
    /// (Vol 4 Part E 7.7.16): its firmware has failed, and whatever it was
    /// running is lost.
    HardwareError(u8),
    /// bluetoothd reset the controller [`RESTARTS`] times in a row, each
    /// within [`SETTLED`] of the start before it: it restarts so when it
    /// rejects the bring-up's answers, and Setup's first screen waits for
    /// it.
    Restarted,
}

/// HCI Resets in a row after an answered one, each within [`SETTLED`] of
/// the last answered one, at which the bridge falls back: one restart
/// passes, the second is the model's.
pub const RESTARTS: u64 = 2;

/// System counts after the controller answered an HCI Reset past which the
/// guest's next HCI Reset starts the count of [`RESTARTS`] again: one
/// minute.
///
/// A bluetoothd that rejects its bring-up exits, and launchd starts it again
/// no sooner than its throttle interval (`ThrottleInterval`, 10 s by
/// default, `launchd.plist(5)`); the bring-up it then runs takes at most a
/// [`DEADLINE`] a command. So a restart loop resets the controller well
/// within a minute of the start before. Bluetooth turned off and on in
/// System Settings, or a sleep and wake, each sends one HCI Reset after a
/// session that ran longer, and is a start of its own: without this, the
/// boot's second of them fell back. The bias is toward the loop: a loop
/// missed leaves Setup waiting on bluetoothd, a session counted as a
/// restart only gives the guest the model.
pub const SETTLED: u64 = 60 * FREQUENCY_HZ;

/// [`SETTLED`] in seconds, as the log says it.
pub const SETTLED_S: u64 = SETTLED / FREQUENCY_HZ;

/// [`DEADLINE`] in milliseconds, as the log says it.
pub const DEADLINE_MS: u64 = DEADLINE * 1000 / FREQUENCY_HZ;

impl fmt::Display for Cause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout => write!(f, "no answer in {DEADLINE_MS} ms"),
            Self::Refused(status) => write!(f, "status {status:#04x} where the model succeeds"),
            Self::Malformed => f.write_str("an answer of another kind or length than the model's"),
            Self::HostFailed => f.write_str("the host controller failed"),
            Self::HardwareError(code) => {
                write!(f, "the host controller reported Hardware Error {code:#04x}")
            }
            Self::Restarted => write!(
                f,
                "bluetoothd reset the controller {RESTARTS} times in a row, each within \
                 {SETTLED_S} s of its start, as it does when it rejects the bring-up"
            ),
        }
    }
}

/// The command the bridge fell back at, if one was in flight, and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fallback {
    pub opcode: Option<u16>,
    pub cause: Cause,
}

impl fmt::Display for Fallback {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.opcode {
            Some(opcode) => write!(f, "{opcode:#06x} {}", self.cause),
            None => write!(f, "between commands: {}", self.cause),
        }
    }
}

/// Counters for the owner's log.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counters {
    /// Commands sent to the host's controller.
    pub forwarded: u64,
    /// Commands the shadow answered while the bridge forwarded.
    pub local: u64,
    /// Events of the host's controller the guest got.
    pub passed: u64,
    /// Packets of the host's controller dropped: vendor events, credits
    /// alone, stale answers, what comes while it is kept out after a guest
    /// reset, and everything after the fallback.
    pub dropped: u64,
    /// HCI Resets of the guest after the first the controller answered.
    pub restarts: u64,
    /// Those of them in a row, each within [`SETTLED`] of the controller's
    /// last answered HCI Reset: at [`RESTARTS`] the bridge falls back.
    pub streak: u64,
}

/// The forwarded command in flight.
#[derive(Debug, Clone)]
struct Pending {
    opcode: u16,
    parameters: Vec<u8>,
    kind: policy::Forward,
    /// The shadow's answer, the first of its events, given on a fallback.
    shadow: Vec<Vec<u8>>,
    deadline: u64,
    /// The command, while it waits for one of the controller's credits.
    unsent: Option<ChipCommand>,
}

/// The bridge.
#[derive(Debug, Clone)]
pub struct Bridge {
    shadow: Model,
    fallen: Option<Fallback>,
    pending: Option<Pending>,
    backlog: VecDeque<Vec<u8>>,
    /// Whether the controller is kept out until the guest's next HCI Reset
    /// is answered: the guest reset or configured the function again, and
    /// the controller, which no USB reset reaches, may still send what the
    /// old session asked for.
    resync: bool,
    /// The BD_ADDR the guest wrote with the module's vendor command.
    written: Option<[u8; 6]>,
    /// Whether the controller reported another address than that.
    address_differs: bool,
    /// The controller's procedures the guest saw start and not yet end.
    running: Vec<Running>,
    /// Whether the controller answered an HCI Reset: the start-up ran.
    started: bool,
    /// The system count at which the controller last answered an HCI Reset.
    last_start: u64,
    /// The commands the controller takes now, as its last Command Complete
    /// or Command Status said (Num_HCI_Command_Packets, Vol 4 Part E 4.4).
    credits: u8,
    counters: Counters,
}

// Commands and events the shadow follows (Bluetooth Core v5.4 Vol 4 Part E
// 7.1.1-7.1.2, 7.1.19-7.1.20, 7.3.2, 7.4.6, 7.7.1, 7.7.7).
const INQUIRY: u16 = 0x0401;
const INQUIRY_CANCEL: u16 = 0x0402;
const REMOTE_NAME_REQUEST: u16 = 0x0419;
const REMOTE_NAME_REQUEST_CANCEL: u16 = 0x041a;
const RESET: u16 = 0x0c03;
const READ_BD_ADDR: u16 = 0x1009;
/// The module's Write BD_ADDR (Linux `btbcm_set_bdaddr`).
const WRITE_BD_ADDR: u16 = 0xfc01;
const INQUIRY_COMPLETE: u8 = 0x01;
const REMOTE_NAME_REQUEST_COMPLETE: u8 = 0x07;
/// Page Timeout (Vol 1 Part F 1.3).
const PAGE_TIMEOUT: u8 = 0x04;

/// A procedure of the controller the guest saw start and not yet end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Running {
    Inquiry,
    /// A remote name request to this address.
    Name([u8; 6]),
}
/// A vendor's own event (7.7.65 leaves code 0xFF to vendors).
const VENDOR_EVENT: u8 = 0xff;
/// Hardware Error (7.7.16): the controller's firmware failed.
const HARDWARE_ERROR: u8 = 0x10;

/// The parts of a Command Complete or Command Status event that answers a
/// command, with its status.
pub(super) struct Completion<'a> {
    pub code: u8,
    pub opcode: u16,
    pub status: u8,
    pub returns: &'a [u8],
}

/// Whether `event` is a Command Complete or Command Status, which answers a
/// command or only gives credits.
fn completes(event: &[u8]) -> bool {
    matches!(event.first(), Some(&(COMMAND_COMPLETE | COMMAND_STATUS)))
}

impl<'a> Completion<'a> {
    pub(super) fn of(event: &'a [u8]) -> Option<Self> {
        if event.len() < 2 || usize::from(event[1]) + 2 != event.len() {
            return None;
        }
        match *event {
            [COMMAND_COMPLETE, _, _, low, high, status, ref returns @ ..] => Some(Self {
                code: COMMAND_COMPLETE,
                opcode: u16::from_le_bytes([low, high]),
                status,
                returns,
            }),
            [COMMAND_STATUS, 4, status, _, low, high] => Some(Self {
                code: COMMAND_STATUS,
                opcode: u16::from_le_bytes([low, high]),
                status,
                returns: &[],
            }),
            _ => None,
        }
    }

    /// The event, with the one command credit the guest always sees.
    fn event(&self, returns: &[u8]) -> Vec<u8> {
        let [low, high] = self.opcode.to_le_bytes();
        if self.code == COMMAND_STATUS {
            return vec![COMMAND_STATUS, 4, self.status, 1, low, high];
        }
        let mut event = vec![COMMAND_COMPLETE, (4 + returns.len()) as u8, 1, low, high];
        event.push(self.status);
        event.extend_from_slice(returns);
        event
    }
}

/// The opcode of a command or of the completion event that answers one.
fn opcode(packet: &[u8]) -> u16 {
    u16::from_le_bytes([packet[0], packet[1]])
}

fn guest(events: Vec<Vec<u8>>) -> Vec<Step> {
    events.into_iter().map(Step::Guest).collect()
}

impl Bridge {
    /// A bridge whose shadow's power-on BD_ADDR is `address`.
    pub fn new(address: [u8; 6]) -> Self {
        Self {
            shadow: Model::new(address),
            fallen: None,
            pending: None,
            backlog: VecDeque::new(),
            resync: false,
            written: None,
            address_differs: false,
            running: Vec::new(),
            started: false,
            last_start: 0,
            credits: 1,
            counters: Counters::default(),
        }
    }

    /// Whether a forwarded command is in flight, so another waits.
    pub fn busy(&self) -> bool {
        self.pending.is_some()
    }

    /// Whether the guest wrote a BD_ADDR the controller does not have: the
    /// controller keeps its own, which Read BD_ADDR gives.
    pub fn address_differs(&self) -> bool {
        self.address_differs
    }

    /// A guest USB bus reset: the shadow powers on again, as the model
    /// does, and the controller is kept out until the next HCI Reset.
    pub fn reset(&mut self) {
        self.shadow.power_on();
        self.written = None;
        self.resync();
    }

    /// The guest configured the function again, which drops its queues: the
    /// command in flight and those waiting are gone, and the controller is
    /// kept out until the next HCI Reset. The controller's credit comes back
    /// with them, as Linux's `hci_dev_do_reset` sets `cmd_cnt` to one: the
    /// answer that would give it back is dropped while the function is not
    /// configured, and the next HCI Reset would wait for it for good.
    pub fn resync(&mut self) {
        self.pending = None;
        self.backlog.clear();
        self.running.clear();
        self.credits = 1;
        self.resync = true;
    }

    /// ACL data from the guest. Milestone 1 forwards no connection, so the
    /// shadow owns every one: it returns the buffer, as the model does.
    pub fn acl(&mut self, packet: &[u8]) -> Vec<Step> {
        guest(self.shadow.acl(packet))
    }

    /// Whether ACL data from the controller reaches the guest: never in
    /// milestone 1, which lends no connection. The guest's connections are
    /// all the shadow's, so the controller's data names none the guest
    /// knows, and its handle could be one of the shadow's.
    pub fn chip_acl(&mut self) -> bool {
        self.counters.dropped += 1;
        false
    }

    /// Why the bridge fell back to the model, once it has.
    pub fn fallen(&self) -> Option<Fallback> {
        self.fallen
    }

    pub fn counters(&self) -> Counters {
        self.counters
    }

    /// The shadow's BD_ADDR now.
    pub fn address(&self) -> [u8; 6] {
        self.shadow.address()
    }

    /// Opcodes the shadow answered Unknown HCI Command that were not taken
    /// before, each once.
    pub fn take_unknown(&mut self) -> Vec<u16> {
        self.shadow.take_unknown()
    }

    /// Whether another command can wait for its turn.
    pub fn has_room(&self) -> bool {
        self.backlog.len() < BACKLOG
    }

    /// One command of the guest, header included, whose length its header
    /// already matches.
    pub fn command(&mut self, packet: &[u8], now: u64) -> Vec<Step> {
        if self.pending.is_some() {
            self.backlog.push_back(packet.to_vec());
            return Vec::new();
        }
        self.start(packet, now)
    }

    fn start(&mut self, packet: &[u8], now: u64) -> Vec<Step> {
        if opcode(packet) == WRITE_BD_ADDR && packet.len() == 3 + 6 {
            self.written = packet[3..].try_into().ok();
        }
        let shadow = self.shadow.command(packet, now);
        if self.fallen.is_some() {
            return guest(shadow);
        }
        let Some((command, kind)) = policy::forward(packet) else {
            self.counters.local += 1;
            return guest(shadow);
        };
        let mut pending = Pending {
            opcode: opcode(packet),
            parameters: packet[3..].to_vec(),
            kind,
            shadow,
            deadline: now.saturating_add(DEADLINE),
            unsent: None,
        };
        // A reset soon after the last start is bluetoothd restarting; at
        // the second in a row, the model takes over, answering this reset.
        // One after a session that ran past `SETTLED` starts a new one.
        // A reset ends the controller's procedures with no event.
        if pending.opcode == RESET && self.started {
            self.counters.restarts += 1;
            if now.saturating_sub(self.last_start) >= SETTLED {
                self.counters.streak = 0;
            }
            self.counters.streak += 1;
            if self.counters.streak >= RESTARTS {
                self.pending = Some(pending);
                return self.fall(Cause::Restarted, now);
            }
        }
        self.counters.forwarded += 1;
        // With no credit the command waits for the controller to give one
        // back; its deadline runs from now all the same.
        let steps = if self.credits == 0 {
            pending.unsent = Some(command);
            Vec::new()
        } else {
            self.credits -= 1;
            vec![Step::Chip(command)]
        };
        self.pending = Some(pending);
        steps
    }

    /// The command waiting for a credit, once the controller has one.
    fn send_held(&mut self) -> Vec<Step> {
        if self.credits == 0 {
            return Vec::new();
        }
        let Some(command) = self.pending.as_mut().and_then(|p| p.unsent.take()) else {
            return Vec::new();
        };
        self.credits -= 1;
        vec![Step::Chip(command)]
    }

    /// The commands that waited, until one is in flight again.
    fn next(&mut self, now: u64) -> Vec<Step> {
        let mut steps = Vec::new();
        while self.pending.is_none()
            && let Some(packet) = self.backlog.pop_front()
        {
            steps.extend(self.start(&packet, now));
        }
        steps
    }

    /// Falls back for good; the guest gets the shadow's answer to the
    /// command in flight, then the end of each procedure the controller was
    /// running (an inquiry completes, a name request times out), which the
    /// shadow then no longer runs, and the commands that waited are the
    /// shadow's. An HCI Reset in flight, whatever the cause, ends those
    /// procedures with no event: the shadow ran it, and the guest gets its
    /// answer.
    fn fall(&mut self, cause: Cause, now: u64) -> Vec<Step> {
        let pending = self.pending.take();
        self.fallen.get_or_insert(Fallback {
            opcode: pending.as_ref().map(|p| p.opcode),
            cause,
        });
        if pending.as_ref().is_some_and(|p| p.opcode == RESET) {
            self.running.clear();
        }
        let mut steps = pending.map(|p| guest(p.shadow)).unwrap_or_default();
        for running in core::mem::take(&mut self.running) {
            let end = match running {
                Running::Inquiry => {
                    self.shadow_only(INQUIRY_CANCEL, &[], now);
                    vec![INQUIRY_COMPLETE, 1, 0]
                }
                Running::Name(peer) => {
                    self.shadow_only(REMOTE_NAME_REQUEST_CANCEL, &peer, now);
                    let mut end = vec![REMOTE_NAME_REQUEST_COMPLETE, 255, PAGE_TIMEOUT];
                    end.extend_from_slice(&peer);
                    end.resize(2 + 255, 0);
                    end
                }
            };
            steps.push(Step::Guest(end));
        }
        steps.extend(self.next(now));
        steps
    }

    /// One event from the host's controller. The answer to the command in
    /// flight becomes the guest's; a vendor event, credits alone or a stale
    /// answer is dropped, as is everything while the controller is kept out
    /// or after the fallback; a Hardware Error falls back, and the guest,
    /// whose controller the model now is, never sees it; every other event
    /// reaches the guest as it is.
    pub fn chip_event(&mut self, event: &[u8], now: u64) -> Vec<Step> {
        if self.fallen.is_some() || event.first() == Some(&VENDOR_EVENT) {
            self.counters.dropped += 1;
            return Vec::new();
        }
        if event.first() == Some(&HARDWARE_ERROR) {
            self.counters.dropped += 1;
            let code = event.get(2).copied().unwrap_or(0);
            return self.fall(Cause::HardwareError(code), now);
        }
        if completes(event) {
            // Every completion gives the controller's credits, a stale one
            // and credits alone too.
            match *event {
                [COMMAND_COMPLETE, _, credits, ..] | [COMMAND_STATUS, _, _, credits, ..] => {
                    self.credits = credits;
                }
                _ => {}
            }
            let completion = Completion::of(event);
            let pending = completion.as_ref().and_then(|completion| {
                self.pending
                    .take_if(|p| p.unsent.is_none() && p.opcode == completion.opcode)
            });
            let (Some(completion), Some(pending)) = (completion, pending) else {
                // Credits alone (opcode 0, no status), or the answer to a
                // command no longer in flight.
                self.counters.dropped += 1;
                return self.send_held();
            };
            return self.answer(pending, &completion, now);
        }
        if self.resync {
            self.counters.dropped += 1;
            return Vec::new();
        }
        self.follow(event, now);
        self.counters.passed += 1;
        vec![Step::Guest(event.to_vec())]
    }

    /// One event from the host's controller while the guest has the
    /// function unconfigured, so no guest takes it: dropped, but for a
    /// Hardware Error, which falls back as it does configured, so the
    /// fallback does not wait for the next forwarded command's deadline.
    /// What the fallback gives the guest is dropped too: no guest can read
    /// it, and the function's queues start empty at its next
    /// configuration.
    pub fn chip_event_unconfigured(&mut self, event: &[u8], now: u64) {
        if event.first() == Some(&HARDWARE_ERROR) && self.fallen.is_none() {
            drop(self.chip_event(event, now));
        } else {
            self.counters.dropped += 1;
        }
    }

    /// The shadow ends what the controller's event ends, so that after a
    /// fallback it gives the guest no second end of it.
    fn follow(&mut self, event: &[u8], now: u64) {
        match *event {
            [INQUIRY_COMPLETE, ..] => {
                self.running.retain(|r| *r != Running::Inquiry);
                self.shadow_only(INQUIRY_CANCEL, &[], now);
            }
            [REMOTE_NAME_REQUEST_COMPLETE, _, _, ref peer @ ..] if peer.len() >= 6 => {
                let peer: [u8; 6] = peer[..6].try_into().expect("six bytes");
                self.running.retain(|r| *r != Running::Name(peer));
                self.shadow_only(REMOTE_NAME_REQUEST_CANCEL, &peer, now);
            }
            _ => {}
        }
    }

    /// Runs a command in the shadow alone, its events discarded.
    fn shadow_only(&mut self, opcode: u16, parameters: &[u8], now: u64) {
        let mut packet = opcode.to_le_bytes().to_vec();
        packet.push(parameters.len() as u8);
        packet.extend_from_slice(parameters);
        self.shadow.command(&packet, now);
    }

    /// The shadow follows what the controller's answer to `pending` says:
    /// the address it has, and a procedure it refused; and the bridge, the
    /// procedures it runs.
    fn follow_answer(&mut self, pending: &Pending, chip: &Completion<'_>, now: u64) {
        let shadow = pending.shadow.first().and_then(|e| Completion::of(e));
        let shadow_succeeded = shadow.is_some_and(|s| s.status == 0);
        let peer: Option<[u8; 6]> = pending.parameters.get(..6).and_then(|p| p.try_into().ok());
        match (pending.opcode, peer) {
            (INQUIRY, _) if chip.status == 0 && !self.running.contains(&Running::Inquiry) => {
                self.running.push(Running::Inquiry);
            }
            (REMOTE_NAME_REQUEST, Some(peer)) if chip.status == 0 => {
                self.running.push(Running::Name(peer));
            }
            (INQUIRY_CANCEL, _) if chip.status == 0 => {
                self.running.retain(|r| *r != Running::Inquiry);
            }
            (RESET, _) if chip.status == 0 => {
                self.running.clear();
                self.started = true;
                self.last_start = now;
            }
            _ => {}
        }
        match pending.opcode {
            RESET if chip.status == 0 => self.resync = false,
            READ_BD_ADDR if chip.status == 0 && chip.returns.len() == 6 => {
                let address = &chip.returns[..6];
                if self.written.is_some_and(|written| written != address) {
                    self.address_differs = true;
                }
                self.shadow_only(WRITE_BD_ADDR, address, now);
            }
            INQUIRY if chip.status != 0 && shadow_succeeded => {
                self.shadow_only(INQUIRY_CANCEL, &[], now);
            }
            REMOTE_NAME_REQUEST
                if chip.status != 0 && shadow_succeeded && pending.parameters.len() >= 6 =>
            {
                self.shadow_only(REMOTE_NAME_REQUEST_CANCEL, &pending.parameters[..6], now);
            }
            _ => {}
        }
    }

    fn answer(&mut self, pending: Pending, chip: &Completion<'_>, now: u64) -> Vec<Step> {
        let shadow = pending.shadow.first().and_then(|e| Completion::of(e));
        match policy::answer(pending.kind, shadow.as_ref(), chip) {
            Ok(returns) => {
                self.follow_answer(&pending, chip, now);
                self.counters.passed += 1;
                let mut steps = vec![Step::Guest(chip.event(&returns))];
                steps.extend(self.next(now));
                steps
            }
            Err(cause) => {
                self.pending = Some(pending);
                self.fall(cause, now)
            }
        }
    }

    /// The system count is `now`: a forwarded command past its deadline
    /// falls back, and the shadow's procedures that ended give their events
    /// once it has.
    pub fn advance(&mut self, now: u64) -> Vec<Step> {
        let mut steps = Vec::new();
        if self.pending.as_ref().is_some_and(|p| p.deadline <= now) {
            steps = self.fall(Cause::Timeout, now);
        }
        let later = self.shadow.advance(now);
        if self.fallen.is_some() {
            steps.extend(guest(later));
        }
        steps
    }

    /// The earliest system count at which [`Self::advance`] has something
    /// to do.
    pub fn deadline(&self) -> Option<u64> {
        let pending = self.pending.as_ref().map(|p| p.deadline);
        let shadow = self.fallen.and(self.shadow.deadline());
        pending.into_iter().chain(shadow).min()
    }

    /// The host's controller failed: the bridge falls back.
    pub fn chip_failed(&mut self, now: u64) -> Vec<Step> {
        if self.fallen.is_some() {
            return Vec::new();
        }
        self.fall(Cause::HostFailed, now)
    }
}

#[cfg(test)]
mod chip;
#[cfg(test)]
mod replay;
#[cfg(test)]
mod tests;
