use super::super::model::{self, Model};
use super::chip::{self, Chip};
use super::*;

const ADDRESS: [u8; 6] = [0x11, 0x22, 0x33, 0x44, 0x55, 0x02];
/// The host controller's own address, as a test gives it.
const CHIP_ADDRESS: [u8; 6] = [0xa1, 0xa2, 0xa3, 0xa4, 0xa5, 0x02];

fn packet(opcode: u16, parameters: &[u8]) -> Vec<u8> {
    let mut packet = opcode.to_le_bytes().to_vec();
    packet.push(parameters.len() as u8);
    packet.extend_from_slice(parameters);
    packet
}

/// A Command Complete with `credits`, `status` and `returns`.
fn complete(opcode: u16, credits: u8, status: u8, returns: &[u8]) -> Vec<u8> {
    let [low, high] = opcode.to_le_bytes();
    let mut event = vec![
        COMMAND_COMPLETE,
        4 + returns.len() as u8,
        credits,
        low,
        high,
        status,
    ];
    event.extend_from_slice(returns);
    event
}

fn status(opcode: u16, credits: u8, status: u8) -> Vec<u8> {
    let [low, high] = opcode.to_le_bytes();
    vec![COMMAND_STATUS, 4, status, credits, low, high]
}

/// The events of `steps`, which must send nothing to the host's controller.
fn events(steps: Vec<Step>) -> Vec<Vec<u8>> {
    steps
        .into_iter()
        .map(|step| match step {
            Step::Guest(event) => event,
            Step::Chip(command) => panic!("{:#06x} went to the chip", command.opcode()),
        })
        .collect()
}

/// The command of `steps`, which must be exactly one for the host's
/// controller.
fn chip(steps: Vec<Step>) -> Vec<u8> {
    match &steps[..] {
        [Step::Chip(command)] => command.bytes().to_vec(),
        _ => panic!("one command for the chip: {steps:02x?}"),
    }
}

#[test]
fn a_forwarded_answer_wins_over_the_shadows_and_has_one_credit() {
    let mut bridge = Bridge::new(ADDRESS);
    let read_address = packet(0x1009, &[]);
    assert_eq!(chip(bridge.command(&read_address, 0)), read_address);
    // The host controller's credits are its own.
    let answer = events(bridge.chip_event(&complete(0x1009, 5, 0, &CHIP_ADDRESS), 10));
    assert_eq!(answer, [complete(0x1009, 1, 0, &CHIP_ADDRESS)]);
    // A procedure answers with Command Status, also with one credit.
    let inquiry = packet(0x0401, &[0x33, 0x8b, 0x9e, 8, 0]);
    assert_eq!(chip(bridge.command(&inquiry, 20)), inquiry);
    assert_eq!(
        events(bridge.chip_event(&status(0x0401, 2, 0), 30)),
        [status(0x0401, 1, 0)]
    );
    assert_eq!(bridge.fallen(), None);
    assert_eq!(bridge.counters().forwarded, 2);
}

#[test]
fn vendor_and_unlisted_commands_are_the_shadows() {
    let mut bridge = Bridge::new(ADDRESS);
    // Read Local Version Information is the module's: Broadcom, Core 4.0.
    let version = events(bridge.command(&packet(0x1001, &[]), 0));
    assert_eq!(version[0][5..], [0, 6, 0, 0, 6, 15, 0, 0x0e, 0x41]);
    // Write BD_ADDR is the module's vendor command.
    let written = events(bridge.command(&packet(0xfc01, &[9, 8, 7, 6, 5, 0x42]), 0));
    assert_eq!(written, [complete(0xfc01, 1, 0, &[])]);
    assert_eq!(bridge.address(), [9, 8, 7, 6, 5, 0x42]);
    // Every vendor opcode, of every parameter length the guest might send.
    for ocf in 0..0x400u16 {
        let opcode = 0x3f << 10 | ocf;
        for length in [0, 1, 9] {
            assert!(policy::forward(&packet(opcode, &vec![0; length])).is_none());
            for step in bridge.command(&packet(opcode, &vec![0; length]), 0) {
                assert!(
                    matches!(step, Step::Guest(_)),
                    "{opcode:#06x} reached the chip"
                );
            }
        }
    }
    // A standard command not on the list.
    let scan = events(bridge.command(&packet(0x0c1a, &[3]), 0));
    assert_eq!(scan, [complete(0x0c1a, 1, 0, &[])]);
    assert_eq!(bridge.counters().forwarded, 0);
    assert_eq!(bridge.fallen(), None);
}

/// The list holds standard commands the model implements and lists, each
/// with the bit bluetoothd reads for it, once.
#[test]
fn the_forwarded_commands_are_standard_ones_the_model_lists() {
    let bitmap = super::super::model::supported_commands();
    for (index, &(opcode, octet, bit, _, _)) in policy::FORWARDED.iter().enumerate() {
        assert!(!policy::vendor(opcode));
        assert!(super::super::model::implements(opcode), "{opcode:#06x}");
        assert_ne!(bitmap[usize::from(octet)] & 1 << bit, 0, "{opcode:#06x}");
        for &(other, other_octet, other_bit, _, _) in &policy::FORWARDED[index + 1..] {
            assert_ne!(other, opcode);
            assert_ne!((other_octet, other_bit), (octet, bit), "{opcode:#06x}");
        }
    }
}

/// A listed command goes to the controller only with its specification's
/// parameter length; any other is the shadow's, so the controller never
/// reads past what the guest sent or takes bytes no field names.
#[test]
fn a_listed_command_of_another_parameter_length_is_the_shadows() {
    for &(opcode, _, _, length, _) in policy::FORWARDED {
        let length = usize::from(length);
        // Read Local Extended Features forwards pages 0 and 1.
        let page = u8::from(opcode == 0x1004);
        let mut parameters = vec![0; length];
        if let Some(first) = parameters.first_mut() {
            *first = page;
        }
        assert!(
            policy::forward(&packet(opcode, &parameters)).is_some(),
            "{opcode:#06x}"
        );
        for other in [length.wrapping_sub(1), length + 1] {
            if other > 255 {
                continue;
            }
            let wrong = packet(opcode, &vec![page; other]);
            assert!(policy::forward(&wrong).is_none(), "{opcode:#06x} {other}");
            let mut bridge = Bridge::new(ADDRESS);
            for step in bridge.command(&wrong, 0) {
                assert!(matches!(step, Step::Guest(_)), "{opcode:#06x} {other}");
            }
        }
    }
    // A header that disagrees with the packet's length.
    let mut mask = packet(0x0c01, &[0xff; 8]);
    mask[2] = 7;
    assert!(policy::forward(&mask).is_none());
}

#[test]
fn one_command_is_in_flight_and_the_next_waits_its_turn() {
    let mut bridge = Bridge::new(ADDRESS);
    let reset = packet(0x0c03, &[]);
    assert_eq!(chip(bridge.command(&reset, 0)), reset);
    // A local command and a forwarded one wait behind it.
    assert!(bridge.command(&packet(0x1001, &[]), 1).is_empty());
    let mask = packet(0x0c01, &[0xff; 8]);
    assert!(bridge.command(&mask, 2).is_empty());
    let steps = bridge.chip_event(&complete(0x0c03, 1, 0, &[]), 3);
    assert_eq!(steps.len(), 3);
    assert_eq!(steps[0], Step::Guest(complete(0x0c03, 1, 0, &[])));
    assert!(matches!(&steps[1], Step::Guest(e) if e[3..5] == [0x01, 0x10]));
    assert!(matches!(&steps[2], Step::Chip(c) if c.bytes() == mask));
    assert!(bridge.has_room());
}

#[test]
fn a_reconfiguration_with_a_command_in_flight_gives_its_credit_back() {
    let mut bridge = Bridge::new(ADDRESS);
    chip(bridge.command(&packet(0x0c14, &[]), 0));
    // bluetoothd configures the function again; the command's answer, which
    // would give the credit back, never reaches the bridge.
    bridge.resync();
    let reset = packet(0x0c03, &[]);
    assert_eq!(chip(bridge.command(&reset, 1)), reset);
    assert_eq!(bridge.fallen(), None);
    // A bus reset does the same.
    let mut bridge = Bridge::new(ADDRESS);
    chip(bridge.command(&packet(0x0c14, &[]), 0));
    bridge.reset();
    assert_eq!(chip(bridge.command(&reset, 1)), reset);
}

#[test]
fn credits_alone_and_stale_answers_are_dropped() {
    let mut bridge = Bridge::new(ADDRESS);
    // Credits alone (opcode 0, no status), before and during a command.
    assert!(
        bridge
            .chip_event(&[COMMAND_COMPLETE, 3, 1, 0, 0], 0)
            .is_empty()
    );
    let local_name = packet(0x0c14, &[]);
    chip(bridge.command(&local_name, 0));
    assert!(
        bridge
            .chip_event(&[COMMAND_COMPLETE, 3, 1, 0, 0], 0)
            .is_empty()
    );
    assert!(bridge.chip_event(&status(0x0000, 1, 0), 0).is_empty());
    // An answer to another command than the one in flight.
    assert!(
        bridge
            .chip_event(&complete(0x0c03, 1, 0, &[]), 0)
            .is_empty()
    );
    let answer = events(bridge.chip_event(&complete(0x0c14, 1, 0, &[0; 248]), 0));
    assert_eq!(answer.len(), 1);
    assert_eq!(bridge.counters().dropped, 4);
    assert_eq!(bridge.fallen(), None);
}

#[test]
fn a_deadline_gives_the_shadows_answer_and_falls_back_for_good() {
    let mut bridge = Bridge::new(ADDRESS);
    chip(bridge.command(&packet(0x0c03, &[]), 100));
    assert_eq!(bridge.deadline(), Some(100 + DEADLINE));
    assert!(bridge.advance(100 + DEADLINE - 1).is_empty());
    let answer = events(bridge.advance(100 + DEADLINE));
    assert_eq!(answer, [complete(0x0c03, 1, 0, &[])]);
    assert_eq!(
        bridge.fallen(),
        Some(Fallback {
            opcode: Some(0x0c03),
            cause: Cause::Timeout,
        })
    );
    assert_eq!(
        bridge.fallen().unwrap().to_string(),
        "0x0c03 no answer in 1000 ms"
    );
    // The late answer, and every event after, is dropped.
    assert!(
        bridge
            .chip_event(&complete(0x0c03, 1, 0, &[]), 0)
            .is_empty()
    );
    assert!(bridge.chip_event(&[0x01, 1, 0], 0).is_empty());
    // Nothing reaches the host's controller again; the shadow answers.
    let address = events(bridge.command(&packet(0x1009, &[]), 0));
    assert_eq!(address, [complete(0x1009, 1, 0, &ADDRESS)]);
    // The shadow's procedures end with their events now.
    let inquiry = events(bridge.command(&packet(0x0401, &[0x33, 0x8b, 0x9e, 1, 0]), 0));
    assert_eq!(inquiry, [status(0x0401, 1, 0)]);
    let end = bridge.deadline().expect("the inquiry ends");
    assert_eq!(events(bridge.advance(end)), [vec![0x01, 1, 0]]);
}

#[test]
fn an_error_the_model_would_not_give_falls_back_and_a_procedures_does_not() {
    let mut bridge = Bridge::new(ADDRESS);
    // Inquiry Cancel with no inquiry: the radio's outcome, passed on.
    chip(bridge.command(&packet(0x0402, &[]), 0));
    let answer = events(bridge.chip_event(&complete(0x0402, 1, 0x0c, &[]), 0));
    assert_eq!(answer, [complete(0x0402, 1, 0x0c, &[])]);
    // An error both give is the controller's.
    let bad_mode = packet(0x0c45, &[7]);
    chip(bridge.command(&bad_mode, 0));
    let answer = events(bridge.chip_event(&complete(0x0c45, 1, 0x12, &[]), 0));
    assert_eq!(answer, [complete(0x0c45, 1, 0x12, &[])]);
    assert_eq!(bridge.fallen(), None);
    // Set Event Mask refused where the model takes it.
    let mask = packet(0x0c01, &[0xff; 8]);
    chip(bridge.command(&mask, 0));
    let answer = events(bridge.chip_event(&complete(0x0c01, 1, 0x1f, &[]), 0));
    assert_eq!(answer, [complete(0x0c01, 1, 0, &[])]);
    assert_eq!(
        bridge.fallen(),
        Some(Fallback {
            opcode: Some(0x0c01),
            cause: Cause::Refused(0x1f),
        })
    );
}

#[test]
fn an_answer_of_another_kind_or_length_falls_back() {
    for wrong in [
        // Read BD_ADDR with five bytes.
        complete(0x1009, 1, 0, &[1, 2, 3, 4, 5]),
        // Command Status for a command that completes.
        status(0x1009, 1, 0),
    ] {
        let mut bridge = Bridge::new(ADDRESS);
        chip(bridge.command(&packet(0x1009, &[]), 0));
        let answer = events(bridge.chip_event(&wrong, 0));
        assert_eq!(answer, [complete(0x1009, 1, 0, &ADDRESS)]);
        assert_eq!(bridge.fallen().unwrap().cause, Cause::Malformed);
    }
}

#[test]
fn a_failed_host_controller_falls_back_with_the_answer_in_flight() {
    let mut bridge = Bridge::new(ADDRESS);
    chip(bridge.command(&packet(0x0c14, &[]), 0));
    assert!(bridge.command(&packet(0x0c03, &[]), 0).is_empty());
    let answers = events(bridge.chip_failed(0));
    assert_eq!(
        answers.len(),
        2,
        "the command in flight and the one waiting"
    );
    assert_eq!(answers[0][2..6], [1, 0x14, 0x0c, 0]);
    assert_eq!(answers[1], complete(0x0c03, 1, 0, &[]));
    assert_eq!(bridge.fallen().unwrap().cause, Cause::HostFailed);
    assert!(bridge.chip_failed(0).is_empty());
    // Between commands it falls back all the same.
    let mut bridge = Bridge::new(ADDRESS);
    assert!(bridge.chip_failed(0).is_empty());
    assert_eq!(
        bridge.fallen().unwrap().to_string(),
        "between commands: the host controller failed"
    );
    events(bridge.command(&packet(0x0c03, &[]), 0));
}

/// bluetoothd 26B123's start-up, the guest's side.
const BRING_UP: &str = include_str!("fixtures/26B123-bring-up.hci");

/// The commands the guest sent in a capture.
fn commands(capture: &str) -> Vec<Vec<u8>> {
    use super::super::capture::{Direction, Kind, read};
    read(capture)
        .expect("a capture")
        .into_iter()
        .filter(|p| p.direction == Direction::FromGuest && p.kind == Kind::Command)
        .map(|p| p.bytes)
        .collect()
}

/// One guest command through `bridge`, `chip` answering at once what
/// reaches it, until nothing is left to carry: the guest's events.
fn exchange(bridge: &mut Bridge, chip: &mut Chip, command: &[u8]) -> Vec<Vec<u8>> {
    let mut steps: VecDeque<Step> = bridge.command(command, 0).into();
    let mut events = Vec::new();
    while let Some(step) = steps.pop_front() {
        match step {
            Step::Guest(event) => events.push(event),
            Step::Chip(command) => {
                for event in chip.command(command.bytes()) {
                    steps.extend(bridge.chip_event(&event, 0));
                }
            }
        }
    }
    events
}

/// The return parameters of the one successful Command Complete of `events`.
fn returns(events: &[Vec<u8>]) -> Vec<u8> {
    assert_eq!(events.len(), 1, "{events:02x?}");
    let completion = Completion::of(&events[0]).expect("a completion");
    assert_eq!((completion.code, completion.status), (COMMAND_COMPLETE, 0));
    completion.returns.to_vec()
}

/// The differential replay: bluetoothd's whole start-up through the bridge
/// and a controller that has everything the model lists gets, at every
/// step, the pure model's event, status and return length, and the same
/// returns but for the controller's own address and ACL buffers. Only the
/// listed standard commands reach the controller, in order.
#[test]
fn bluetoothds_start_up_through_the_bridge_answers_as_the_model_does() {
    let mut model = Model::new(ADDRESS);
    let mut bridge = Bridge::new(ADDRESS);
    let mut chip = Chip::new();
    let start_up = commands(BRING_UP);
    assert_eq!(start_up.len(), 46);
    for command in &start_up {
        let op = opcode(command);
        let expected = model.command(command, 0);
        let got = exchange(&mut bridge, &mut chip, command);
        assert_eq!(got.len(), expected.len(), "{op:#06x}");
        for (got, expected) in got.iter().zip(&expected) {
            let (g, e) = (
                Completion::of(got).unwrap(),
                Completion::of(expected).unwrap(),
            );
            assert_eq!(
                (g.code, g.opcode, g.status, g.returns.len()),
                (e.code, e.opcode, e.status, e.returns.len()),
                "{op:#06x}"
            );
            assert_eq!(got[2 + usize::from(g.code == COMMAND_STATUS)], 1);
            match op {
                0x1009 => assert_eq!(g.returns, chip::ADDRESS),
                0x1005 => assert_eq!(g.returns, [0xfd, 0x03, 0, 6, 0, 0, 0]),
                _ => assert_eq!(got, expected, "{op:#06x}"),
            }
        }
    }
    assert_eq!(bridge.fallen(), None);
    assert_eq!(
        chip.opcodes(),
        [
            0x0c03, 0x1002, 0x1009, 0x1003, 0x2003, 0x0c01, 0x2001, 0x0c56, 0x0c6d, 0x1005, 0x0c5b,
            0x0c24, 0x0c23, 0x0c13, 0x0c14, 0x0c18, 0x0c45, 0x0c52,
        ]
    );
    // The module's identity stays: Core 4.0 Broadcom, chip id 73, its IDs.
    let version = returns(&exchange(&mut bridge, &mut chip, &packet(0x1001, &[])));
    assert_eq!(version, [6, 0, 0, 6, 15, 0, 0x0e, 0x41]);
    let verbose = returns(&exchange(&mut bridge, &mut chip, &packet(0xfc79, &[])));
    assert_eq!(verbose[0], 73);
    let product = returns(&exchange(&mut bridge, &mut chip, &packet(0xfc5a, &[])));
    assert_eq!(product, [0xac, 0x05, 0x8f, 0x82]);
    assert_eq!(chip.opcodes().len(), 18);
}

/// The capabilities the guest reads are the model's, less what the
/// controller lacks of the commands it is sent, and never more.
#[test]
fn the_merge_keeps_the_modules_capabilities_less_what_the_controller_lacks() {
    let mut chip = Chip::new();
    // No Write Default Erroneous Data Reporting (octet 18 bit 3) or Write
    // Inquiry Mode (12, 7), both forwarded; no Write Scan Enable (7, 7) or
    // Host Number Of Completed Packets (10, 7), both the shadow's.
    for (octet, bit) in [(18, 3), (12, 7), (7, 7), (10, 7)] {
        chip.commands[octet] &= !(1 << bit);
    }
    // No extended inquiry response (feature bit 48).
    chip.features[6] &= !1;
    // ACL longer than the transport carries.
    chip.buffers[..2].copy_from_slice(&0x2000u16.to_le_bytes());
    let mut bridge = Bridge::new(ADDRESS);
    let model = model::supported_commands();
    let bitmap = returns(&exchange(&mut bridge, &mut chip, &packet(0x1002, &[])));
    for octet in 0..64 {
        let lacking = match octet {
            18 => 1 << 3,
            12 => 1 << 7,
            _ => 0,
        };
        assert_eq!(bitmap[octet], model[octet] & !lacking, "octet {octet}");
    }
    // Features: the model's, less what the controller lacks.
    let features = returns(&exchange(&mut bridge, &mut chip, &packet(0x1003, &[])));
    let expected = returns(&Model::new(ADDRESS).command(&packet(0x1003, &[]), 0));
    assert_ne!(expected[6] & 1, 0, "the model has it");
    assert_eq!(features[6], expected[6] & !1);
    for ((feature, model), chip) in features.iter().zip(&expected).zip(chip.features) {
        assert_eq!(*feature, model & chip);
    }
    // Extended features: the model's page numbers, maximum 1; page 1 as
    // both have it; page 2 the module's, empty, whatever the controller has.
    for page in [0, 1] {
        let extended = returns(&exchange(&mut bridge, &mut chip, &packet(0x1004, &[page])));
        assert_eq!(extended[..2], [page, 1]);
    }
    let page_2 = returns(&exchange(&mut bridge, &mut chip, &packet(0x1004, &[2])));
    assert_eq!(page_2, [2, 1, 0, 0, 0, 0, 0, 0, 0, 0]);
    assert!(
        chip.received
            .iter()
            .all(|c| opcode(c) != 0x1004 || c[3] < 2),
        "later pages stay the module's"
    );
    // Buffers: the controller's ACL buffers, as long as the transport
    // carries at most; no synchronous ones.
    let buffers = returns(&exchange(&mut bridge, &mut chip, &packet(0x1005, &[])));
    assert_eq!(buffers, [0x00, 0x10, 0, 6, 0, 0, 0]);
    // LE features: none in milestone 1, whatever the controller has.
    let le = returns(&exchange(&mut bridge, &mut chip, &packet(0x2003, &[])));
    assert_eq!(le, [0; 8]);
    assert_eq!(bridge.fallen(), None);
}

/// The event filter: vendor events and credits alone go nowhere; inquiry
/// results of each kind, LE meta events, completed packets and the rest
/// reach the guest as they are.
#[test]
fn vendor_events_are_dropped_and_the_radios_events_pass_as_they_are() {
    let mut bridge = Bridge::new(ADDRESS);
    assert!(bridge.chip_event(&[0xff, 3, 0x2a, 1, 2], 0).is_empty());
    assert!(
        bridge
            .chip_event(&[COMMAND_COMPLETE, 3, 1, 0, 0], 0)
            .is_empty()
    );
    let passed = [
        // Inquiry Result, with RSSI, Extended; Inquiry Complete.
        [&[0x02, 15, 1][..], &[7; 14]].concat(),
        [&[0x22, 15, 1][..], &[7; 14]].concat(),
        [&[0x2f, 255, 1][..], &[7; 254]].concat(),
        vec![0x01, 1, 0],
        // Number Of Completed Packets, an LE meta event.
        vec![0x13, 5, 1, 0x40, 0, 1, 0],
        vec![0x3e, 3, 0x0c, 0, 0],
    ];
    for event in &passed {
        assert_eq!(
            events(bridge.chip_event(event, 0)),
            core::slice::from_ref(event)
        );
    }
    assert_eq!(bridge.counters().dropped, 2);
    assert_eq!(bridge.counters().passed, passed.len() as u64);
}

/// A guest bus reset drops the command in flight, and the controller's
/// events stay out until the guest's next HCI Reset is answered: the real
/// controller is never reset by USB, so it may still be busy with the old
/// session.
#[test]
fn a_bus_reset_keeps_the_controllers_events_out_until_the_next_reset() {
    let mut bridge = Bridge::new(ADDRESS);
    events(bridge.command(&packet(0xfc01, &[9, 8, 7, 6, 5, 0x42]), 0));
    chip(bridge.command(&packet(0x0c14, &[]), 0));
    bridge.reset();
    assert_eq!(bridge.address(), ADDRESS, "power-on, as the model");
    let result = [&[0x2f, 255, 1][..], &[7; 254]].concat();
    for stale in [complete(0x0c14, 1, 0, &[0; 248]), result.clone()] {
        assert!(bridge.chip_event(&stale, 0).is_empty());
    }
    // A command before the reset is answered; the reset's answer ends it.
    chip(bridge.command(&packet(0x1009, &[]), 0));
    assert_eq!(
        events(bridge.chip_event(&complete(0x1009, 1, 0, &CHIP_ADDRESS), 0)).len(),
        1
    );
    chip(bridge.command(&packet(0x0c03, &[]), 0));
    assert!(bridge.chip_event(&result, 0).is_empty());
    assert_eq!(
        events(bridge.chip_event(&complete(0x0c03, 1, 0, &[]), 0)).len(),
        1
    );
    assert_eq!(
        events(bridge.chip_event(&result, 0)),
        core::slice::from_ref(&result)
    );
    // A new configuration does the same, keeping the shadow's state.
    events(bridge.command(&packet(0xfc01, &[9, 8, 7, 6, 5, 0x42]), 0));
    bridge.resync();
    assert_eq!(bridge.address(), [9, 8, 7, 6, 5, 0x42]);
    assert!(bridge.chip_event(&result, 0).is_empty());
    assert_eq!(bridge.fallen(), None);
}

/// The shadow follows the controller: a procedure the controller ended or
/// refused ends in the shadow too, so a later fallback gives the guest no
/// second end of it; and the controller's address becomes the shadow's.
#[test]
fn the_shadow_follows_the_controllers_procedures_and_address() {
    let inquiry = packet(0x0401, &[0x33, 0x8b, 0x9e, 8, 0]);
    let mut name = CHIP_ADDRESS.to_vec();
    name.extend_from_slice(&[1, 0, 0, 0]);
    let name = packet(0x0419, &name);
    let mut ended = vec![0x07, 255, 0];
    ended.extend_from_slice(&CHIP_ADDRESS);
    ended.resize(257, b'x');
    for (command, end) in [
        (&inquiry, Some(vec![0x01, 1, 0])),
        (&name, Some(ended)),
        // Refused by the controller: Command Disallowed.
        (&inquiry, None),
        (&name, None),
    ] {
        let mut bridge = Bridge::new(ADDRESS);
        chip(bridge.command(command, 0));
        let op = opcode(command);
        match end {
            Some(end) => {
                events(bridge.chip_event(&status(op, 1, 0), 0));
                assert_eq!(events(bridge.chip_event(&end, 0)), [end]);
            }
            None => {
                let refused = events(bridge.chip_event(&status(op, 1, 0x0c), 0));
                assert_eq!(refused, [status(op, 1, 0x0c)]);
            }
        }
        events(bridge.chip_failed(0));
        assert!(events(bridge.advance(u64::MAX)).is_empty(), "{op:#06x}");
    }
    // The address.
    let mut bridge = Bridge::new(ADDRESS);
    events(bridge.command(&packet(0xfc01, &[9, 8, 7, 6, 5, 0x42]), 0));
    chip(bridge.command(&packet(0x1009, &[]), 0));
    events(bridge.chip_event(&complete(0x1009, 1, 0, &CHIP_ADDRESS), 0));
    assert!(bridge.address_differs(), "the guest wrote another");
    events(bridge.chip_failed(0));
    let address = events(bridge.command(&packet(0x1009, &[]), 0));
    assert_eq!(address, [complete(0x1009, 1, 0, &CHIP_ADDRESS)]);
    let mut bridge = Bridge::new(ADDRESS);
    chip(bridge.command(&packet(0x1009, &[]), 0));
    events(bridge.chip_event(&complete(0x1009, 1, 0, &CHIP_ADDRESS), 0));
    assert!(!bridge.address_differs(), "the guest wrote none");
}

/// In milestone 1 every connection is the shadow's, so ACL data from the
/// guest names none of the controller's: the shadow returns its buffer.
/// The controller's ACL data names none the guest knows, so it is dropped,
/// whether the bridge forwards, keeps the controller out or fell back.
#[test]
fn acl_from_the_guest_is_the_shadows_and_the_controllers_is_dropped() {
    let mut bridge = Bridge::new(ADDRESS);
    assert_eq!(
        events(bridge.acl(&[0x40, 0x20, 2, 0, 9, 9])),
        [vec![0x13, 5, 1, 0x40, 0, 1, 0]]
    );
    assert!(!bridge.chip_acl());
    bridge.resync();
    assert!(!bridge.chip_acl());
    let mut bridge = Bridge::new(ADDRESS);
    chip(bridge.command(&packet(0x0c03, &[]), 0));
    events(bridge.chip_event(&complete(0x0c03, 1, 0, &[]), 0));
    assert!(!bridge.chip_acl(), "after the reset is answered too");
    events(bridge.chip_failed(0));
    assert!(!bridge.chip_acl());
    assert_eq!(bridge.counters().dropped, 2);
    assert_eq!(bridge.counters().passed, 1, "the reset's answer alone");
}

/// A Hardware Error says the controller's firmware failed: the bridge falls
/// back, the command in flight gets the shadow's answer and the procedures
/// the controller ran end, and the guest, whose controller the model now
/// is, never sees the error.
#[test]
fn a_hardware_error_falls_back_with_the_answer_in_flight() {
    let mut bridge = Bridge::new(ADDRESS);
    chip(bridge.command(&packet(0x0401, &[0x33, 0x8b, 0x9e, 8, 0]), 0));
    events(bridge.chip_event(&status(0x0401, 1, 0), 0));
    chip(bridge.command(&packet(0x0c14, &[]), 0));
    let steps = events(bridge.chip_event(&[0x10, 1, 0x2a], 0));
    assert_eq!(steps.len(), 2, "{steps:02x?}");
    assert_eq!(steps[0][..6], [COMMAND_COMPLETE, 252, 1, 0x14, 0x0c, 0]);
    assert_eq!(steps[1], [0x01, 1, 0], "the inquiry ends");
    assert!(steps.iter().all(|e| e[0] != 0x10));
    let fallback = bridge.fallen().expect("fell back");
    assert_eq!(
        (fallback.opcode, fallback.cause),
        (Some(0x0c14), Cause::HardwareError(0x2a))
    );
    assert_eq!(
        fallback.to_string(),
        "0x0c14 the host controller reported Hardware Error 0x2a"
    );
    // Nothing reaches the controller again.
    events(bridge.command(&packet(0x0c03, &[]), 0));
    // Between commands, and while the controller is kept out, alike.
    let mut bridge = Bridge::new(ADDRESS);
    bridge.resync();
    assert!(events(bridge.chip_event(&[0x10, 1, 0], 0)).is_empty());
    assert_eq!(
        bridge.fallen().map(|f| (f.opcode, f.cause)),
        Some((None, Cause::HardwareError(0)))
    );
}

/// The log's words for the deadline and the restarts are the constants'.
#[test]
fn the_causes_name_the_deadline_and_restarts_the_bridge_uses() {
    assert_eq!(DEADLINE_MS * FREQUENCY_HZ, DEADLINE * 1000);
    assert_eq!(
        Cause::Timeout.to_string(),
        alloc::format!("no answer in {DEADLINE_MS} ms")
    );
    assert!(Cause::Restarted.to_string().contains(&alloc::format!(
        "controller {RESTARTS} times in a row, each within {SETTLED_S} s"
    )));
    assert_eq!(SETTLED_S * FREQUENCY_HZ, SETTLED);
}

/// A fallback ends what the controller was running: the guest saw the
/// procedure start, and the shadow's own end may already have passed while
/// the bridge forwarded. Each ends exactly once.
#[test]
fn a_fallback_ends_the_procedures_the_controller_was_running() {
    // An inquiry the controller runs past the shadow's end.
    let mut bridge = Bridge::new(ADDRESS);
    chip(bridge.command(&packet(0x0401, &[0x33, 0x8b, 0x9e, 1, 0]), 0));
    events(bridge.chip_event(&status(0x0401, 1, 0), 10));
    let shadow_end = bridge.shadow.deadline().expect("the shadow's inquiry");
    assert!(events(bridge.advance(shadow_end)).is_empty(), "discarded");
    let ended = events(bridge.chip_failed(shadow_end + 1));
    assert_eq!(ended, [vec![0x01, 1, 0]]);
    assert!(events(bridge.advance(u64::MAX)).is_empty(), "once");
    // A name request, ended with Page Timeout, and an inquiry the shadow
    // still runs, ended once, now.
    let mut bridge = Bridge::new(ADDRESS);
    let mut name = CHIP_ADDRESS.to_vec();
    name.extend_from_slice(&[1, 0, 0, 0]);
    chip(bridge.command(&packet(0x0419, &name), 0));
    events(bridge.chip_event(&status(0x0419, 1, 0), 0));
    chip(bridge.command(&packet(0x0401, &[0x33, 0x8b, 0x9e, 8, 0]), 0));
    events(bridge.chip_event(&status(0x0401, 1, 0), 0));
    let mut timeout = vec![0x07, 255, 0x04];
    timeout.extend_from_slice(&CHIP_ADDRESS);
    timeout.resize(257, 0);
    let ended = events(bridge.chip_failed(0));
    assert_eq!(ended, [timeout, vec![0x01, 1, 0]]);
    assert!(events(bridge.advance(u64::MAX)).is_empty(), "once");
    // Procedures the controller ended, cancelled or refused end nothing.
    let mut bridge = Bridge::new(ADDRESS);
    chip(bridge.command(&packet(0x0401, &[0x33, 0x8b, 0x9e, 8, 0]), 0));
    events(bridge.chip_event(&status(0x0401, 1, 0), 0));
    chip(bridge.command(&packet(0x0402, &[]), 0));
    events(bridge.chip_event(&complete(0x0402, 1, 0, &[]), 0));
    chip(bridge.command(&packet(0x0419, &name), 0));
    events(bridge.chip_event(&status(0x0419, 1, 0x0c), 0));
    assert!(events(bridge.chip_failed(0)).is_empty());
    assert!(events(bridge.advance(u64::MAX)).is_empty());
    let mut bridge = Bridge::new(ADDRESS);
    chip(bridge.command(&packet(0x0401, &[0x33, 0x8b, 0x9e, 8, 0]), 0));
    events(bridge.chip_event(&status(0x0401, 1, 0), 0));
    events(bridge.chip_event(&[0x01, 1, 0], 0));
    chip(bridge.command(&packet(0x0419, &name), 0));
    events(bridge.chip_event(&status(0x0419, 1, 0), 0));
    let mut named = vec![0x07, 255, 0];
    named.extend_from_slice(&CHIP_ADDRESS);
    named.resize(257, b'x');
    events(bridge.chip_event(&named, 0));
    assert!(events(bridge.chip_failed(0)).is_empty());
    assert!(events(bridge.advance(u64::MAX)).is_empty());
}

/// An HCI Reset that falls back, by its deadline or refused, ends the
/// controller's procedures with no event, as every reset does: the guest
/// gets the reset's answer and no Inquiry Complete or Page Timeout after it.
#[test]
fn a_fallback_at_an_hci_reset_ends_the_procedures_with_no_event() {
    let mut name = CHIP_ADDRESS.to_vec();
    name.extend_from_slice(&[1, 0, 0, 0]);
    let reset = packet(0x0c03, &[]);
    for refused in [false, true] {
        let mut bridge = Bridge::new(ADDRESS);
        chip(bridge.command(&packet(0x0401, &[0x33, 0x8b, 0x9e, 8, 0]), 0));
        events(bridge.chip_event(&status(0x0401, 1, 0), 0));
        chip(bridge.command(&packet(0x0419, &name), 0));
        events(bridge.chip_event(&status(0x0419, 1, 0), 0));
        assert_eq!(chip(bridge.command(&reset, 10)), reset);
        let answer = if refused {
            events(bridge.chip_event(&complete(0x0c03, 1, 0x03, &[]), 20))
        } else {
            events(bridge.advance(10 + DEADLINE))
        };
        assert_eq!(answer, [complete(0x0c03, 1, 0, &[])], "refused: {refused}");
        let cause = if refused {
            Cause::Refused(0x03)
        } else {
            Cause::Timeout
        };
        assert_eq!(bridge.fallen().map(|f| f.cause), Some(cause));
        assert!(
            events(bridge.advance(u64::MAX)).is_empty(),
            "refused: {refused}"
        );
    }
}

/// bluetoothd restarts, and resets its controller, when it rejects even a
/// well-formed answer (reason 108), and Setup's first screen waits for it:
/// a second restart after the start-up falls back, so the model answers it.
#[test]
fn bluetoothd_restarting_twice_after_its_start_up_falls_back() {
    let mut bridge = Bridge::new(ADDRESS);
    let reset = packet(0x0c03, &[]);
    // The start-up's reset, then one restart: both the controller's.
    for _ in 0..2 {
        chip(bridge.command(&reset, 0));
        events(bridge.chip_event(&complete(0x0c03, 1, 0, &[]), 0));
    }
    assert_eq!((bridge.fallen(), bridge.counters().restarts), (None, 1));
    // The second restart: the model answers it and everything after.
    assert_eq!(
        events(bridge.command(&reset, 0)),
        [complete(0x0c03, 1, 0, &[])]
    );
    let fallback = bridge.fallen().expect("fell back");
    assert_eq!(
        (fallback.opcode, fallback.cause),
        (Some(0x0c03), Cause::Restarted)
    );
    assert_eq!(
        fallback.to_string(),
        "0x0c03 bluetoothd reset the controller 2 times in a row, each within 60 s \
         of its start, as it does when it rejects the bring-up"
    );
    assert_eq!(
        (bridge.counters().restarts, bridge.counters().streak),
        (2, 2)
    );
    events(bridge.command(&packet(0x1009, &[]), 0));
    // Resets before the controller answered one are the start-up's own.
    let mut bridge = Bridge::new(ADDRESS);
    for _ in 0..3 {
        chip(bridge.command(&reset, 0));
        bridge.resync();
        // Its late answer, stale now, gives the controller's credit back.
        assert!(
            bridge
                .chip_event(&complete(0x0c03, 1, 0, &[]), 0)
                .is_empty()
        );
    }
    assert_eq!((bridge.fallen(), bridge.counters().restarts), (None, 0));
}

/// Bluetooth turned off and on, or a sleep and wake, resets the controller
/// once after a session that ran: each starts the count of restarts again,
/// so any number of them keep the lend, while two quick restarts after one
/// still fall back.
#[test]
fn a_reset_after_a_settled_session_starts_the_count_again() {
    let mut bridge = Bridge::new(ADDRESS);
    let reset = packet(0x0c03, &[]);
    let mut now = 0;
    // The start-up's reset.
    chip(bridge.command(&reset, now));
    events(bridge.chip_event(&complete(0x0c03, 1, 0, &[]), now));
    // Five off/on cycles, each a minute or more after the last start.
    for cycle in 1..=5 {
        now += SETTLED;
        assert_eq!(chip(bridge.command(&reset, now)), reset, "cycle {cycle}");
        events(bridge.chip_event(&complete(0x0c03, 1, 0, &[]), now));
        assert_eq!(bridge.fallen(), None, "cycle {cycle}");
        assert_eq!(
            (bridge.counters().restarts, bridge.counters().streak),
            (cycle, 1)
        );
    }
    // Then bluetoothd rejects the bring-up and restarts within the minute.
    now += SETTLED - 1;
    assert_eq!(
        events(bridge.command(&reset, now)),
        [complete(0x0c03, 1, 0, &[])]
    );
    assert_eq!(
        bridge.fallen().map(|f| f.cause),
        Some(Cause::Restarted),
        "the model answers the second in a row"
    );
    assert_eq!(bridge.counters().streak, RESTARTS);
}

/// The controller's own command credits: a forwarded command waits until
/// the controller has one, and falls back at its deadline if none comes.
#[test]
fn a_command_waits_for_the_controllers_credit() {
    let mut bridge = Bridge::new(ADDRESS);
    chip(bridge.command(&packet(0x0c03, &[]), 0));
    // Answered with no credit left; the guest still sees one.
    let answer = events(bridge.chip_event(&complete(0x0c03, 0, 0, &[]), 0));
    assert_eq!(answer, [complete(0x0c03, 1, 0, &[])]);
    let mask = packet(0x0c01, &[0xff; 8]);
    assert!(bridge.command(&mask, 0).is_empty(), "held for a credit");
    assert!(bridge.busy());
    // Credits alone give one back: the command goes.
    assert_eq!(
        chip(bridge.chip_event(&[COMMAND_COMPLETE, 3, 1, 0, 0], 0)),
        mask
    );
    events(bridge.chip_event(&complete(0x0c01, 0, 0, &[]), 0));
    // A Command Status gives credits too.
    let address = packet(0x1009, &[]);
    assert!(bridge.command(&address, 0).is_empty());
    assert_eq!(chip(bridge.chip_event(&status(0x0000, 1, 0), 0)), address);
    events(bridge.chip_event(&complete(0x1009, 0, 0, &CHIP_ADDRESS), 0));
    // None comes: the deadline falls back.
    assert!(bridge.command(&packet(0x0c14, &[]), 10).is_empty());
    let answer = events(bridge.advance(10 + DEADLINE));
    assert_eq!(answer[0][3..6], [0x14, 0x0c, 0]);
    assert_eq!(bridge.fallen().unwrap().cause, Cause::Timeout);
}
