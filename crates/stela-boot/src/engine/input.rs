//! The host's keyboards and pointers, read between engine slices and fed to
//! the native USB peripheral the guest enumerates: every input device probe
//! found, whatever its transport ([`stela_drivers::input::InputDevice`]).
//!
//! Their drivers reach registers and DMA pages through the loader's
//! identity root, as the storage services do, so each poll runs in that
//! root (`storage::RestoreRoot`). The guest sees an input change at
//! its peripheral's next interrupt poll after the slice it arrived in. A
//! device that gave no input for a few seconds is polled every
//! [`IDLE_MILLIS`], so its first input waits that long. A device whose
//! polls cost more than a [`COST_SHARE`]th of that is polled as much less
//! often, but at most [`SLOW_MILLIS`] apart when idle and [`ACTIVE_MILLIS`]
//! when active, which bound its first input's wait while the host drivers'
//! lock is free. A posted GPU call is pumped between devices
//! (`gpu::pump_posted`).
//!
//! No call waits for the host drivers' lock for longer than
//! [`STARVED_WAIT_MICROS`]: the turn may be a step of a PE that runs guest
//! code, and another processor can hold the lock for longer than one exit
//! may take. A poll, an LED change or a stopped device's removal that finds
//! the lock busy stays due for the next turn. A device holds its input
//! meanwhile as far as its hardware does: a USB device one report in its
//! queued transfer and what it keeps itself behind the endpoint's NAKs, a
//! PS/2 device its own buffer behind the controller's.
//!
//! Each device's first polling and LED failure is reported once. A device
//! that stops is reported and dropped, so re-plugged keyboards do not pile
//! up as stopped devices polled for the rest of the boot, and the guest
//! gets a release of every key and button it held ([`HeldInputs`]). The
//! guest has one keyboard and one pointer for all host devices, so each
//! device's keys and buttons are kept apart and joined: a touchpad's report
//! does not let go of a mouse's drag, nor one keyboard's Shift the other's.
//!
//! With `--trackpad`, every precision touchpad gives whole frames for the
//! guest's emulated Magic Trackpad 2 instead of moving its mouse
//! ([`HostInput::trackpad`]); what the guest asks of the trackpad is
//! logged as `TRACKPAD` lines when inputs are delivered.
use super::storage::{Inputs, RestoreRoot};
use crate::output;
use alloc::vec::Vec;
use stela_boot_core::options::GuestTrackpad;
use stela_drivers::input::{HeldInputs, Input, InputDevice, multitouch::Surface};
use stela_soc::machine::AppleDevices;
use stela_soc::usb::trackpad::{self, Identity};

/// The surface the guest trackpad maps without a precision touchpad: the
/// Magic Trackpad 2's own, 160.00 by 114.90 mm (hid-magicmouse.c
/// `TRACKPAD2_DIMENSION_X`/`_Y`).
const MT2_SURFACE: Surface = Surface {
    width_um: 160_000,
    height_um: 114_900,
};

// stela.efi carries the Magic Trackpad 2 model `--trackpad` cables:
// stela-soc's `trackpad` feature, set in crates/stela-boot/Cargo.toml.
const _: () = assert!(
    stela_soc::usb::TRACKPAD,
    "stela.efi needs stela-soc's trackpad feature"
);

/// What was reported of one device, each kind once, and when it was read.
#[derive(Clone, Default)]
struct Reported {
    /// The device's identity while it is read ([`HeldInputs`]).
    id: u64,
    failed: bool,
    leds: bool,
    /// The device's last polling failure, for the line that says why it
    /// stopped when the device holds no cause of its own.
    last_failure: Option<alloc::string::String>,
    /// The keyboard LEDs last wanted have not reached the device yet.
    leds_due: bool,
    /// The counter when the device was last polled, and when a poll last
    /// found input.
    looked: u64,
    active: u64,
    /// Its polls' recent cost in counter ticks, without lock waits: an
    /// eighth of each poll joins seven eighths of the last.
    cost: u64,
    /// Its polls since the last [`TIMES_SECONDS`] report.
    times: Times,
    /// For an i8042: its bytes read and dropped at the last report, and
    /// the standing status and configuration byte last logged.
    ps2: Option<Ps2Seen>,
}

/// What [`Reported`] keeps of an i8042 between lines.
#[derive(Clone, Copy, Default)]
struct Ps2Seen {
    read: [u64; 2],
    dropped: u64,
    logged: Option<(u8, Option<u8>)>,
}

/// One device's polls between reports, in 24 MHz counter ticks: how many,
/// the waits for the host-drivers lock before them and the polls' own time,
/// each in total and at most, and the turns a busy lock skipped it.
#[derive(Clone, Copy, Default)]
struct Times {
    polls: u64,
    waited: u64,
    longest_wait: u64,
    polled: u64,
    longest_poll: u64,
    skipped: u64,
    /// The inputs its polls gave the guest.
    delivered: u64,
}

/// How often the input devices' poll times are reported.
const TIMES_SECONDS: u64 = 10;

/// How often an idle device is polled. A poll can be a port or register
/// access a hypervisor takes to its device model, which waits there for
/// the model's lock, and the boot PE serves the host between slices of
/// guest code, holding back its guest, the devices' deadlines and the GPUs'
/// work. A device holds what arrives meanwhile: a USB device in its
/// transfer, a PS/2 device in its own buffer while the controller's is
/// full. USB low-speed keyboards and mice are polled as rarely.
const IDLE_MILLIS: u64 = 8;
/// How long a busy host-drivers lock may leave a due device unpolled before
/// a poll waits for the lock, for [`STARVED_WAIT_MICROS`] at most. A disk
/// command can hold it for tens of milliseconds; below this a due device is
/// polled at the next turn the lock is free.
const STARVED_MILLIS: u64 = 32;
/// How long a turn waits for the host drivers' lock for a device unpolled
/// past [`STARVED_MILLIS`], once for all its devices: about a service
/// step's length. The wait shows a holder that works in steps (the GPUs'
/// pump) that the lock is wanted, so it lets go at its next step; a holder
/// that keeps it longer leaves the poll due for the next turn.
const STARVED_WAIT_MICROS: u64 = 1000;
/// How long a device is polled at every service turn after it last gave
/// input, so a pointer's motion keeps its full rate.
const ACTIVE_SECONDS: u64 = 5;
/// A device's polls may take at most one part in this of the boot PE: a
/// poll costing more is spaced this many times its cost apart. A port read
/// that costs a microsecond on hardware can wait milliseconds for a
/// hypervisor's device-model lock; the boot PE's time is the guest's.
const COST_SHARE: u64 = 32;
/// The longest an idle device waits for its next poll however costly its
/// polls are, which its first input can wait.
const SLOW_MILLIS: u64 = 250;
/// The longest an active device waits for its next poll.
const ACTIVE_MILLIS: u64 = 32;

pub struct HostInput {
    inputs: Inputs,
    /// What was reported of each of `inputs.devices`, in the same order.
    reported: Vec<Reported>,
    /// The first input refusal, reported once.
    refused: bool,
    /// The guest's keyboard LEDs as last sent to the host keyboards.
    leds: Option<u8>,
    /// The guest's keyboard LEDs as the last delivery found them.
    wanted_leds: Option<u8>,
    /// The counter at the last poll-time report.
    reported_times: u64,
    /// What each device holds down, released when it stops.
    held: HeldInputs,
    /// The next device's identity.
    next_id: u64,
    /// `--trackpad`: precision touchpads give whole frames.
    touch_frames: bool,
    /// Whether the guest trackpad maps a host touchpad's surface, not the
    /// MT2's own it was cabled with when none was there.
    trackpad_surface: bool,
    /// A touchpad's surface that arrived after the cabling, for the guest
    /// trackpad at the next delivery.
    new_surface: Option<Surface>,
    /// The guest trackpad's counters at the last `TRACKPAD frames` line,
    /// and the counter then.
    trackpad_seen: trackpad::Counters,
    trackpad_reported: u64,
    /// `--profile`'s window mark on the host keyboards (`profile_window`).
    marks: stela_boot_core::profile_window::MarkKeys,
}

impl HostInput {
    pub fn new(inputs: Inputs) -> Self {
        for device in &inputs.devices {
            output::arguments(format_args!("INPUT host device {}\n", device.name()));
        }
        if inputs.devices.is_empty() {
            output::arguments(format_args!(
                "INPUT no host keyboard or pointer: the peripheral stays idle\n"
            ));
        }
        let reported = (0..inputs.devices.len() as u64)
            .map(|id| Reported {
                id,
                ..Reported::default()
            })
            .collect();
        Self {
            next_id: inputs.devices.len() as u64,
            reported,
            inputs,
            refused: false,
            leds: None,
            wanted_leds: None,
            reported_times: 0,
            held: HeldInputs::new(),
            touch_frames: false,
            trackpad_surface: false,
            new_surface: None,
            trackpad_seen: trackpad::Counters::default(),
            trackpad_reported: 0,
            marks: stela_boot_core::profile_window::MarkKeys::new(),
        }
    }

    /// `--trackpad`: switch every precision touchpad to frames mode, now
    /// and as they arrive, and the guest trackpad they feed: the first
    /// one's surface, else the Magic Trackpad 2's own until one arrives.
    pub fn trackpad(&mut self, kind: GuestTrackpad) -> trackpad::Config {
        self.touch_frames = true;
        let mut surface = None;
        for device in &mut self.inputs.devices {
            surface = surface.or(frames(device));
        }
        self.trackpad_surface = surface.is_some();
        let identity = match kind {
            GuestTrackpad::Mt2 => Identity::MagicTrackpad2,
            GuestTrackpad::Builtin => Identity::Builtin,
        };
        match surface {
            Some(surface) => output::arguments(format_args!(
                "TRACKPAD surface {}, as 05ac:{:04x} ({})\n",
                Millimetres(surface),
                identity.product(),
                kind.name()
            )),
            None => output::arguments(format_args!(
                "TRACKPAD no precision touchpad yet: the guest trackpad, as 05ac:{:04x} ({}), \
                 spans the MT2's own {} until one arrives\n",
                identity.product(),
                kind.name(),
                Millimetres(MT2_SURFACE)
            )),
        }
        trackpad::Config {
            identity,
            surface: surface.unwrap_or(MT2_SURFACE),
        }
    }

    /// Read `devices` too from now on: keyboards and pointers that arrived.
    /// With `--trackpad` a precision touchpad among them gives frames, and
    /// the first, when the guest trackpad spans the MT2's own size, gives
    /// it its surface at the next delivery.
    pub fn add(&mut self, mut devices: Vec<InputDevice>) {
        if self.touch_frames {
            for device in &mut devices {
                if let Some(surface) = frames(device)
                    && !core::mem::replace(&mut self.trackpad_surface, true)
                {
                    output::arguments(format_args!(
                        "TRACKPAD surface {} from the touchpad that arrived\n",
                        Millimetres(surface)
                    ));
                    self.new_surface = Some(surface);
                }
            }
        }
        for device in devices {
            output::arguments(format_args!("INPUT host device {}\n", device.name()));
            self.inputs.devices.push(device);
            self.reported.push(Reported {
                id: self.next_id,
                ..Reported::default()
            });
            self.next_id += 1;
        }
    }

    fn deliver(&mut self, devices: &mut AppleDevices, input: Input) {
        match devices.input(input) {
            Ok(()) => super::session::input(),
            Err(error) => {
                if !core::mem::replace(&mut self.refused, true) {
                    output::arguments(format_args!("INPUT refused {input:?}: {error}\n"));
                }
            }
        }
    }

    /// Deliver what [`Self::read`] read from the host's devices, and note
    /// the guest's lock-key LEDs for the next read to send.
    pub fn deliver_all(&mut self, devices: &mut AppleDevices, mut inputs: Vec<Input>) {
        super::profile_window::filter(&mut self.marks, &mut inputs);
        if let Some(surface) = self.new_surface.take() {
            devices.set_trackpad_surface(surface);
        }
        for input in inputs {
            self.deliver(devices, input);
        }
        self.wanted_leds = devices.keyboard_leds();
        if self.touch_frames {
            self.report_trackpad(devices);
        }
    }

    /// `TRACKPAD` lines: each of the guest trackpad's trace entries, and
    /// once per [`TIMES_SECONDS`] its frames since the last line, with the
    /// pad's rate by the frames' own scan times.
    fn report_trackpad(&mut self, devices: &mut AppleDevices) {
        for event in devices.trackpad_trace() {
            output::arguments(format_args!("TRACKPAD {event}\n"));
        }
        let now = super::host::fresh_counter();
        if now.wrapping_sub(self.trackpad_reported) < super::host::COUNTER_HZ * TIMES_SECONDS {
            return;
        }
        self.trackpad_reported = now;
        let Some(counters) = devices.trackpad_counters() else {
            return;
        };
        let seen = core::mem::replace(&mut self.trackpad_seen, counters);
        let received = counters.received.saturating_sub(seen.received);
        if received == 0 {
            return;
        }
        let reporting_us = counters.reporting_us.saturating_sub(seen.reporting_us);
        let rate = match reporting_us {
            0 => 0,
            us => received.saturating_mul(1_000_000) / us,
        };
        output::arguments(format_args!(
            "TRACKPAD frames received={received} sent={} coalesced={} dropped={} rate={rate} Hz \
             over {} ms of touch\n",
            counters.sent.saturating_sub(seen.sent),
            counters.coalesced.saturating_sub(seen.coalesced),
            counters.dropped.saturating_sub(seen.dropped),
            reporting_us / 1000,
        ));
    }

    /// Read the host's input devices, without the guest's devices, in the
    /// order probe found them. The guest's lock-key LEDs reach each host
    /// keyboard first, as Linux `hidinput_led_worker` sends an input
    /// device's LED changes.
    pub fn read(
        &mut self,
        #[cfg(feature = "diagnostics")] turn: &mut super::service::InputTurn,
    ) -> Vec<Input> {
        if let Some(leds) = self.wanted_leds
            && self.leds != Some(leds)
        {
            self.leds = Some(leds);
            #[cfg(feature = "diagnostics")]
            {
                turn.led_changed = true;
            }
            for reported in &mut self.reported {
                reported.leds_due = true;
            }
        }
        if let Some(leds) = self.leds {
            for (device, reported) in self.inputs.devices.iter_mut().zip(&mut self.reported) {
                if !reported.leds_due || device.stopped() {
                    continue;
                }
                let set = {
                    // SAFETY: as below, for the device's controller. A busy
                    // lock leaves the change due for the next turn.
                    let Some(_root) = (unsafe { RestoreRoot::try_enter(self.inputs.root) }) else {
                        continue;
                    };
                    device.set_leds(leds)
                };
                reported.leds_due = false;
                if let Err(error) = set
                    && !core::mem::replace(&mut reported.leds, true)
                {
                    output::arguments(format_args!("INPUT {} LEDs: {error}\n", device.name()));
                }
            }
        }
        #[cfg(feature = "diagnostics")]
        {
            turn.led_done = super::host::rdtsc();
            turn.devices = self.inputs.devices.len();
        }
        let mut pending = Vec::new();
        let mut stopped = false;
        let now = super::host::fresh_counter();
        let millis = super::host::COUNTER_HZ / 1000;
        let starved = millis * STARVED_MILLIS;
        // Whether this turn may still wait for the lock.
        let mut may_wait = true;
        for (device, reported) in self.inputs.devices.iter_mut().zip(&mut self.reported) {
            // A stopped device a busy lock kept from being dropped.
            if device.stopped() {
                stopped = true;
                continue;
            }
            let active =
                now.wrapping_sub(reported.active) < super::host::COUNTER_HZ * ACTIVE_SECONDS;
            let interval = if active {
                (reported.cost * COST_SHARE).min(millis * ACTIVE_MILLIS)
            } else {
                (reported.cost * COST_SHARE).clamp(millis * IDLE_MILLIS, millis * SLOW_MILLIS)
            };
            let since = now.wrapping_sub(reported.looked);
            if since < interval {
                continue;
            }
            let before = pending.len();
            let entering = super::host::fresh_counter();
            let entered;
            let polled = {
                // SAFETY: the loader root `storage::open` captured, which
                // maps this device.
                let root = unsafe {
                    if since >= interval + starved && core::mem::take(&mut may_wait) {
                        RestoreRoot::enter_within(self.inputs.root, STARVED_WAIT_MICROS)
                    } else {
                        RestoreRoot::try_enter(self.inputs.root)
                    }
                };
                let Some(_root) = root else {
                    reported.times.skipped += 1;
                    continue;
                };
                entered = super::host::fresh_counter();
                device.poll(&mut pending)
            };
            self.held.polled(reported.id, &mut pending, before);
            let times = &mut reported.times;
            let (waited, took) = (
                entered.wrapping_sub(entering),
                super::host::fresh_counter().wrapping_sub(entered),
            );
            times.polls += 1;
            times.waited += waited;
            times.longest_wait = times.longest_wait.max(waited);
            times.polled += took;
            times.longest_poll = times.longest_poll.max(took);
            reported.cost = reported.cost - reported.cost / 8 + took / 8;
            reported.looked = now;
            reported.times.delivered += pending.len().saturating_sub(before) as u64;
            if pending.len() != before {
                reported.active = now;
                // A laptop's own keyboard or touchpad lights a panel the
                // lid darkened, whatever its `_LID` reads (`--lid method`).
                if built_in(device) && super::lid::built_in_input() == Some(false) {
                    output::arguments(format_args!(
                        "HOST lid: input of {}: the panel lights again\n",
                        device.name()
                    ));
                }
            }
            controller_change(&*device, reported);
            for notice in device.take_notices() {
                output::arguments(format_args!("INPUT {}: {notice}\n", device.name()));
            }
            if let Err(error) = polled {
                let error = alloc::format!("{error}");
                if !core::mem::replace(&mut reported.failed, true) {
                    output::arguments(format_args!("INPUT {}: {error}\n", device.name()));
                }
                reported.last_failure = Some(error);
            }
            stopped |= device.stopped();
            // A poll can take milliseconds in a hypervisor: a posted GPU call
            // does not wait for the remaining devices too.
            super::gpu::pump_posted();
        }
        if stopped {
            self.remove_stopped(&mut pending);
        }
        if now.wrapping_sub(self.reported_times) >= super::host::COUNTER_HZ * TIMES_SECONDS {
            self.reported_times = now;
            self.report_times();
        }
        pending
    }

    /// `INPUT TIMES name:polls/waited_us/longest_wait_us/polled_us/
    /// longest_poll_us/skipped/delivered[/read=K+M/dropped=D] …`: each
    /// device's polls since the last report, the host-drivers lock waits
    /// before them, their own time, the inputs they gave the guest, and for
    /// an i8042 the bytes read from its keyboard and mouse ports and the
    /// bytes dropped since the last report.
    fn report_times(&mut self) {
        let micros = |ticks: u64| ticks / (super::host::COUNTER_HZ / 1_000_000);
        let mut line = alloc::string::String::from("INPUT TIMES");
        for (device, reported) in self.inputs.devices.iter().zip(&mut self.reported) {
            let t = core::mem::take(&mut reported.times);
            let _ = core::fmt::Write::write_fmt(
                &mut line,
                format_args!(
                    " {}:{}/{}/{}/{}/{}/{}/{}",
                    device.name().replace(' ', "_"),
                    t.polls,
                    micros(t.waited),
                    micros(t.longest_wait),
                    micros(t.polled),
                    micros(t.longest_poll),
                    t.skipped,
                    t.delivered,
                ),
            );
            if let Some(counters) = device.ps2_counters() {
                let seen = reported.ps2.get_or_insert_with(Ps2Seen::default);
                let _ = core::fmt::Write::write_fmt(
                    &mut line,
                    format_args!(
                        "/read={}+{}/dropped={}",
                        counters.read[0].saturating_sub(seen.read[0]),
                        counters.read[1].saturating_sub(seen.read[1]),
                        counters.dropped.saturating_sub(seen.dropped),
                    ),
                );
                seen.read = counters.read;
                seen.dropped = counters.dropped;
            }
        }
        output::arguments(format_args!("{line}\n"));
    }

    /// Report and drop every device that stopped, appending to `pending` a
    /// release of every key and button it held, at once even while a busy
    /// lock keeps the device. Dropping one can stop its transfers, so it
    /// runs in the loader's root as a poll does; a device a busy lock keeps
    /// is dropped at a later turn, unpolled meanwhile.
    fn remove_stopped(&mut self, pending: &mut Vec<Input>) {
        let mut index = 0;
        while index < self.inputs.devices.len() {
            if !self.inputs.devices[index].stopped() {
                index += 1;
                continue;
            }
            self.held.stopped(self.reported[index].id, pending);
            // SAFETY: the loader root `storage::open` captured, which maps
            // this device; the processor's pinned record is active.
            let Some(root) = (unsafe { RestoreRoot::try_enter(self.inputs.root) }) else {
                index += 1;
                continue;
            };
            let device = self.inputs.devices.remove(index);
            let reported = self.reported.remove(index);
            let name = device.name();
            let cause = stop_cause(device.stop_cause(), reported.last_failure);
            drop(device);
            drop(root);
            output::arguments(format_args!("INPUT {name} stopped and removed: {cause}\n"));
        }
    }
}

/// Set `device` to give whole frames if it is a precision touchpad, logging
/// it: its surface, else `None`.
fn frames(device: &mut InputDevice) -> Option<Surface> {
    if !device.set_touch_frames(true) {
        return None;
    }
    output::arguments(format_args!("TRACKPAD frames from {}\n", device.name()));
    device.touch_surface()
}

/// A surface as `W.WW x H.HH mm`.
struct Millimetres(Surface);

impl core::fmt::Display for Millimetres {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let mm = |um: u32| (um / 1000, um % 1000 / 10);
        let ((w, w_frac), (h, h_frac)) = (mm(self.0.width_um), mm(self.0.height_um));
        write!(f, "{w}.{w_frac:02} x {h}.{h_frac:02} mm")
    }
}

/// Why a device stopped, for its removal's line: the cause the device
/// holds, else its last polling failure, else that neither is known.
/// Whether `device` is a laptop's own: its i8042 keyboard or touchpad, an
/// I2C HID device, an ELAN or Cypress touchpad or an ISH client, where a
/// USB or virtio device is plugged in.
fn built_in(device: &InputDevice) -> bool {
    matches!(
        device,
        InputDevice::Ps2(_)
            | InputDevice::I2cHid(_)
            | InputDevice::Touchpad(_)
            | InputDevice::Ish(_)
    )
}

fn stop_cause(
    held: Option<alloc::string::String>,
    last_failure: Option<alloc::string::String>,
) -> alloc::string::String {
    held.or(last_failure)
        .unwrap_or_else(|| alloc::string::String::from("the device recorded no cause"))
}

/// `INPUT <name> controller: status .., configuration ..` for an i8042 at
/// its first poll and whenever its standing status bits or its
/// configuration byte change, so a log says whether its keyboard port was
/// on, translated and interrupting while it was polled, which set its
/// keys are decoded as, and which check found the keyboard.
fn controller_change(device: &InputDevice, reported: &mut Reported) {
    let Some(counters) = device.ps2_counters() else {
        return;
    };
    let Some(status) = counters.status else {
        return;
    };
    let now = (
        stela_drivers::input::ps2::standing_status(status),
        counters.config,
    );
    let seen = reported.ps2.get_or_insert_with(Ps2Seen::default);
    if seen.logged == Some(now) {
        return;
    }
    seen.logged = Some(now);
    let firmware = counters
        .firmware_config
        .map_or_else(alloc::string::String::new, |config| {
            alloc::format!(
                "; the firmware left configuration {config:02x}, read with both ports disabled"
            )
        });
    output::arguments(format_args!(
        "INPUT {} controller: {}{firmware}; {}; {}\n",
        device.name(),
        stela_drivers::input::ps2::describe_controller(Some(status), counters.config),
        stela_drivers::input::ps2::describe_keyboard_path(counters.translated),
        stela_drivers::input::ps2::describe_presence(counters.keyboard_presence)
    ));
}
