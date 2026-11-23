use super::*;
use crate::wifi_page::Draw;
use alloc::vec;

/// A 1280 by 720 screen, the startup menu's header above row 112.
const SCREEN: Geometry = Geometry {
    width: 1280,
    height: 720,
    top: 112,
    glyph: 8,
};

fn bss(ssid: &str, signal: i8, kind: Kind) -> Nearby {
    Nearby {
        ssid: ssid.as_bytes().to_vec(),
        signal_dbm: Some(signal),
        kind,
    }
}

/// `cafe` strong, `home` (saved) weaker, `lab` weakest; one open and one
/// enterprise network beside them.
fn nearby() -> Vec<Nearby> {
    vec![
        bss("lab", -80, Kind::Joinable),
        bss("home", -70, Kind::Joinable),
        bss("cafe", -50, Kind::Joinable),
        bss("airport", -40, Kind::Open),
        bss("corp", -60, Kind::Unsupported),
    ]
}

fn picker() -> Picker {
    Picker::new(scan(&nearby(), &[b"home".to_vec()]), true, 0)
}

fn names(picker: &Picker) -> Vec<&str> {
    picker
        .choices()
        .iter()
        .map(|choice| choice.name.as_str())
        .collect()
}

fn typed(picker: &mut Picker, text: &str) {
    for character in text.chars() {
        assert_eq!(picker.press(Key::Character(character), 0), Action::Stay);
    }
}

fn has_text(scene: &Scene, wanted: &str) -> bool {
    scene.texts().any(|text| text.contains(wanted))
}

/// The picker at `cafe`'s password dialog.
fn asking() -> Picker {
    let mut picker = picker();
    assert_eq!(picker.press(Key::Enter, 0), Action::Stay);
    assert!(matches!(picker.mode(), Mode::Password(_)));
    picker
}

fn password(picker: &Picker) -> &Password {
    match picker.mode() {
        Mode::Password(password) => password,
        _ => panic!("the password dialog is not open"),
    }
}

// The scan.

#[test]
fn bars_step_at_the_thresholds_phones_use() {
    let steps = [
        (Some(-30), 4),
        (Some(-55), 4),
        (Some(-56), 3),
        (Some(-67), 3),
        (Some(-68), 2),
        (Some(-75), 2),
        (Some(-76), 1),
        (Some(-100), 1),
        (None, 1),
    ];
    for (signal, bars_drawn) in steps {
        assert_eq!(bars(signal), bars_drawn, "{signal:?}");
    }
}

#[test]
fn the_scan_lists_joinable_networks_strongest_first_and_counts_the_rest() {
    let scan = scan(&nearby(), &[b"home".to_vec()]);
    let rows: Vec<(&str, u8, bool)> = scan
        .choices
        .iter()
        .map(|choice| (choice.name.as_str(), choice.bars, choice.saved))
        .collect();
    assert_eq!(
        rows,
        [("cafe", 4, false), ("home", 2, true), ("lab", 1, false)]
    );
    assert_eq!(scan.unjoinable, 2);
}

#[test]
fn each_network_is_listed_once_at_its_strongest_joinable_bss() {
    let nearby = vec![
        bss("home", -80, Kind::Joinable),
        bss("home", -50, Kind::Joinable),
        // A stronger BSS of it that cannot be joined does not count.
        bss("home", -30, Kind::Unsupported),
        // A network with one joinable BSS among others is joinable.
        bss("mixed", -40, Kind::Open),
        bss("mixed", -90, Kind::Joinable),
    ];
    let scan = scan(&nearby, &[]);
    let rows: Vec<(&str, u8)> = scan
        .choices
        .iter()
        .map(|choice| (choice.name.as_str(), choice.bars))
        .collect();
    assert_eq!(rows, [("home", 4), ("mixed", 1)]);
    assert_eq!(scan.unjoinable, 0);
}

#[test]
fn hidden_networks_are_left_out_and_ties_go_by_name() {
    let nearby = vec![
        bss("", -40, Kind::Joinable),
        Nearby {
            ssid: vec![0; 8],
            signal_dbm: Some(-40),
            kind: Kind::Joinable,
        },
        bss("b-net", -60, Kind::Joinable),
        bss("a-net", -60, Kind::Joinable),
        Nearby {
            ssid: b"quiet".to_vec(),
            signal_dbm: None,
            kind: Kind::Joinable,
        },
    ];
    let scan = scan(&nearby, &[]);
    let listed: Vec<&str> = scan.choices.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(listed, ["a-net", "b-net", "quiet"]);
    assert_eq!(scan.unjoinable, 0);
}

#[test]
fn at_most_the_strongest_networks_are_listed() {
    let nearby: Vec<Nearby> = (0..MAX_CHOICES + 8)
        .map(|n| bss(&format!("net-{n:02}"), -30 - n as i8, Kind::Joinable))
        .collect();
    let scan = scan(&nearby, &[]);
    assert_eq!(scan.choices.len(), MAX_CHOICES);
    assert_eq!(scan.choices[0].name, "net-00");
    assert_eq!(
        scan.choices[MAX_CHOICES - 1].name,
        format!("net-{:02}", MAX_CHOICES - 1)
    );
}

#[test]
fn a_name_that_is_not_text_is_drawn_with_question_marks_and_joined_by_its_bytes() {
    let nearby = vec![Nearby {
        ssid: vec![b'a', 0xff, b'\n', b'b'],
        signal_dbm: Some(-50),
        kind: Kind::Joinable,
    }];
    let mut picker = Picker::new(scan(&nearby, &[]), false, 0);
    assert_eq!(names(&picker), ["a??b"]);
    picker.press(Key::Enter, 0);
    typed(&mut picker, "password1");
    let Action::Join(request) = picker.press(Key::Enter, 0) else {
        panic!("no join");
    };
    assert_eq!(request.ssid, vec![b'a', 0xff, b'\n', b'b']);
}

// The gate.

fn facts() -> Facts {
    Facts {
        adapter: true,
        screen: true,
        keyboard: true,
        scanned: true,
        in_range: false,
        held: false,
    }
}

#[test]
fn the_picker_shows_only_when_no_network_of_the_boots_own_is_in_range() {
    assert_eq!(decide(facts()), Gate::Show(Shown::NoneInRange));
    assert_eq!(
        decide(Facts {
            in_range: true,
            ..facts()
        }),
        Gate::Skip(Skipped::InRange)
    );
    assert_eq!(
        decide(Facts {
            scanned: false,
            ..facts()
        }),
        Gate::Skip(Skipped::NoScan)
    );
}

#[test]
fn option_held_shows_it_even_in_range_but_never_without_a_keyboard_or_screen() {
    let held = Facts {
        held: true,
        in_range: true,
        scanned: false,
        ..facts()
    };
    assert_eq!(decide(held), Gate::Show(Shown::Asked));
    for (facts, skipped) in [
        (
            Facts {
                keyboard: false,
                ..held
            },
            Skipped::NoKeyboard,
        ),
        (
            Facts {
                screen: false,
                ..held
            },
            Skipped::NoScreen,
        ),
        (
            Facts {
                adapter: false,
                ..held
            },
            Skipped::NoAdapter,
        ),
    ] {
        assert_eq!(decide(facts), Gate::Skip(skipped));
    }
}

#[test]
fn the_gates_lines_count_networks_and_name_none() {
    let scan = scan(&nearby(), &[b"home".to_vec()]);
    let shown = Gate::Show(Shown::NoneInRange).line(&scan);
    assert_eq!(
        shown,
        "WIFI PICKER shown: no saved network in range (3 networks nearby, 2 not joinable)\n"
    );
    let skipped = Gate::Skip(Skipped::NoKeyboard).line(&scan);
    assert_eq!(skipped, "WIFI PICKER skipped: no keyboard\n");
    for line in [shown, skipped, Gate::Show(Shown::Asked).line(&scan)] {
        for name in ["cafe", "home", "lab", "airport", "corp"] {
            assert!(!line.contains(name), "{line}");
        }
    }
}

// The list.

#[test]
fn up_and_down_wrap_over_the_networks_and_join_other() {
    let mut picker = picker();
    assert_eq!(picker.selected(), Row::Network(0));
    picker.press(Key::Up, 0);
    assert_eq!(picker.selected(), Row::JoinOther);
    picker.press(Key::Down, 0);
    assert_eq!(picker.selected(), Row::Network(0));
    picker.press(Key::End, 0);
    assert_eq!(picker.selected(), Row::JoinOther);
    picker.press(Key::Home, 0);
    assert_eq!(picker.selected(), Row::Network(0));
}

#[test]
fn escape_on_the_list_goes_on_without_wifi() {
    let mut picker = picker();
    assert_eq!(picker.press(Key::Escape, 0), Action::Skip(Ended::Escape));
}

#[test]
fn r_scans_again_and_a_new_scan_keeps_the_selection_on_its_network() {
    let mut picker = picker();
    picker.press(Key::Down, 0);
    assert_eq!(picker.choices()[1].name, "home");
    assert_eq!(picker.press(Key::Character('r'), 0), Action::Rescan);
    assert!(picker.notice().is_some());
    // home is now the strongest.
    let mut stronger = nearby();
    stronger[1].signal_dbm = Some(-20);
    picker.update(scan(&stronger, &[b"home".to_vec()]));
    assert_eq!(names(&picker), ["home", "cafe", "lab"]);
    assert_eq!(picker.selected(), Row::Network(0));
    // Gone: the first row.
    picker.press(Key::Down, 0);
    picker.update(scan(&[bss("lab", -70, Kind::Joinable)], &[]));
    assert_eq!(picker.selected(), Row::Network(0));
    // On "Join other network..." it stays there.
    picker.press(Key::End, 0);
    picker.update(scan(&nearby(), &[]));
    assert_eq!(picker.selected(), Row::JoinOther);
}

#[test]
fn a_saved_network_joins_with_its_saved_key_without_asking() {
    let mut picker = picker();
    picker.press(Key::Down, 0);
    assert_eq!(
        picker.press(Key::Enter, 0),
        Action::JoinSaved(b"home".to_vec())
    );
    assert!(matches!(picker.mode(), Mode::Joining { .. }));
}

// The password.

#[test]
fn a_password_is_asked_masked_and_tab_shows_it() {
    let mut picker = asking();
    typed(&mut picker, "hunter2hunter2");
    assert_eq!(password(&picker).password_len(), 14);
    let scene = picker.scene(SCREEN);
    assert!(has_text(&scene, "Join cafe"));
    assert!(!has_text(&scene, "hunter2"));
    assert_eq!(picker.press(Key::Tab, 0), Action::Stay);
    assert!(password(&picker).shown());
    assert!(has_text(&picker.scene(SCREEN), "hunter2hunter2"));
    picker.press(Key::Tab, 0);
    assert!(!has_text(&picker.scene(SCREEN), "hunter2"));
}

#[test]
fn enter_joins_once_the_password_is_one_a_network_takes() {
    let mut picker = asking();
    let errors = [
        ("", wifi_page::EMPTY_PASSWORD),
        ("short", wifi_page::PASSWORD_LENGTH),
    ];
    for (text, error) in errors {
        typed(&mut picker, text);
        assert_eq!(picker.press(Key::Enter, 0), Action::Stay);
        assert_eq!(password(&picker).error(), Some(error));
        for _ in 0..text.len() {
            picker.press(Key::Backspace, 0);
        }
    }
    typed(&mut picker, &"g".repeat(64));
    assert_eq!(picker.press(Key::Enter, 0), Action::Stay);
    assert_eq!(password(&picker).error(), Some(wifi_page::PASSWORD_HEX));
    for _ in 0..64 {
        picker.press(Key::Backspace, 0);
    }
    // Not plain ASCII, then too long.
    picker.press(Key::Character('\u{e9}'), 0);
    assert_eq!(password(&picker).error(), Some(wifi_page::PLAIN_PASSWORD));
    typed(&mut picker, &"a".repeat(64));
    picker.press(Key::Character('a'), 0);
    assert_eq!(password(&picker).error(), Some(wifi_page::LONG_PASSWORD));
    for _ in 0..56 {
        picker.press(Key::Backspace, 0);
    }
    let Action::Join(request) = picker.press(Key::Enter, 0) else {
        panic!("no join");
    };
    assert_eq!(request.ssid, b"cafe".to_vec());
    assert_eq!(request.password, b"aaaaaaaa".to_vec());
    assert!(matches!(picker.mode(), Mode::Joining { .. }));
}

#[test]
fn escape_in_the_dialog_goes_back_to_the_list() {
    let mut picker = asking();
    typed(&mut picker, "secret-pass");
    assert_eq!(picker.press(Key::Escape, 0), Action::Stay);
    assert!(matches!(picker.mode(), Mode::List));
}

#[test]
fn join_other_network_takes_a_hidden_networks_name_and_password() {
    let mut picker = picker();
    picker.press(Key::End, 0);
    picker.press(Key::Enter, 0);
    assert!(matches!(picker.mode(), Mode::Other(_)));
    typed(&mut picker, "Hidden 5G");
    assert_eq!(picker.press(Key::Enter, 0), Action::Stay);
    typed(&mut picker, "hidden-pass");
    assert!(!has_text(&picker.scene(SCREEN), "hidden-pass"));
    let Action::Join(request) = picker.press(Key::Enter, 0) else {
        panic!("no join");
    };
    assert_eq!(request.ssid, b"Hidden 5G".to_vec());
    assert_eq!(request.password, b"hidden-pass".to_vec());
}

// Joining.

fn joining() -> Picker {
    let mut picker = asking();
    typed(&mut picker, "wrong-password");
    assert!(matches!(picker.press(Key::Enter, 0), Action::Join(_)));
    picker
}

#[test]
fn a_join_shows_until_its_outcome_and_escape_stops_it() {
    let mut picker = joining();
    assert!(has_text(&picker.scene(SCREEN), "Joining cafe..."));
    assert_eq!(picker.press(Key::Character('x'), 0), Action::Stay);
    assert_eq!(picker.press(Key::Escape, 0), Action::Cancel);
    assert!(matches!(picker.mode(), Mode::List));
    // An outcome that comes after is ignored.
    assert_eq!(picker.joined(Outcome::Joined, 0), Action::Stay);
}

#[test]
fn joined_goes_on_with_the_boot() {
    let mut picker = joining();
    assert_eq!(picker.joined(Outcome::Joined, 0), Action::Done);
}

#[test]
fn a_failed_join_asks_again_with_why_and_a_fresh_field() {
    for (outcome, why) in [
        (Outcome::WrongPassword, WRONG_PASSWORD),
        (Outcome::TimedOut, TIMED_OUT),
        (Outcome::Failed, FAILED),
    ] {
        let mut picker = joining();
        assert_eq!(picker.joined(outcome, 0), Action::Stay);
        let asked = password(&picker);
        assert_eq!(asked.error(), Some(why));
        assert_eq!(asked.name(), "cafe");
        assert_eq!(asked.password_len(), 0);
        assert_eq!(picker.failures(), 1);
        // The retry joins the same network.
        typed(&mut picker, "right-password");
        let Action::Join(request) = picker.press(Key::Enter, 0) else {
            panic!("no retry");
        };
        assert_eq!(request.ssid, b"cafe".to_vec());
    }
}

#[test]
fn a_saved_networks_failed_key_is_asked_for_and_a_gone_network_back_on_the_list() {
    let mut picker = picker();
    picker.press(Key::Down, 0);
    picker.press(Key::Enter, 0);
    picker.joined(Outcome::WrongPassword, 0);
    assert_eq!(password(&picker).name(), "home");
    let mut picker = joining();
    assert_eq!(picker.joined(Outcome::Gone, 0), Action::Stay);
    assert!(matches!(picker.mode(), Mode::List));
    assert_eq!(picker.notice(), Some("cafe is no longer in range."));
}

#[test]
fn after_three_failures_the_dialog_says_how_to_get_back() {
    let mut picker = joining();
    for attempt in 1..=FAILURES_BEFORE_HINT {
        picker.joined(Outcome::WrongPassword, 0);
        let hinted = has_text(&picker.scene(SCREEN), BACK_HINT);
        assert_eq!(hinted, attempt >= FAILURES_BEFORE_HINT, "{attempt}");
        typed(&mut picker, "another-try");
        picker.press(Key::Enter, 0);
    }
}

// The timeout.

#[test]
fn a_minute_with_no_key_goes_on_without_wifi_and_a_key_starts_it_again() {
    let mut picker = picker();
    assert_eq!(picker.tick(IDLE_TIMEOUT_MS - 1), Action::Stay);
    picker.press(Key::Down, IDLE_TIMEOUT_MS - 1);
    assert_eq!(picker.tick(2 * IDLE_TIMEOUT_MS - 2), Action::Stay);
    assert_eq!(
        picker.tick(2 * IDLE_TIMEOUT_MS - 1),
        Action::Skip(Ended::Timeout)
    );
}

#[test]
fn the_timeout_waits_while_a_join_runs_and_runs_again_from_its_outcome() {
    let mut picker = joining();
    assert_eq!(picker.tick(10 * IDLE_TIMEOUT_MS), Action::Stay);
    picker.joined(Outcome::TimedOut, 10 * IDLE_TIMEOUT_MS);
    assert_eq!(picker.tick(11 * IDLE_TIMEOUT_MS - 1), Action::Stay);
    assert_eq!(
        picker.tick(11 * IDLE_TIMEOUT_MS),
        Action::Skip(Ended::Timeout)
    );
}

#[test]
fn the_footer_counts_the_last_half_minute_down() {
    let mut picker = picker();
    picker.tick(IDLE_TIMEOUT_MS - COUNTDOWN_MS - 1);
    assert!(!has_text(
        &picker.scene(SCREEN),
        "Going on without Wi-Fi in"
    ));
    picker.tick(IDLE_TIMEOUT_MS - COUNTDOWN_MS);
    assert!(has_text(
        &picker.scene(SCREEN),
        "Going on without Wi-Fi in 30 s"
    ));
    picker.tick(IDLE_TIMEOUT_MS - 1);
    assert!(has_text(
        &picker.scene(SCREEN),
        "Going on without Wi-Fi in 1 s"
    ));
}

// The screen.

#[test]
fn the_list_draws_each_network_with_its_marker_and_the_unjoinable_count() {
    let scene = picker().scene(SCREEN);
    for text in [
        "Choose a network",
        "Networks nearby",
        "cafe",
        "home",
        "saved",
        "lab",
        "Join other network...",
        "2 more networks Stambha cannot join (open or enterprise)",
        "Esc: go on without Wi-Fi",
    ] {
        assert!(has_text(&scene, text), "{text}");
    }
    // Open and enterprise networks are only counted.
    assert!(!has_text(&scene, "airport") && !has_text(&scene, "corp"));
}

#[test]
fn signal_bars_light_as_many_as_the_network_has() {
    let scene = Picker::new(scan(&[bss("cafe", -60, Kind::Joinable)], &[]), true, 0).scene(SCREEN);
    // The selected row's bars: three lit, the fourth a shade off its panel.
    let bars: Vec<Tone> = scene
        .draws
        .iter()
        .filter_map(|draw| match draw {
            Draw::Panel {
                width: 3,
                tone,
                height,
                ..
            } if [4, 7, 10, 13].contains(height) => Some(*tone),
            _ => None,
        })
        .collect();
    assert_eq!(bars, [Tone::Text, Tone::Text, Tone::Text, Tone::Card]);
}

#[test]
fn an_empty_scan_says_it_is_still_looking() {
    let scene = Picker::new(Scan::default(), true, 0).scene(SCREEN);
    assert!(has_text(&scene, "Looking for networks..."));
    assert!(has_text(
        &scene,
        "No network Stambha can join was found yet."
    ));
    assert!(has_text(&scene, "Join other network..."));
}

#[test]
fn the_dialog_says_whether_the_network_is_saved_for_the_next_boot() {
    let mut saving = asking();
    assert!(has_text(
        &saving.scene(SCREEN),
        "saved on this USB for the next boot"
    ));
    saving.press(Key::Escape, 0);
    let mut once = Picker::new(scan(&nearby(), &[]), false, 0);
    once.press(Key::Enter, 0);
    assert!(has_text(&once.scene(SCREEN), "Joined for this boot only"));
}

#[test]
fn a_long_list_scrolls_with_the_selection() {
    let nearby: Vec<Nearby> = (0..MAX_CHOICES)
        .map(|n| bss(&format!("net-{n:02}"), -30 - n as i8, Kind::Joinable))
        .collect();
    let mut picker = Picker::new(scan(&nearby, &[]), true, 0);
    let visible = picker.visible(SCREEN);
    assert!(visible.len() < MAX_CHOICES + 1);
    picker.press(Key::End, 0);
    assert!(picker.visible(SCREEN).contains(&MAX_CHOICES));
    assert!(has_text(
        &picker.scene(SCREEN),
        &format!("{0} of {0}", MAX_CHOICES + 1)
    ));
}

// Privacy.

#[test]
fn no_name_or_password_reaches_debug_or_a_log_line() {
    let mut picker = asking();
    typed(&mut picker, "hunter2hunter2");
    let action = picker.press(Key::Enter, 0);
    let shown = format!("{action:?}");
    assert!(
        !shown.contains("hunter2") && !shown.contains("cafe"),
        "{shown}"
    );
    let logged = line(&action).unwrap();
    assert!(!logged.contains("hunter2") && !logged.contains("cafe"));
    let scan = scan(&nearby(), &[]);
    let shown = format!("{:?} {:?}", scan, nearby());
    for name in ["cafe", "home", "lab", "airport"] {
        assert!(!shown.contains(name), "{shown}");
    }
    for outcome in [
        Outcome::Joined,
        Outcome::WrongPassword,
        Outcome::Gone,
        Outcome::TimedOut,
        Outcome::Failed,
    ] {
        assert!(outcome_line(outcome).starts_with("WIFI PICKER "));
    }
    assert_eq!(line(&Action::Stay), None);
    assert_eq!(line(&Action::Rescan), None);
    assert!(line(&Action::JoinSaved(b"home".to_vec())).is_some_and(|l| !l.contains("home")));
}
