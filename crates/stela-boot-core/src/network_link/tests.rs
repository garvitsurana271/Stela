use super::*;
use alloc::{string::ToString, vec, vec::Vec};
use core::cell::{Cell, RefCell};
use stela_drivers::net::wifi::realtek::ax::WIFI_CARRIER_WAIT_US;
use stela_runtime::net::{ClockError, NetError, stack::DHCP_WAIT_US};

/// A keeper's failure as the network stack reports it.
fn not_found() -> NetError {
    NetError::Link(String::from("rtw89: <ssid> not found"))
}

/// `address` over `results`, one per attempt, in order: what it gave, what
/// it said of the failures it went past, and how many attempts it made.
fn drive(cut: &Cut, results: Vec<Result<u32, NetError>>) -> (Result<u32, NetError>, String, usize) {
    let results = RefCell::new(results.into_iter());
    let attempts = Cell::new(0);
    let (result, retried) = address(cut, || {
        attempts.set(attempts.get() + 1);
        results
            .borrow_mut()
            .next()
            .expect("an attempt past the last result")
    });
    (result, retried.to_string(), attempts.get())
}

/// A wired adapter's wait ends at its first failure, whatever it is, as it
/// did before a Wi-Fi link's went on, and its clock is never cut.
#[test]
fn a_wired_link_stops_at_its_first_failure() {
    for failure in [
        not_found(),
        NetError::NoCarrier,
        NetError::Dhcp("no DHCPv4 lease"),
        NetError::DuplicateAddress(core::net::Ipv6Addr::LOCALHOST),
    ] {
        let cut = Cut::new(false, WIFI_CARRIER_WAIT_US);
        let (result, retried, attempts) = drive(&cut, vec![Err(failure.clone()), Ok(1)]);
        assert_eq!(result, Err(failure));
        assert_eq!((retried.as_str(), attempts), ("", 1));
        assert!(!cut.passed(0) && !cut.passed(u64::MAX) && !cut.reached());
    }
    let cut = Cut::new(false, WIFI_CARRIER_WAIT_US);
    assert_eq!(drive(&cut, vec![Ok(7)]), (Ok(7), String::new(), 1));
}

/// A Wi-Fi link's keeper fails and tries again on its own, and its failure
/// reaches the stack as the link's: the wait goes on past it.
#[test]
fn a_wifi_link_is_driven_past_its_keepers_failures() {
    let cut = Cut::new(true, WIFI_CARRIER_WAIT_US);
    let (result, retried, attempts) = drive(&cut, vec![Err(not_found()), Ok(7)]);
    assert_eq!((result, attempts), (Ok(7), 2));
    assert_eq!(
        retried,
        "retried past 1 failure: network link: rtw89: <ssid> not found"
    );
    // The wait is over: the stack's later reads, a download's, are never
    // cut.
    assert!(!cut.passed(u64::MAX) && !cut.reached());

    let cut = Cut::new(true, WIFI_CARRIER_WAIT_US);
    let (result, retried, attempts) = drive(
        &cut,
        vec![
            Err(not_found()),
            Err(NetError::NoCarrier),
            Err(NetError::Dhcp("no lease")),
            Ok(7),
        ],
    );
    assert_eq!((result, attempts), (Ok(7), 4));
    assert_eq!(
        retried,
        "retried past 3 failures, first: network link: rtw89: <ssid> not found, last: DHCP: \
         no lease"
    );
    let cut = Cut::new(true, WIFI_CARRIER_WAIT_US);
    let (_, retried, _) = drive(&cut, vec![Err(not_found()), Err(not_found()), Ok(7)]);
    assert_eq!(
        retried,
        "retried past 2 failures: network link: rtw89: <ssid> not found"
    );
}

/// A failure no other attempt mends ends a Wi-Fi link's wait at once, as a
/// wired one's, and so does a clock that failed before the cut.
#[test]
fn a_wifi_link_stops_at_a_failure_no_attempt_mends() {
    for failure in [
        NetError::DuplicateAddress(core::net::Ipv6Addr::LOCALHOST),
        NetError::Invalid("DHCP after a configuration by hand"),
        NetError::Clock(ClockError::Unavailable),
    ] {
        let cut = Cut::new(true, WIFI_CARRIER_WAIT_US);
        let (result, retried, attempts) = drive(&cut, vec![Err(not_found()), Err(failure.clone())]);
        assert_eq!((result, attempts), (Err(failure), 2));
        assert_eq!(
            retried,
            "retried past 1 failure: network link: rtw89: <ssid> not found"
        );
        assert!(!cut.passed(u64::MAX));
    }
}

/// A Wi-Fi link's wait lasts its carrier wait and then the DHCP wait, the
/// stack's, from the stack's first read of the clock; a read at or past
/// that fails from then on.
#[test]
fn a_wifi_links_wait_is_its_carrier_wait_and_the_dhcp_wait() {
    assert_eq!(DHCP_WAIT_US, 60_000_000);
    let cut = Cut::new(true, WIFI_CARRIER_WAIT_US);
    let start = 5_000_000;
    let end = start + WIFI_CARRIER_WAIT_US + DHCP_WAIT_US;
    assert!(!cut.passed(start));
    assert!(!cut.passed(end - 1));
    assert!(!cut.reached());
    assert!(cut.passed(end));
    assert!(cut.reached());
    // Once cut, every read is: the stack refuses from then on.
    assert!(cut.passed(start));
}

/// The stack's clock cut drives a Wi-Fi link's wait to its end: the
/// failure is the last one gone past, so a keeper that never joined still
/// reads as no carrier.
#[test]
fn a_wifi_links_wait_ends_at_its_cut() {
    let cut = Cut::new(true, WIFI_CARRIER_WAIT_US);
    let now = Cell::new(1_000);
    let attempts = Cell::new(0);
    // Each attempt reads the clock as it starts, then waits out the carrier
    // wait without carrier.
    let (result, retried) = address(&cut, || {
        attempts.set(attempts.get() + 1);
        if cut.passed(now.get()) {
            return Err::<u32, _>(NetError::Clock(ClockError::Unavailable));
        }
        now.set(now.get() + WIFI_CARRIER_WAIT_US);
        Err(NetError::NoCarrier)
    });
    assert_eq!((result, attempts.get()), (Err(NetError::NoCarrier), 4));
    assert_eq!(
        retried.to_string(),
        "retried past 3 failures: network link has no carrier; no address within 90 s"
    );
    assert!(!cut.passed(u64::MAX));

    // Cut inside its first attempt, with no failure gone past: a lease that
    // did not come.
    let cut = Cut::new(true, WIFI_CARRIER_WAIT_US);
    assert!(!cut.passed(0));
    assert!(cut.passed(WIFI_CARRIER_WAIT_US + DHCP_WAIT_US));
    let (result, retried, attempts) =
        drive(&cut, vec![Err(NetError::Clock(ClockError::Unavailable))]);
    assert_eq!(attempts, 1);
    assert_eq!(result, Err(NetError::Dhcp(NO_LEASE_IN_TIME)));
    assert_eq!(retried, "no address within 90 s");
}

#[test]
fn hide_ssid_hides_every_occurrence_of_the_name() {
    // The connection keeper's failures name the network after the radio.
    assert_eq!(
        hide_ssid("rtw89: test not found", b"test"),
        "rtw89: <ssid> not found"
    );
    assert_eq!(
        hide_ssid(
            "rtw89: test offers no supported security: Akm; test",
            b"test"
        ),
        "rtw89: <ssid> offers no supported security: Akm; <ssid>"
    );
    // Byte for byte: another case, or part of the name, is not the name.
    for text in ["rtw89: TEST not found", "rtw89: tes not found", ""] {
        assert_eq!(hide_ssid(text, b"test"), text);
    }
    // A name of several-byte characters, and one that is not UTF-8, which
    // the keeper writes as from_utf8_lossy does.
    assert_eq!(
        hide_ssid("rtw89: café ü not found", "café ü".as_bytes()),
        "rtw89: <ssid> not found"
    );
    assert_eq!(
        hide_ssid("rtw89: caf\u{fffd} not found", b"caf\xe9"),
        "rtw89: <ssid> not found"
    );
    // An empty name, a wired adapter's, hides nothing.
    assert_eq!(hide_ssid("rtw89:  not found", b""), "rtw89:  not found");
}

#[test]
fn hide_ssid_hides_a_name_inside_other_words_too() {
    // The line gives up legibility, never the name.
    assert_eq!(
        hide_ssid("eapol rx 2; scan rx", b"rx"),
        "eapol <ssid> 2; scan <ssid>"
    );
    assert_eq!(
        hide_ssid("scans 1, assoc", b"s"),
        "<ssid>can<ssid> 1, a<ssid><ssid>oc"
    );
    // Overlapping occurrences are hidden as one, touching ones one each.
    assert_eq!(hide_ssid("x aaa y", b"aa"), "x <ssid> y");
    assert_eq!(hide_ssid("ababa", b"aba"), "<ssid>");
    assert_eq!(hide_ssid("abab", b"ab"), "<ssid><ssid>");
}

#[test]
fn hide_ssid_leaves_a_hidden_name_hidden() {
    // A `<ssid>` already written stays as it is, so hiding twice changes
    // nothing, even for a name that is part of it or is it.
    let text = "rtw89: test not found; <ssid>";
    for name in ["test", "s", "ssid", "id>", "<", "<ssid>"] {
        let once = hide_ssid(text, name.as_bytes());
        assert!(once.ends_with("; <ssid>"), "{name}: {once}");
        assert_eq!(hide_ssid(&once, name.as_bytes()), once, "{name}");
    }
    // A name longer than the placeholder is still hidden over it.
    assert_eq!(hide_ssid("<ssid> <ssid>x", b"<ssid>x"), "<ssid> <ssid>");
}

/// The boot's `NETWORK adapter … failed` line and its account on the
/// screen write the network stack's error, which carries the keeper's
/// failure as the link's.
#[test]
fn a_keeper_failure_reaches_the_boot_with_the_name_hidden() {
    let failure = hide_ssid("rtw89: test not found", b"test");
    let error = NetError::Link(failure);
    assert_eq!(error.to_string(), "network link: rtw89: <ssid> not found");
    assert_eq!(
        alloc::format!("0000:03:00.0: no address ({error})."),
        "0000:03:00.0: no address (network link: rtw89: <ssid> not found)."
    );
    assert!(!error.to_string().contains("test"));
}

/// macOS's network takes an adapter with carrier first; with none, the
/// configured Wi-Fi adapter, not an empty Ethernet port probed before it;
/// without a configured network, the first adapter as before.
#[test]
fn the_guest_takes_the_configured_wifi_over_an_empty_ethernet_port() {
    // Ethernet (no cable) then Wi-Fi, a network configured: Wi-Fi.
    assert_eq!(guest_adapter(&[false, false], &[false, true], true), 1);
    // No network configured: the first adapter, as before.
    assert_eq!(guest_adapter(&[false, false], &[false, true], false), 0);
    // A cable in the Ethernet port: carrier wins over Wi-Fi.
    assert_eq!(guest_adapter(&[true, false], &[false, true], true), 0);
    // Carrier on a later adapter wins over the configured Wi-Fi.
    assert_eq!(
        guest_adapter(&[false, false, true], &[false, true, false], true),
        2
    );
    // No Wi-Fi adapter at all: the first adapter.
    assert_eq!(guest_adapter(&[false, false], &[false, false], true), 0);
    // Wi-Fi alone: itself.
    assert_eq!(guest_adapter(&[false], &[true], true), 0);
}

/// macOS's network leaves a host adapter without carrier for a spare that
/// has one, once the first has been down for a while and the spare's link
/// has held: a cable in a USB adapter whose PHY was still negotiating when
/// the guest chose the empty built-in port is followed.
#[test]
fn the_guest_moves_to_a_spare_whose_link_held_once_its_own_is_gone() {
    // Its own link up: no move, whatever the spares say.
    assert_eq!(guest_switch(10_000, None, &[Some(0)]), None);
    // Down for less than the wait: no move yet.
    assert_eq!(
        guest_switch(10_000, Some(10_000 - SWITCH_AFTER_MS + 1), &[Some(0)]),
        None
    );
    // Down for the wait, the spare up long enough: the spare.
    assert_eq!(
        guest_switch(10_000, Some(10_000 - SWITCH_AFTER_MS), &[Some(0)]),
        Some(0)
    );
    // A spare that only just came up (a blinking port) is passed over for a
    // later one whose link held.
    assert_eq!(
        guest_switch(10_000, Some(0), &[Some(9_900), None, Some(8_000)]),
        Some(2)
    );
    // No spare with a held link: none.
    assert_eq!(guest_switch(10_000, Some(0), &[None, Some(9_999)]), None);
    assert_eq!(guest_switch(10_000, Some(0), &[]), None);
}

/// A wired link is recorded for the next boot's menu only once it held: the
/// 107 ms and 410 ms links of a 0.6.0 IdeaPad log are not a wired network.
#[test]
fn a_wired_link_counts_once_it_held() {
    assert!(!steady(None, 50_000, WIRED_STEADY_MS));
    assert!(!steady(Some(1_000), 1_107, WIRED_STEADY_MS));
    assert!(!steady(Some(1_000), 1_410, WIRED_STEADY_MS));
    assert!(steady(
        Some(1_000),
        1_000 + WIRED_STEADY_MS,
        WIRED_STEADY_MS
    ));
    // A clock read before the start counts as no time held.
    assert!(!steady(Some(5_000), 1_000, 1));
}

/// An adapter without a usable station address is passed over, keeping the
/// order the guest's choice follows; none at all when no adapter has one.
#[test]
fn the_guest_adapter_is_one_with_a_usable_station_address() {
    // A laptop's I219 with no cable and an AX200 whose firmware has not
    // named its address: the I219, where 0.6.1 took the AX200 and stopped.
    let (carriers, wireless) = ([false, false], [false, true]);
    assert_eq!(guest_adapter(&carriers, &wireless, true), 1);
    assert_eq!(
        usable_guest_adapter(&carriers, &wireless, &[true, false], true),
        Some(0)
    );
    // With its address the Wi-Fi chip is taken, as before.
    assert_eq!(
        usable_guest_adapter(&carriers, &wireless, &[true, true], true),
        Some(1)
    );
    assert_eq!(
        usable_guest_adapter(&carriers, &wireless, &[true, true], false),
        Some(0)
    );
    // A carrier still comes first among the usable, and the indices are
    // the adapters' own.
    assert_eq!(
        usable_guest_adapter(
            &[true, false, true],
            &[false; 3],
            &[false, true, true],
            false
        ),
        Some(2)
    );
    assert_eq!(
        usable_guest_adapter(
            &[false, false, false],
            &[false, true, true],
            &[true, false, true],
            true
        ),
        Some(2)
    );
    assert_eq!(
        usable_guest_adapter(&carriers, &wireless, &[false, false], true),
        None
    );
    assert_eq!(usable_guest_adapter(&[], &[], &[], true), None);
}

/// An adapter that raises no receive signal (the I219 of a 0.6.1 log whose
/// line read "stays: no MSI-X entry for processor apic=0xe" while 0xe ran
/// one of the boot's PEs) is polled: its driver is never asked to move a
/// signal, and the line says it polls rather than naming a missing entry.
#[test]
fn an_adapter_without_a_receive_signal_is_polled_and_never_retargeted() {
    let asked = Cell::new(false);
    let signal = receive_signal(false, || {
        asked.set(true);
        true
    });
    assert_eq!(signal, ReceiveSignal::Polled);
    assert!(!asked.get());
    let line = receive_signal_line(signal, "0000:00:1f.6 Intel I219", 0xe, 0x63);
    assert_eq!(
        line,
        "GUEST NETWORK receive polled: host adapter 0000:00:1f.6 Intel I219 raises no receive \
         signal, its frames are read at each host service turn\n"
    );
    assert!(!line.contains("MSI-X"));
    assert!(!line.contains("apic"));
}

/// An adapter that signals is asked to move its signal once: moved, the
/// line keeps 0.6.1's wording; refused, it names the driver, not an entry.
#[test]
fn a_signalling_adapter_is_moved_or_kept_as_its_driver_answers() {
    let asked = Cell::new(0);
    let moved = receive_signal(true, || {
        asked.set(asked.get() + 1);
        true
    });
    assert_eq!((moved, asked.get()), (ReceiveSignal::Moved, 1));
    assert_eq!(
        receive_signal_line(moved, "0000:00:03.0", 0x2, 0x63),
        "GUEST NETWORK receive signal moved to processor apic=0x2 vector=0x63\n"
    );
    let kept = receive_signal(true, || {
        asked.set(asked.get() + 1);
        false
    });
    assert_eq!((kept, asked.get()), (ReceiveSignal::Kept, 2));
    assert_eq!(
        receive_signal_line(kept, "0000:02:00.0 RTL8168H/RTL8111H XID 541", 0xc, 0x63),
        "GUEST NETWORK receive signal stays where host adapter 0000:02:00.0 RTL8168H/RTL8111H \
         XID 541's driver set it: the driver cannot move it to processor apic=0xc vector=0x63\n"
    );
}
