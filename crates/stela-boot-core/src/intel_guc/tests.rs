use super::*;
use alloc::string::ToString;

fn failed(device: u16, cause: Cause) -> Record {
    Record {
        device,
        cause,
        kept: false,
    }
}

#[test]
fn a_setting_is_auto_off_or_on() {
    assert_eq!(Setting::parse("auto"), Some(Setting::Auto));
    assert_eq!(Setting::parse("off"), Some(Setting::Off));
    assert_eq!(Setting::parse("on"), Some(Setting::On));
    for word in ["AUTO", "Off", "ON", "1", "0", ""] {
        assert_eq!(Setting::parse(word), None, "{word}");
    }
    assert_eq!(Setting::default(), Setting::Auto);
    assert_eq!(Setting::Off.name(), "off");
    assert_eq!(Setting::Auto.name(), "auto");
    assert_eq!(Setting::On.name(), "on");
}

#[test]
fn a_record_round_trips_through_its_line() {
    for cause in [
        Cause::Init(-11),
        Cause::Init(-110),
        Cause::Wedged,
        Cause::Transport,
        Cause::Error,
        Cause::Metal,
    ] {
        for kept in [false, true] {
            let record = Record {
                device: 0xa7a0,
                cause,
                kept,
            };
            let line = record.line();
            assert!(line.ends_with('\n'));
            assert_eq!(Record::parse(line.trim_end()), Some(record), "{line}");
            let stamped = alloc::format!("[  142.235] {}", line.trim_end());
            assert_eq!(Record::parse(&stamped), Some(record));
        }
    }
    assert_eq!(
        failed(0xa7a0, Cause::Init(-11)).line(),
        "INTEL GUC RECORD failed device=a7a0 cause=init-11 build=test4\n"
    );
}

#[test]
fn a_quoted_or_foreign_record_is_passed_over() {
    let line = failed(0xa7ab, Cause::Transport).line();
    let line = line.trim_end();
    // An earlier boot's line repeated in this one.
    assert_eq!(
        Record::parse(&alloc::format!(
            "[    8.686] PREVIOUS BOOT [   29.099] {line}"
        )),
        None
    );
    // Another build's record.
    assert_eq!(Record::parse(&line.replace(BUILD, "dev64-1")), None);
    for broken in [
        "INTEL GUC RECORD failed device=a7a0 cause=init-11",
        "INTEL GUC RECORD failed device=zzzz cause=wedged build=dev64-1",
        "INTEL GUC RECORD failed device=a7a0 cause=sleepy build=dev64-1",
        "INTEL GUC RECORD maybe device=a7a0 cause=wedged build=dev64-1",
        "INTEL GUC RECORD",
    ] {
        assert_eq!(Record::parse(broken), None, "{broken}");
    }
}

#[test]
fn the_earlier_boot_is_the_part_after_its_marker() {
    let older = failed(0x46a6, Cause::Wedged).line();
    let newer = failed(0xa7a0, Cause::Init(-11)).line();
    let kept = Record {
        device: 0xa7a0,
        cause: Cause::Init(-11),
        kept: true,
    };
    let log = alloc::format!(
        "=== STELA BOOT 16 === version=0.6.1\n[ 1.0] {older}=== STELA BOOT 17 === \
         version=0.6.1\n[ 2.0] GPU x\n[ 142.235] {newer}[ 143.0] {}[ 144.0] STELA STOPPED code=0x11\n",
        kept.line()
    );
    assert_eq!(earlier(log.as_bytes()), Some(kept));
    // A boot with no record: the one before it does not count.
    let log = alloc::format!("=== STELA BOOT 16 ===\n{older}=== STELA BOOT 17 ===\nnothing\n");
    assert_eq!(earlier(log.as_bytes()), None);
    // No marker: the bytes are the end of a boot whose marker was written over.
    assert_eq!(
        earlier(newer.as_bytes()),
        Some(failed(0xa7a0, Cause::Init(-11)))
    );
    assert_eq!(earlier(b""), None);
}

#[test]
fn the_mode_follows_the_setting_then_the_record_of_this_device() {
    // Raptor Lake S, whose default stays the GuC in this build.
    let s = "ALDERLAKE_S";
    let record = Some(failed(0xa780, Cause::Init(-11)));
    assert_eq!(Mode::decide(Setting::Auto, None, 0xa780, s), Mode::Guc);
    assert_eq!(
        Mode::decide(Setting::Auto, record, 0xa780, s),
        Mode::Fallback(Cause::Init(-11))
    );
    // The stick moved to another PC: the GuC is tried there.
    assert_eq!(Mode::decide(Setting::Auto, record, 0xa788, s), Mode::Guc);
    assert_eq!(Mode::decide(Setting::Off, None, 0xa780, s), Mode::OffByHand);
    assert_eq!(
        Mode::decide(Setting::Off, record, 0xa780, s),
        Mode::OffByHand
    );
    assert_eq!(Mode::decide(Setting::On, None, 0xa780, s), Mode::Guc);
    assert!(
        Mode::Guc.guc()
            && !Mode::OffByHand.guc()
            && !Mode::OffByDefault.guc()
            && !Mode::Fallback(Cause::Metal).guc()
    );
}

#[test]
fn alder_and_raptor_lake_p_start_with_the_guc_in_test_4_and_fall_back_after_a_failure() {
    let p = "ALDERLAKE_P";
    assert!(!off_by_default(p) && falls_back(p));
    for platform in ["ALDERLAKE_S", "DG1", "TIGERLAKE", "DG2", "METEORLAKE"] {
        assert!(!off_by_default(platform), "{platform}");
    }
    let record = Some(failed(0xa7a0, Cause::Metal));
    for device in [0xa7a0, 0xa7ab, 0x46a8] {
        assert_eq!(Mode::decide(Setting::Auto, None, device, p), Mode::Guc);
        assert_eq!(Mode::decide(Setting::Off, None, device, p), Mode::OffByHand);
        assert_eq!(Mode::decide(Setting::On, None, device, p), Mode::Guc);
    }
    // The boot after this device's GuC failed goes without it, asked or not.
    for setting in [Setting::Auto, Setting::On] {
        assert_eq!(
            Mode::decide(setting, record, 0xa7a0, p),
            Mode::Fallback(Cause::Metal)
        );
        assert_eq!(Mode::decide(setting, record, 0xa7ab, p), Mode::Guc);
    }
    assert_eq!(Mode::decide(Setting::Off, record, 0xa7a0, p), Mode::OffByHand);
    let line = Mode::OffByDefault.line(0xa7a0, None);
    assert!(
        line.starts_with("INTEL GUC off (test4): ") && line.contains("--i915-guc on"),
        "{line}"
    );
    assert_eq!(line.matches('\n').count(), 1);
}

/// The bytes before boot 23's marker in dev64's log, cut to these lines:
/// boot 22 started the GuC, lost its Metal device and wrote its record at
/// the stop. The reader finds the record in them, as it did in dev64-1;
/// the engine, which runs a copy of the image taken before any static was
/// set, never saw what the loader read, and now takes it from the handoff.
/// This build passes a dev64-1 record over and follows one of its own.
#[test]
fn the_record_dev64s_boot_22_wrote_is_read_from_its_bytes() {
    let real = include_bytes!("dev64-boot22-tail.log");
    let text = core::str::from_utf8(real).unwrap();
    assert!(text.contains("=== STELA BOOT 22 ===") && text.ends_with("power off to continue\n"));
    assert!(
        text.contains(
            "[   45.409] INTEL GUC RECORD failed device=a7a0 cause=metal build=dev64-1\n"
        )
    );
    // Another build's record: passed over.
    assert_eq!(earlier(real), None);
    let ours = text.replace("build=dev64-1", &alloc::format!("build={BUILD}"));
    let record = earlier(ours.as_bytes());
    assert_eq!(record, Some(failed(0xa7a0, Cause::Metal)));
    assert_eq!(
        Mode::decide(Setting::On, record, 0xa7a0, "ALDERLAKE_P"),
        Mode::Fallback(Cause::Metal)
    );
    // Its mode line is no record.
    assert!(
        ours.lines()
            .filter(|line| line.contains("INTEL GUC on"))
            .all(|line| Record::parse(line).is_none())
    );
    // Once boot 23's marker joins them, boot 22's record is not boot 23's.
    let next = alloc::format!("{ours}=== STELA BOOT 23 === version=0.6.1\n");
    assert_eq!(earlier(next.as_bytes()), None);
}

#[test]
fn each_mode_says_itself_in_one_line() {
    let fallback = Mode::Fallback(Cause::Init(-11)).line(0xa7a0, None);
    assert!(
        fallback.starts_with(
            "INTEL GUC fallback (test4): GuC failed last boot (its start failed (-11)), \
             starting i915 on a7a0 without GuC"
        ),
        "{fallback}"
    );
    assert!(
        Mode::OffByHand
            .line(0xa7a0, None)
            .contains("--i915-guc off")
    );
    let on = Mode::Guc.line(0xa7ab, Some(failed(0xa7a0, Cause::Wedged)));
    assert!(on.starts_with("INTEL GUC on (test4)") && on.contains("another PC's"));
    for line in [fallback, on] {
        assert_eq!(line.matches('\n').count(), 1);
    }
}

#[test]
fn the_failures_dev64_and_a_second_raptor_lake_printed_are_told_apart() {
    let lines = [
        (
            "i915 0000:00:02.0: [drm] *ERROR* GT0: GuC initialization failed -11",
            Some(Cause::Init(-11)),
        ),
        (
            "i915 0000:00:02.0: [drm] *ERROR* GT0: Failed to initialize GPU, declaring it wedged!",
            Some(Cause::Wedged),
        ),
        (
            "i915 0000:00:02.0: [drm] *ERROR* GT0: GUC: CT: Unsolicited response message: len 1, \
             data 0xe0000101 (fence 105, last 107)",
            Some(Cause::Transport),
        ),
        (
            "i915 0000:00:02.0: [drm] *ERROR* GT0: GUC: Failed to allocate or map log buffer -12",
            Some(Cause::Error),
        ),
        (
            "i915 0000:00:02.0: [drm] *ERROR* GT0: Enabling uc failed (-5)",
            None,
        ),
        (
            "i915 0000:00:02.0: [drm] *ERROR* Device initialization failed (-12)",
            None,
        ),
    ];
    for (line, cause) in lines {
        assert_eq!(Cause::of_line(line), cause, "{line}");
    }
}

#[test]
fn only_the_platforms_whose_default_is_guc_submission_fall_back() {
    for platform in ["ALDERLAKE_P", "ALDERLAKE_S", "DG1"] {
        assert!(falls_back(platform), "{platform}");
    }
    for platform in ["TIGERLAKE", "ROCKETLAKE", "DG2", "METEORLAKE", "SKYLAKE"] {
        assert!(!falls_back(platform), "{platform}");
    }
}

#[test]
fn the_second_start_names_the_cause() {
    let stop = second_start(Cause::Transport);
    assert!(stop.starts_with("Intel graphics needs a second start; restart the PC."));
    assert!(stop.contains("command transport") && !stop.contains('\n'));
    assert_eq!(
        Cause::Metal.told().to_string(),
        "the Metal device did not open"
    );
}
