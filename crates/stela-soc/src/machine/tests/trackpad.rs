//! The emulated Magic Trackpad 2 on the native hub: its port, and host
//! precision-touchpad frames reaching it.
use super::*;
use crate::usb::{
    BusPeer, Reply, Token,
    control::{
        GET_DESCRIPTOR, SET_ADDRESS, SET_CONFIGURATION,
        tests::{setup, transfer},
    },
    hub::Ports,
    trackpad::{self, Config, Identity, Trackpad},
};
use stela_drivers::input::multitouch::{Finger, Frame, Surface};

const SURFACE: Surface = Surface {
    width_um: 126_900,
    height_um: 82_000,
};

fn config() -> Config {
    Config {
        identity: Identity::MagicTrackpad2,
        surface: SURFACE,
    }
}

fn attachments(trackpad: bool, network: bool, bluetooth: bool) -> Attachments {
    Attachments {
        input: Peripheral::new(),
        trackpad: trackpad.then(|| Trackpad::new(config(), &trackpad::serial(1))),
        network: network.then(|| {
            Ecm::with_model(Mac([2, 0, 0, 0, 0, 1]), crate::usb::ecm::Model::Ncm).unwrap()
        }),
        bluetooth: bluetooth.then(Hci::new),
        disks: Vec::new(),
        touch_dropped: 0,
    }
}

/// The vendor ID a port's function enumerates with.
fn vendor(function: &mut dyn Function) -> u16 {
    let device = transfer(function, setup(0x80, GET_DESCRIPTOR, 0x100, 0, 18), &[]).unwrap();
    u16::from_le_bytes([device[8], device[9]])
}

#[test]
fn trackpad_cabled_on_full_speed_port() {
    let mut ports = attachments(true, true, true);
    assert_eq!(ports.count(), 4);
    assert!(ports.count() <= hub::MAX_PORTS);
    let speeds: Vec<Speed> = (0..ports.count()).map(|port| ports.speed(port)).collect();
    assert_eq!(speeds, [Speed::Full, Speed::High, Speed::High, Speed::Full]);
    // The trackpad takes the last port: the keyboard and pointer, the
    // network adapter and Bluetooth keep the ports they have without it, so
    // macOS sees no new network interface.
    let mut without = attachments(false, true, true);
    for port in 0..3 {
        assert_eq!(vendor(ports.function(port)), vendor(without.function(port)));
    }
    assert_eq!(vendor(ports.function(3)), 0x05ac);
    // Alone beside the keyboard and pointer, it is the second.
    let mut alone = attachments(true, false, false);
    assert_eq!((alone.count(), alone.speed(1)), (2, Speed::Full));
    assert_eq!(vendor(alone.function(1)), 0x05ac);
    assert_eq!(
        UsbCables::disk_ports(true, true, true),
        UsbCables::disk_ports(true, true, false) - 1
    );
    assert_eq!(
        UsbCables::disk_ports(false, false, true),
        hub::MAX_PORTS - 2
    );
}

#[test]
fn no_flag_no_port() {
    // Without the trackpad the ports are today's: the keyboard and pointer
    // at full speed, then the network adapter and Bluetooth at high speed.
    let mut ports = attachments(false, true, true);
    assert_eq!(ports.count(), 3);
    let speeds: Vec<Speed> = (0..3).map(|port| ports.speed(port)).collect();
    assert_eq!(speeds, [Speed::Full, Speed::High, Speed::High]);
    assert_eq!(vendor(ports.function(0)), 0x1209);
    assert_ne!(vendor(ports.function(1)), 0x05ac);
    assert_eq!(
        UsbCables::disk_ports(false, false, false),
        hub::MAX_PORTS - 1
    );
    assert_eq!(UsbCables::disk_ports(true, true, false), hub::MAX_PORTS - 3);
}

/// A board with a USB controller on connector 1 and the native hub cabled
/// to it with `cables`.
fn cabled(cables: UsbCables) -> AppleDevices {
    let mut d = AppleDevices::new();
    let dart = d
        .add_dart(
            Range {
                physical: 0x382f00000,
                bytes: 0x4000,
            },
            16,
            None,
            None,
        )
        .unwrap();
    add_usb_phy(&mut d);
    d.add_usb(&usb_placement(), dart).unwrap();
    d.attach_input(1, cables).unwrap();
    d
}

fn the_trackpad(d: &mut AppleDevices) -> &mut Trackpad {
    d.usb_masters[0]
        .peripheral
        .as_mut()
        .unwrap()
        .ports
        .trackpad
        .as_mut()
        .expect("a trackpad is cabled")
}

fn enumerate(t: &mut Trackpad) {
    transfer(t, setup(0, SET_ADDRESS, 5, 0, 0), &[]).unwrap();
    transfer(t, setup(0, SET_CONFIGURATION, 1, 0, 0), &[]).unwrap();
}

/// One report from the multitouch endpoint, or `None` for a NAK.
fn report(t: &mut Trackpad) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let mut packet = [0u8; 64];
        match t.packet(0x83, Token::In, &mut packet).unwrap() {
            Reply::Ack(n) => {
                out.extend_from_slice(&packet[..n]);
                if n < 64 {
                    return Some(out);
                }
            }
            Reply::Nak => return None,
            Reply::Stall => panic!("the multitouch endpoint stalled"),
        }
    }
}

/// A guest network whose host adapter has no usable station address yet
/// (an Intel Wi-Fi chip before its firmware read it, all zeros) places no
/// USB Ethernet adapter and the board is still placed, where 0.6.1 stopped
/// the boot; a usable address places one.
#[test]
fn a_network_address_that_is_not_unicast_places_no_usb_ethernet() {
    for unusable in [Mac([0; 6]), Mac([1, 0, 0x5e, 0, 0, 1])] {
        let mut d = cabled(UsbCables {
            network: Some(unusable),
            ..UsbCables::default()
        });
        assert!(d.usb_network().is_err());
        assert!(d.network_deliver(&[0; 60]).is_err());
    }
    let mut d = cabled(UsbCables {
        network: Some(Mac([2, 0, 0, 0, 0, 1])),
        ..UsbCables::default()
    });
    assert!(d.usb_network().is_ok());
}

#[test]
fn touch_routes_to_trackpad() {
    let mut d = cabled(UsbCables {
        trackpad: Some(config()),
        ..UsbCables::default()
    });
    enumerate(the_trackpad(&mut d));
    let finger = Finger {
        id: 0,
        x_um: 63_450,
        y_um: 41_000,
        tip: true,
        confident: true,
    };
    d.input(Input::Touch(Frame::new(1_000_000, 0, &[finger])))
        .unwrap();
    let r = report(the_trackpad(&mut d)).expect("the frame reached the trackpad");
    assert_eq!((r[0], r.len()), (0x02, 21));
    assert_eq!(d.trackpad_counters().unwrap().received, 1);
    assert!(
        d.trackpad_trace()
            .contains(&trackpad::TrackpadEvent::Configured(1))
    );
    // The keyboard and mouse still reach the native peripheral.
    d.input(Input::MouseButtons(1)).unwrap();
}

#[test]
fn a_late_touchpad_s_surface_reaches_the_trackpad() {
    let mut d = cabled(UsbCables {
        trackpad: Some(config()),
        ..UsbCables::default()
    });
    let late = Surface {
        width_um: 100_000,
        height_um: 60_000,
    };
    d.set_trackpad_surface(late);
    assert_eq!(the_trackpad(&mut d).surface(), late);
    // Without a trackpad there is nothing to tell.
    let mut bare = cabled(UsbCables::default());
    bare.set_trackpad_surface(late);
    assert_eq!(bare.trackpad_counters(), None);
}

#[test]
fn touch_without_a_trackpad_is_dropped_and_counted() {
    let mut d = cabled(UsbCables::default());
    d.input(Input::Touch(Frame::lifted(5))).unwrap();
    d.input(Input::Touch(Frame::lifted(6))).unwrap();
    assert_eq!(d.trackpad_counters(), None);
    assert!(d.trackpad_trace().is_empty());
    let hub = d.usb_masters[0].peripheral.as_ref().unwrap();
    assert_eq!(hub.ports.touch_dropped, 2);
    assert!(d.usb_diagnostics()[0].contains("touch_dropped=2"));
    // Without one, the diagnostics line is today's.
    let quiet = cabled(UsbCables::default());
    assert!(!quiet.usb_diagnostics()[0].contains("touch_dropped"));
}

/// `stela-drivers`' `ptp/tests.rs` fixture: a precision touchpad of two
/// finger collections, 1000 x 600 units over 100 x 60 mm, with a contact
/// count, a scan time and one button, Input Mode in feature report 3 and
/// the switches in feature report 4.
fn ptp_finger() -> Vec<u8> {
    vec![
        0x09, 0x22, 0xa1, 0x02, // Finger
        0x15, 0x00, 0x25, 0x01, 0x09, 0x47, 0x09, 0x42, 0x95, 0x02, 0x75, 0x01, 0x81,
        0x02, // Confidence, Tip Switch
        0x95, 0x06, 0x81, 0x03, // padding
        0x25, 0x05, 0x09, 0x51, 0x75, 0x08, 0x95, 0x01, 0x81, 0x02, // Contact ID
        0x05, 0x01, 0x15, 0x00, 0x26, 0xe8, 0x03, 0x35, 0x00, 0x46, 0xe8, 0x03, 0x65, 0x11, 0x55,
        0x0e, 0x75, 0x10, 0x95, 0x01, 0x09, 0x30, 0x81, 0x02, // X
        0x26, 0x58, 0x02, 0x46, 0x58, 0x02, 0x09, 0x31, 0x81, 0x02, // Y
        0x65, 0x00, 0x55, 0x00, 0x35, 0x00, 0x45, 0x00, 0x05, 0x0d, 0xc0,
    ]
}

fn ptp_descriptor() -> Vec<u8> {
    let mut d = vec![0x05, 0x0d, 0x09, 0x05, 0xa1, 0x01, 0x85, 0x01];
    d.extend(ptp_finger());
    d.extend(ptp_finger());
    d.extend([
        0x15, 0x00, 0x27, 0xff, 0xff, 0x00, 0x00, 0x75, 0x10, 0x95, 0x01, 0x09, 0x56, 0x81,
        0x02, // Scan Time
        0x25, 0x7f, 0x75, 0x08, 0x09, 0x54, 0x81, 0x02, // Contact Count
        0x05, 0x09, 0x09, 0x01, 0x25, 0x01, 0x75, 0x01, 0x95, 0x01, 0x81, 0x02, // Button 1
        0x95, 0x07, 0x81, 0x03, // padding
        0x05, 0x0d, 0x85, 0x02, 0x09, 0x55, 0x25, 0x05, 0x75, 0x08, 0x95, 0x01, 0xb1,
        0x02, // Contact Count Maximum
        0xc0, // end Touch Pad
        0x05, 0x0d, 0x09, 0x0e, 0xa1, 0x01, 0x85, 0x03, // Device Configuration
        0x09, 0x22, 0xa1, 0x02, 0x09, 0x52, 0x15, 0x00, 0x25, 0x0a, 0x75, 0x08, 0x95, 0x01, 0xb1,
        0x02, 0xc0, // Input Mode
        0x09, 0x22, 0xa1, 0x00, 0x85, 0x04, 0x09, 0x57, 0x09, 0x58, 0x75, 0x01, 0x95, 0x02, 0x25,
        0x01, 0xb1, 0x02, 0x95, 0x06, 0xb1, 0x03, 0xc0, // switches
        0xc0,
    ]);
    d
}

/// One report of the fixture: one finger (confidence, tip, id, x, y) and
/// an empty second slot.
fn ptp_report(finger: (bool, bool, u8, u16, u16), scan: u16) -> Vec<u8> {
    let mut r = vec![0x01];
    for (confidence, tip, id, x, y) in [finger, (false, false, 0, 0, 0)] {
        r.push(u8::from(confidence) | u8::from(tip) << 1);
        r.push(id);
        r.extend(x.to_le_bytes());
        r.extend(y.to_le_bytes());
    }
    r.extend(scan.to_le_bytes());
    r.push(1);
    r.push(0);
    r
}

fn hex(text: &str) -> Vec<u8> {
    text.split_whitespace()
        .map(|byte| u8::from_str_radix(byte, 16).unwrap())
        .collect()
}

#[test]
fn ptp_to_wire_e2e() {
    use stela_drivers::{
        input::ptp::{Profile, Ptp},
        usb::hid::{Translated, Translator},
    };
    let mut translator = Translator::describe(&ptp_descriptor()).unwrap();
    let profile = Profile::select(translator.descriptor().unwrap()).unwrap();
    let mut ptp = Ptp::new(profile);
    ptp.set_frames(true);
    let mut d = cabled(UsbCables {
        trackpad: Some(Config {
            identity: Identity::MagicTrackpad2,
            surface: ptp.surface(),
        }),
        ..UsbCables::default()
    });
    enumerate(the_trackpad(&mut d));
    let mut wire = Vec::new();
    for (finger, scan, now_us) in [
        ((true, true, 1, 500, 300), 100, 1_000_000),
        ((true, true, 1, 600, 300), 180, 1_004_000),
        ((true, false, 1, 600, 300), 260, 1_009_000),
    ] {
        let (mut events, mut inputs) = (Vec::new(), Vec::new());
        let Translated::Data(frame) =
            translator.translate(&ptp_report(finger, scan), &mut events, &mut inputs)
        else {
            panic!("report rejected");
        };
        ptp.report(
            translator.descriptor().unwrap(),
            &frame,
            &events,
            now_us,
            &mut inputs,
        );
        assert!(matches!(inputs[..], [Input::Touch(_)]), "{inputs:?}");
        for input in inputs {
            d.input(input).unwrap();
        }
        wire.push(report(the_trackpad(&mut d)).unwrap());
    }
    wire.push(report(the_trackpad(&mut d)).unwrap());
    assert_eq!(report(the_trackpad(&mut d)), None);
    assert_eq!(
        wire,
        [
            hex("02 00 00 00 00 00 00 03 31 44 1f 00 80 40 f9 6b 14 14 0a 05 81"),
            hex("02 00 00 00 00 00 00 03 31 84 1f 00 5c 42 f9 8b 14 14 0a 05 81"),
            hex("02 00 00 00 00 00 00 02 31 c4 1f 00 5c 42 f9 eb 00 00 00 00 81"),
            hex("02 00 00 00 00 00 00 02 31 c4 1f 00"),
        ]
    );
}
