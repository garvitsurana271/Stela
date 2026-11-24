use super::*;

#[test]
fn the_lid_is_ignored_unless_asked_and_its_sleep_needs_the_method() {
    // The default does nothing with the lid, and writes nothing.
    let plain = System::parse("--system").unwrap();
    assert_eq!(plain.lid(), Lid::Ignore);
    assert_eq!(plain.to_string(), "--system");
    for (text, lid) in [
        ("--system --lid ignore", Lid::Ignore),
        ("--system --lid method", Lid::Method),
    ] {
        let options = System::parse(text).unwrap();
        assert_eq!(options.lid(), lid);
        assert_eq!(options.to_string(), text);
    }
    for (text, error) in [
        ("--system --lid", Error::Lid),
        ("--system --lid close", Error::Lid),
        ("--system --lid method --lid ignore", Error::DuplicateLid),
        (
            "--system --lid-sleep",
            Error::Constraint(Constraint::LidSleepDependency),
        ),
        (
            "--system --lid ignore --lid-sleep",
            Error::Constraint(Constraint::LidSleepDependency),
        ),
    ] {
        assert_eq!(System::parse(text).err(), Some(error), "{text}");
    }
    assert!(System::parse("--system --lid method --lid-sleep").is_ok());
}

#[test]
fn a_laptop_takes_its_features_unless_laptop_off_and_the_option_round_trips() {
    // The default is auto, and writes nothing.
    let plain = System::parse("--system").unwrap();
    assert_eq!(plain.laptop(), Laptop::Auto);
    assert_eq!(plain.laptop, None);
    for (text, laptop) in [
        ("--system --laptop auto", Laptop::Auto),
        ("--system --laptop off", Laptop::Off),
    ] {
        let options = System::parse(text).unwrap();
        assert_eq!(options.laptop(), laptop);
        assert_eq!(options.to_string(), text);
    }
    for (text, error) in [
        ("--system --laptop", Error::Laptop),
        ("--system --laptop on", Error::Laptop),
        (
            "--system --laptop off --laptop auto",
            Error::DuplicateLaptop,
        ),
    ] {
        assert_eq!(System::parse(text).err(), Some(error), "{text}");
    }
    // `--host-core`: off unless given, each value kept as written.
    assert!(!plain.host_core());
    assert_eq!(plain.host_core, None);
    for (text, on) in [
        ("--system --host-core on", true),
        ("--system --host-core off", false),
    ] {
        let options = System::parse(text).unwrap();
        assert_eq!(options.host_core(), on);
        assert_eq!(options.to_string(), text);
    }
    for (text, error) in [
        ("--system --host-core", Error::HostCore),
        ("--system --host-core yes", Error::HostCore),
        (
            "--system --host-core on --host-core off",
            Error::DuplicateHostCore,
        ),
    ] {
        assert_eq!(System::parse(text).err(), Some(error), "{text}");
    }
    // `off` beside the opt-ins it leaves: each still parses as before.
    let given =
        System::parse("--system --laptop off --battery --lid method --brightness-keys").unwrap();
    assert_eq!(given.battery, Some(Battery::Host));
    assert_eq!(given.lid(), Lid::Method);
    assert!(given.brightness_keys);
}

#[test]
fn i915_takes_its_guc_unless_i915_guc_is_off_and_the_option_round_trips() {
    use crate::intel_guc::Setting;
    let plain = System::parse("--system").unwrap();
    assert_eq!(plain.i915_guc, None);
    assert_eq!(plain.i915_guc(), Setting::Auto);
    assert_eq!(plain.to_string(), "--system");
    for (text, setting) in [
        ("--system --i915-guc auto", Setting::Auto),
        ("--system --i915-guc off", Setting::Off),
    ] {
        let options = System::parse(text).unwrap();
        assert_eq!(options.i915_guc(), setting);
        assert_eq!(options.to_string(), text);
    }
    for (text, error) in [
        ("--system --i915-guc", Error::I915Guc),
        ("--system --i915-guc on", Error::I915Guc),
        (
            "--system --i915-guc auto --i915-guc off",
            Error::DuplicateI915Guc,
        ),
    ] {
        assert_eq!(System::parse(text).err(), Some(error), "{text}");
    }
}

#[test]
fn a_hybrid_laptops_panel_is_copied_by_the_cpu_unless_hybrid_scanout_is_on() {
    let plain = System::parse("--system").unwrap();
    assert!(!plain.hybrid_scanout());
    assert_eq!(plain.hybrid_scanout, None);
    assert_eq!(plain.to_string(), "--system");
    for (text, on) in [
        ("--system --hybrid-scanout on", true),
        ("--system --hybrid-scanout off", false),
    ] {
        let options = System::parse(text).unwrap();
        assert_eq!(options.hybrid_scanout(), on);
        assert_eq!(options.to_string(), text);
    }
    for (text, error) in [
        ("--system --hybrid-scanout", Error::HybridScanout),
        ("--system --hybrid-scanout yes", Error::HybridScanout),
        (
            "--system --hybrid-scanout on --hybrid-scanout off",
            Error::DuplicateHybridScanout,
        ),
    ] {
        assert_eq!(System::parse(text).err(), Some(error), "{text}");
    }
}

#[test]
fn an_asus_laptop_runs_in_performance_unless_asus_mode_says_otherwise() {
    // The default is performance, and writes nothing.
    let plain = System::parse("--system").unwrap();
    assert_eq!(plain.asus_mode(), AsusMode::Performance);
    assert_eq!(plain.asus_mode, None);
    assert_eq!(plain.to_string(), "--system");
    for (text, mode) in [
        ("--system --asus-mode performance", AsusMode::Performance),
        ("--system --asus-mode turbo", AsusMode::Turbo),
        ("--system --asus-mode silent", AsusMode::Silent),
        ("--system --asus-mode off", AsusMode::Off),
    ] {
        let options = System::parse(text).unwrap();
        assert_eq!(options.asus_mode(), mode);
        assert_eq!(options.to_string(), text);
    }
    for (text, error) in [
        ("--system --asus-mode", Error::AsusMode),
        ("--system --asus-mode balanced", Error::AsusMode),
        (
            "--system --asus-mode off --asus-mode turbo",
            Error::DuplicateAsusMode,
        ),
    ] {
        assert_eq!(System::parse(text).err(), Some(error), "{text}");
    }
}

#[test]
fn the_tlbi_keep_is_off_unless_asked_for_and_its_check_needs_it() {
    let plain = System::parse("--system").unwrap();
    assert_eq!(plain.tlbi_keep(), (false, false));
    assert_eq!(plain.to_string(), "--system");
    for (text, keep) in [
        ("--system --tlbi-keep on", (true, false)),
        ("--system --tlbi-keep off", (false, false)),
        ("--system --tlbi-keep on --tlbi-verify-check", (true, true)),
        (
            "--system --tlbi-keep off --tlbi-verify-check",
            (false, false),
        ),
        ("--system --tlbi-verify-check", (false, false)),
    ] {
        let options = System::parse(text).unwrap();
        assert_eq!(options.tlbi_keep(), keep, "{text}");
        assert_eq!(options.to_string(), text);
    }
    for (text, error) in [
        ("--system --tlbi-keep", Error::TlbiKeep),
        ("--system --tlbi-keep yes", Error::TlbiKeep),
        (
            "--system --tlbi-keep on --tlbi-keep off",
            Error::DuplicateTlbiKeep,
        ),
        (
            "--system --tlbi-verify-check --tlbi-verify-check",
            Error::DuplicateTlbiVerifyCheck,
        ),
    ] {
        assert_eq!(System::parse(text).err(), Some(error), "{text}");
    }
}

#[test]
fn a_gpu_link_keeps_the_firmwares_aspm_unless_gpu_aspm_turns_it_off() {
    let plain = System::parse("--system").unwrap();
    assert_eq!(plain.gpu_aspm(), GpuAspm::Firmware);
    assert_eq!(plain.gpu_aspm, None);
    assert_eq!(plain.to_string(), "--system");
    for (text, aspm) in [
        ("--system --gpu-aspm off", GpuAspm::Off),
        ("--system --gpu-aspm firmware", GpuAspm::Firmware),
        ("--system --acpi-hardware --gpu-aspm off", GpuAspm::Off),
    ] {
        let options = System::parse(text).unwrap();
        assert_eq!(options.gpu_aspm(), aspm);
        assert_eq!(options.to_string(), text);
    }
    for (text, error) in [
        ("--system --gpu-aspm", Error::GpuAspm),
        ("--system --gpu-aspm on", Error::GpuAspm),
        (
            "--system --gpu-aspm off --gpu-aspm off",
            Error::DuplicateGpuAspm,
        ),
    ] {
        assert_eq!(System::parse(text).err(), Some(error), "{text}");
    }
}

#[test]
fn audio_hpet_handoff_requires_an_audio_diagnostic() {
    assert_eq!(
        System::parse("--system --audio-own-hpet").unwrap_err(),
        Error::Constraint(Constraint::AudioHpetDependency)
    );
}
#[test]
fn native_pci_ioapic_handoff_round_trips_and_refuses_diagnostic_paths() {
    let text = "--system --native-pci-own-ioapic";
    let options = System::parse(text).unwrap();
    assert!(options.native_pci_own_ioapic);
    assert_eq!(System::parse(&options.to_string()), Ok(options));
    assert_eq!(
        System::parse("--system --native-pci-own-ioapic --native-pci-own-ioapic").unwrap_err(),
        Error::DuplicateNativePciIoapic
    );
    assert_eq!(
        System::parse("--system --native-pci-own-ioapic --audio-check duplex").unwrap_err(),
        Error::Constraint(Constraint::NativePciIoapicExclusive)
    );
}

use alloc::{format, string::ToString, vec};

#[test]
fn the_trackpad_is_cabled_only_beside_host_input() {
    // The disk-port count and the cabling both read this: no port is kept
    // for a trackpad there is no host input to cable.
    let cabled = System {
        input: true,
        trackpad: Some(GuestTrackpad::Mt2),
        ..System::default()
    };
    assert_eq!(cabled.guest_trackpad(), Some(GuestTrackpad::Mt2));
    let no_input = System {
        input: false,
        ..cabled.clone()
    };
    assert_eq!(no_input.guest_trackpad(), None);
    let no_flag = System {
        trackpad: None,
        ..cabled
    };
    assert_eq!(no_flag.guest_trackpad(), None);
}

#[test]
fn options_round_trip_trackpad() {
    for (word, kind) in [
        ("mt2", GuestTrackpad::Mt2),
        ("builtin", GuestTrackpad::Builtin),
    ] {
        let text = format!("--system --input --trackpad {word}");
        let options = System::parse(&text).unwrap();
        assert_eq!(options.trackpad, Some(kind));
        assert_eq!(options.to_string(), text);
        assert_eq!(System::parse(&options.to_string()), Ok(options));
    }
    // Without the flag: no trackpad, and the line has no word for it.
    let plain = System::parse("--system --input").unwrap();
    assert_eq!(plain.trackpad, None);
    assert_eq!(plain.to_string(), "--system --input");
    for (text, error) in [
        ("--system --input --trackpad nope", Error::Trackpad),
        ("--system --input --trackpad", Error::Trackpad),
        (
            "--system --input --trackpad mt2 --trackpad builtin",
            Error::DuplicateTrackpad,
        ),
        (
            "--system --trackpad mt2",
            Error::Constraint(Constraint::TrackpadDependency),
        ),
    ] {
        assert_eq!(System::parse(text).unwrap_err(), error, "{text}");
    }
}

#[test]
fn diagnostic_options_are_typed() {
    for text in ["--system", "--system --wifi --audio-own-hpet"] {
        assert_eq!(System::parse(text).unwrap().diagnostic_option(), None);
    }
    for (text, flag) in [
        (
            "--system --native-cores 2 --native-bootstrap-check",
            "--native-bootstrap-check",
        ),
        (
            "--system --native-cores 2 --native-shared-check",
            "--native-shared-check",
        ),
    ] {
        assert_eq!(System::parse(text).unwrap().diagnostic_option(), Some(flag));
    }
}

#[test]
fn host_check_lines_round_trip_and_refuse_bad_values() {
    let line = "--host-check \
                --nor-file 11111111-2222-3333-4444-555555555555:/stela/nor.bin --deadline 30 \
                --key-seconds 5";
    let options = HostCheck::parse(line).unwrap();
    assert_eq!(options.deadline, Some(30));
    assert_eq!(HostCheck::parse(&options.to_string()), Ok(options));
    assert_eq!(HostCheck::parse("--host-check"), Ok(HostCheck::default()));
    for text in [
        "--system",
        "--host-check --bluetooth-check=1",
        "--host-check --bluetooth-check on",
        "--host-check --native-dma-check",
        "--host-check --deadline 0",
        "--host-check --nor-file x",
        "--host-check --unknown",
        "--host-check --keep-lines=1",
        "--host-check --keep-lines 1",
    ] {
        assert!(HostCheck::parse(text).is_err(), "{text}");
    }
}

#[test]
fn host_check_keep_lines_is_off_unless_named_and_round_trips() {
    assert!(!HostCheck::parse("--host-check").unwrap().keep_lines);
    assert!(
        !HostCheck::parse("--host-check --deadline 30")
            .unwrap()
            .keep_lines
    );
    let options = HostCheck::parse("--host-check --keep-lines --deadline 600").unwrap();
    assert!(options.keep_lines);
    assert_eq!(options.deadline, Some(600));
    assert_eq!(
        options.to_string(),
        "--host-check --deadline 600 --keep-lines"
    );
    assert_eq!(HostCheck::parse(&options.to_string()), Ok(options));
    assert_eq!(
        HostCheck::parse("--host-check --keep-lines --keep-lines").unwrap(),
        HostCheck {
            keep_lines: true,
            ..HostCheck::default()
        }
    );
}

const DISK: &str = "74EXN0LI:11111111-2222-3333-4444-555555555555";

fn round_trip(text: &str) -> System {
    let options = System::parse(text).unwrap();
    let line = options.to_string();
    assert_eq!(System::parse(&line), Ok(options.clone()), "{line}");
    options
}

#[test]
fn single_user_needs_a_preboot_selection() {
    let selected = round_trip(&format!("--system --preboot {DISK} {SINGLE_USER}"));
    assert!(selected.single_user);
    assert!(System::parse(&format!("--system {SINGLE_USER}")).is_err());
}

#[test]
fn a_bundle_repair_is_a_preboot_macos_boot() {
    use stela_loader::boot_selection::BUNDLE_REPAIR_OPTION;
    let repair = round_trip(&format!("--system --preboot {DISK} {BUNDLE_REPAIR_OPTION}"));
    assert!(repair.bundle_repair);
    assert!(!round_trip(&format!("--system --preboot {DISK}")).bundle_repair);
    assert_eq!(
        System::parse(&format!("--system {BUNDLE_REPAIR_OPTION}")),
        Err(Error::Constraint(Constraint::PrebootSelection))
    );
    assert_eq!(
        System::parse(&format!(
            "--system --preboot {DISK} {BUNDLE_REPAIR_OPTION} --upgrade"
        )),
        Err(Error::Constraint(Constraint::BundleRepairSelection))
    );
}

#[test]
fn no_boot_that_starts_metal_clients_sees_the_agx_model_and_restore_ramdisks_keep_it() {
    use stela_loader::boot_selection::{
        BUNDLE_REPAIR_OPTION, PAIRED_RECOVERY_OPTION, SYSTEM_RECOVERY_OPTION, UPGRADE_OPTION,
    };
    // The installed macOS, and a recoveryOS, whose WindowServer composites
    // in software without a Metal device: no Metal device rather than the
    // model, whose work never completes.
    for flag in [
        "",
        SINGLE_USER,
        BUNDLE_REPAIR_OPTION,
        PAIRED_RECOVERY_OPTION,
        SYSTEM_RECOVERY_OPTION,
    ] {
        let system = round_trip(&format!("--system --preboot {DISK} {flag}"));
        assert!(system.starts_metal_clients(), "{flag}");
        assert!(system.hides_agx(false), "{flag}");
        assert!(system.hides_agx(true), "{flag}");
    }
    // The installer's restore and a staged update run the restore ramdisk,
    // open no host GPU and keep the model.
    for line in [
        String::from("--system"),
        format!("--system --preboot {DISK} {UPGRADE_OPTION}"),
    ] {
        let ramdisk = round_trip(&line);
        assert!(!ramdisk.starts_metal_clients(), "{line}");
        assert!(!ramdisk.hides_agx(false), "{line}");
        assert!(ramdisk.hides_agx(true), "{line}");
    }
}

#[test]
fn an_install_a_restore_a_staged_update_a_recovery_os_and_a_repair_get_no_battery() {
    use stela_loader::boot_selection::{
        BUNDLE_REPAIR_OPTION, PAIRED_RECOVERY_OPTION, SAFE_BOOT_OPTION, SYSTEM_RECOVERY_OPTION,
        UPGRADE_OPTION,
    };
    let withheld = |line: &str| round_trip(line).battery_withheld(false);
    // The installed macOS takes the battery, in single-user and safe mode
    // too.
    for flag in ["", SINGLE_USER, SAFE_BOOT_OPTION] {
        let line = format!("--system --battery --preboot {DISK} {flag}");
        assert_eq!(withheld(&line), None, "{line}");
    }
    // A staged update runs the restore ramdisk; a recoveryOS and the repair
    // mend the installed system.
    for (flag, boot) in [
        (UPGRADE_OPTION, "a staged macOS update"),
        (PAIRED_RECOVERY_OPTION, "recoveryOS"),
        (SYSTEM_RECOVERY_OPTION, "recoveryOS"),
        (BUNDLE_REPAIR_OPTION, "the Metal plugin repair"),
    ] {
        let line = format!("--system --battery --preboot {DISK} {flag}");
        assert_eq!(withheld(&line), Some(boot), "{line}");
    }
    // An install and a restore study, by their options or by the restore the
    // engine runs.
    let install = format!(
        "--system --test-battery 80,charging --storage {DISK} --nor {DISK} --bundle b \
         --install p.json"
    );
    let study = format!(
        "--system --battery --bundle b --storage {DISK} --restore-study --restore-source {DISK} \
         --restore-manifest m.plist --restore-identity 2"
    );
    for line in [install, study] {
        assert_eq!(withheld(&line), Some("the macOS installer"), "{line}");
    }
    let plain = round_trip("--system --battery");
    assert_eq!(plain.battery_withheld(false), None);
    assert_eq!(plain.battery_withheld(true), Some("the macOS installer"));
}

#[test]
fn scanout_mirror_round_trips() {
    assert!(!round_trip("--system").scanout_mirror);
    assert!(round_trip(&format!("--system {SCANOUT_MIRROR}")).scanout_mirror);
}

#[test]
fn maintenance_writes_require_an_explicit_preboot_selection() {
    let ordinary = round_trip(&format!("--system --preboot {DISK}"));
    assert!(!ordinary.maintenance_write);
    assert!(!ordinary.to_string().contains(MAINTENANCE_WRITE));
    let maintenance = round_trip(&format!("--system --preboot {DISK} {MAINTENANCE_WRITE}"));
    assert!(maintenance.maintenance_write);
    assert_eq!(
        System::parse(&format!("--system {MAINTENANCE_WRITE}")),
        Err(Error::Constraint(Constraint::PrebootSelection))
    );
}

#[test]
fn setup_lines_parse_and_serialize_identically() {
    let preboot = format!(
        "--system --storage {DISK} --nor {DISK} --preboot {DISK} --ecid 0x0000000000001234 \
         --external-disks --guest-network"
    );
    let options = round_trip(&preboot);
    assert!(options.bundle && options.external_disks);
    assert_eq!(options.ecid, Some(0x1234));
    assert_eq!(options.to_string(), preboot);
    let bundle = format!("--system --storage {DISK} --nor {DISK} --bundle \\EFI\\stela\\r");
    assert_eq!(round_trip(&bundle).to_string(), bundle);
}

#[test]
fn chosen_disk_round_trips_and_the_mount_refuses_it_unreplaced() {
    let line = "--system --storage @disk --nor @disk --preboot @disk";
    let options = round_trip(line);
    assert!(options.storage.as_ref().is_some_and(is_chosen));
    assert_eq!(options.to_string(), line);
    assert!(System::parse_resolved(line).is_err());
    let resolved = line.replace("@disk", DISK);
    assert!(System::parse_resolved(&resolved).is_ok());
}

/// `--medium-read-ahead` takes the install medium's read-ahead in 1 MiB
/// chunks, 0 (read only when asked) to [`MAX_MEDIUM_READ_AHEAD`], once, for
/// an `--install`; without it the engine takes its own default.
#[test]
fn the_medium_read_ahead_is_a_chunk_count_for_an_install() {
    let install = format!("--system --storage {DISK} --nor {DISK} --bundle b --install p.json");
    assert_eq!(System::parse(&install).unwrap().medium_read_ahead, None);
    for chunks in [0, 4, MAX_MEDIUM_READ_AHEAD] {
        let options = round_trip(&format!("{install} {MEDIUM_READ_AHEAD} {chunks}"));
        assert_eq!(options.medium_read_ahead, Some(chunks));
    }
    let past = MAX_MEDIUM_READ_AHEAD + 1;
    for (text, error) in [
        (
            format!("{install} {MEDIUM_READ_AHEAD} {past}"),
            Error::MediumReadAhead,
        ),
        (
            format!("{install} {MEDIUM_READ_AHEAD}"),
            Error::MediumReadAhead,
        ),
        (
            format!("{install} {MEDIUM_READ_AHEAD} -1"),
            Error::MediumReadAhead,
        ),
        (
            format!("{install} {MEDIUM_READ_AHEAD} abc"),
            Error::MediumReadAhead,
        ),
        (
            format!("{install} {MEDIUM_READ_AHEAD} 1 {MEDIUM_READ_AHEAD} 2"),
            Error::DuplicateMediumReadAhead,
        ),
        (
            format!("--system {MEDIUM_READ_AHEAD} 2"),
            Error::Constraint(Constraint::MediumReadAheadDependency),
        ),
    ] {
        assert_eq!(System::parse(&text).unwrap_err(), error, "{text}");
    }
}

#[test]
fn every_option_survives_a_round_trip() {
    let options = round_trip(&format!(
        "--system --storage {DISK} --nor {DISK} --bundle b --install p.json \
         --install-source 11111111-2222-3333-4444-555555555555 --verify-on-read \
         --usb-storage A --usb-storage B --input --idle-skip --sep-observe --native-cores 3 \
         --native-artifacts --serialized --profile --lid method --lid-sleep --brightness-keys \
         --acpi-hardware --keep-running-on-panic --tier2 on --tier2-check --amx-native off \n         --hle --hle-off bzero \n         --monitor-trace \
         --display-trace \
         --network-check http://h/p --audio-check transport-duplex --audio-own-hpet --dram-base 0x10000000000 \
         --load 0x10000004000 --entry 0x10000008000 --x0 7 --budget 99 --break 0x10 \
         --trace 0x20 --watch 0x30,0x40 --dump x21-0x8,0x10 --dump 0x50,0x60"
    ));
    assert_eq!(options.usb_storage, ["A", "B"]);
    assert_eq!(options.audio_check, Some(AudioCheck::TransportDuplex));
    assert!(options.audio_own_hpet);
    assert!(options.display_trace);
    assert!(!System::default().display_trace);
    assert!(options.lid_sleep);
    assert!(!System::default().lid_sleep);
    assert_eq!(options.lid(), Lid::Method);
    assert!(options.brightness_keys);
    assert!(!System::default().brightness_keys);
    assert!(options.acpi_hardware);
    assert!(!System::default().acpi_hardware);
    assert!(options.keep_running_on_panic);
    assert!(!System::default().keep_running_on_panic);
    assert_eq!(options.tier2, Some(true));
    assert!(options.tier2_check && options.tier2_runs());
    assert_eq!(options.amx_model(), AmxModel::ALL);
    assert!(!options.parallel);
    assert!(options.hle);
    assert_eq!(options.hle_off, [stela_engine::hle::Id::Bzero]);
    assert_eq!(options.dumps[0], (Base::Register(21, -8), 0x10));
    let restore = round_trip(&format!(
        "--system --bundle b --storage {DISK} --restore-study --restore-source {DISK} \
         --restore-manifest m.plist --restore-identity 2"
    ));
    assert_eq!(restore.restore_manifest_path.as_deref(), Some("m.plist"));
}

/// The guest Bluetooth capture is off unless asked for, and survives the
/// line a setup writes.
#[test]
fn the_bluetooth_capture_round_trips_and_is_off_by_default() {
    assert!(!System::default().bluetooth_trace);
    assert!(!round_trip("--system --input").bluetooth_trace);
    let line = format!("--system --storage {DISK} --input {BLUETOOTH_TRACE}");
    let options = round_trip(&line);
    assert!(options.input && options.bluetooth_trace);
    assert_eq!(options.to_string(), line);
    assert!(!System::default().to_string().contains("bluetooth"));
}

/// Lending the host's controller is off unless asked for: a line without
/// the flag gives every boot the model, as before.
#[test]
fn the_bluetooth_lend_round_trips_and_is_off_by_default() {
    assert!(!System::default().bluetooth_lend);
    assert!(!round_trip("--system --input --bluetooth-trace").bluetooth_lend);
    let line = format!("--system --storage {DISK} --input {BLUETOOTH_LEND} {BLUETOOTH_TRACE}");
    let options = round_trip(&line);
    assert!(options.bluetooth_lend && options.bluetooth_trace);
    assert_eq!(options.to_string(), line);
}

#[test]
fn hle_flags_round_trip_and_refuse_unknown_ids() {
    use stela_engine::hle::Id;
    let off = round_trip("--system");
    assert!(!off.hle);
    assert!(off.hle_off.is_empty());
    assert!(round_trip("--system --hle").hle);
    let some = round_trip("--system --hle --hle-off memmove,strlen");
    assert!(some.hle);
    assert_eq!(some.hle_off, [Id::Memmove, Id::Strlen]);
    assert_eq!(some.to_string(), "--system --hle --hle-off memmove,strlen");
    // Without --hle the list is kept, and turns nothing off.
    let alone = round_trip("--system --hle-off bzero,memset,memcmp");
    assert!(!alone.hle);
    assert_eq!(alone.hle_off, [Id::Bzero, Id::Memset, Id::Memcmp]);
    for text in [
        "--system --hle-off nosuch",
        "--system --hle-off",
        "--system --hle-off memmove,",
        "--system --hle-off ,memmove",
        "--system --hle-off memmove,memmove",
        "--system --hle-off bail",
        "--system --hle-off Memmove",
    ] {
        assert_eq!(System::parse(text), Err(Error::HleId), "{text}");
    }
    for text in [
        "--system --hle --hle",
        "--system --hle-off memmove --hle-off strlen",
    ] {
        assert_eq!(System::parse(text), Err(Error::DuplicateHle), "{text}");
    }
}

#[test]
fn usb4_approvals_repeat_and_keep_their_canonical_spelling() {
    let first = "446ab8f0-5aff-47c9-8962-5870641e4d61";
    let second = "e254fadb-773c-43c3-b00d-4170323c36f8";
    let options = round_trip(&format!(
        "--system --usb4-approve {} --usb4-approve {second}",
        first.to_uppercase()
    ));
    assert_eq!(options.usb4_approved.len(), 2);
    assert_eq!(
        options.to_string(),
        format!("--system --usb4-approve {first} --usb4-approve {second}")
    );
    for (text, error) in [
        ("--system --usb4-approve", Error::Usb4Approve),
        ("--system --usb4-approve nonsense", Error::Usb4Approve),
    ] {
        assert_eq!(System::parse(text).err(), Some(error), "{text}");
    }
    assert_eq!(
        System::parse(&format!(
            "--system --usb4-approve {first} --usb4-approve {first}"
        ))
        .err(),
        Some(Error::DuplicateUsb4Approve)
    );
}

#[test]
fn wifi_names_escape_spaces_and_the_key_path_defaults() {
    let options = round_trip("--system --wifi Home%20Net%25 --wifi-country gb");
    assert_eq!(options.ssid.as_deref(), Some("Home Net%"));
    assert_eq!(options.wifi_key.as_deref(), Some(DEFAULT_WIFI_KEY));
    assert_eq!(options.wifi_country.as_deref(), Some("GB"));
    assert_eq!(
        options.to_string(),
        "--system --wifi Home%20Net%25 --wifi-country GB"
    );
    let keyed = round_trip("--system --wifi caf\u{e9} --wifi-key \\EFI\\k");
    assert_eq!(keyed.ssid.as_deref(), Some("caf\u{e9}"));
    assert_eq!(keyed.wifi_key.as_deref(), Some("\\EFI\\k"));
    for text in [
        "--system --wifi-key \\k",
        "--system --wifi-country GB",
        "--system --wifi %",
        "--system --wifi %4",
        "--system --wifi %FF",
        "--system --wifi 0123456789abcdef0123456789abcdef0",
        "--system --wifi a --wifi b",
        "--system --wifi a --wifi-country GBR",
        "--system --wifi a --wifi-country 12",
    ] {
        assert!(System::parse(text).is_err(), "{text}");
    }
}

#[test]
fn static_ipv4_and_dns_round_trip_and_stay_in_their_subnet() {
    let text = "--system --ipv4 192.168.1.20/24 --ipv4-gateway 192.168.1.1 --dns 1.1.1.1,2606:4700:4700::1111";
    let options = round_trip(text);
    assert_eq!(
        options.ipv4,
        Some(StaticIpv4 {
            address: core::net::Ipv4Addr::new(192, 168, 1, 20),
            prefix: 24
        })
    );
    assert_eq!(options.dns.len(), 2);
    assert_eq!(options.to_string(), text);
    assert_eq!(
        round_trip("--system --ipv4 10.0.0.1/31").to_string(),
        "--system --ipv4 10.0.0.1/31"
    );
    for text in [
        "--system --ipv4 192.168.1.20",
        "--system --ipv4 192.168.1.20/33",
        "--system --ipv4 192.168.1.0/24",
        "--system --ipv4 192.168.1.255/24",
        "--system --ipv4 224.0.0.1/4",
        "--system --ipv4 10.0.0.2/8 --ipv4 10.0.0.3/8",
        "--system --ipv4-gateway 192.168.1.1",
        "--system --ipv4 192.168.1.20/24 --ipv4-gateway 192.168.2.1",
        "--system --ipv4 192.168.1.20/24 --ipv4-gateway 192.168.1.20",
        "--system --dns 1.1.1.1,1.1.1.1",
        "--system --dns 1.1.1.1,1.0.0.1,9.9.9.9,8.8.8.8",
        "--system --dns 0.0.0.0",
        "--system --dns 1.1.1.1 --dns 9.9.9.9",
    ] {
        assert!(System::parse(text).is_err(), "{text}");
    }
}

#[test]
fn native_checks_imply_serialized_execution_without_writing_it() {
    let options = round_trip("--system --native-cores 2 --native-rounds");
    assert!(!options.parallel);
    assert!(!options.to_string().contains("--serialized"));
    assert!(System::parse("--system").unwrap().parallel);
}

#[test]
fn native_core_counts_refuse_duplicates_zero_and_over_capacity() {
    for text in [
        "--system --native-cores 1 --native-cores 1",
        "--system --native-cores 0",
        "--system --native-cores 65",
        "--system --native-cores x",
        "--system --native-cores",
    ] {
        assert!(System::parse(text).is_err(), "{text}");
    }
    assert_eq!(
        System::parse("--system --native-cores 64")
            .unwrap()
            .native_cores,
        Some(64)
    );
}

/// `--timer-delay-us` bounds how late a busy PE's timer interrupt comes,
/// from 100 us to 100 ms; without it the engine's 10 ms holds.
#[test]
fn a_timer_delay_takes_microseconds_within_its_bounds() {
    assert_eq!(System::default().timer_delay, None);
    let options = round_trip("--system --timer-delay-us 2000");
    assert_eq!(options.timer_delay, Some(2000));
    assert_eq!(options.to_string(), "--system --timer-delay-us 2000");
    for micros in [100, 10_000, 100_000] {
        let line = format!("--system --timer-delay-us {micros}");
        assert_eq!(round_trip(&line).timer_delay, Some(micros), "{line}");
    }
    for (text, error) in [
        ("--system --timer-delay-us", Error::TimerDelay),
        ("--system --timer-delay-us x", Error::TimerDelay),
        ("--system --timer-delay-us 0", Error::TimerDelay),
        ("--system --timer-delay-us 99", Error::TimerDelay),
        ("--system --timer-delay-us 100001", Error::TimerDelay),
        (
            "--system --timer-delay-us 1000 --timer-delay-us 1000",
            Error::DuplicateTimerDelay,
        ),
    ] {
        assert_eq!(System::parse(text).err(), Some(error), "{text}");
    }
}

/// `--no-ibpb` drops the prediction barrier at guest context switches; a
/// boot without it isolates as before.
#[test]
fn prediction_isolation_stays_on_unless_no_ibpb_is_given() {
    assert!(!System::default().no_ibpb);
    assert!(!System::parse("--system").unwrap().no_ibpb);
    let options = round_trip("--system --no-ibpb");
    assert!(options.no_ibpb);
    assert_eq!(options.to_string(), "--system --no-ibpb");
}

/// `--code-window` names the board's code window in MiB: whole 16 MiB
/// steps from 64 MiB to 2 GiB, the window's own bounds, once.
#[test]
fn a_code_window_takes_whole_steps_within_the_windows_bounds() {
    let options = round_trip("--system --code-window 1536");
    assert_eq!(options.code_window, Some(1536));
    assert_eq!(options.to_string(), "--system --code-window 1536");
    assert_eq!(System::default().code_window, None);
    assert!(!System::default().to_string().contains("--code-window"));
    for mib in [64, 784, 2048] {
        let line = format!("--system --code-window {mib}");
        assert_eq!(round_trip(&line).code_window, Some(mib), "{line}");
    }
    for (text, error) in [
        ("--system --code-window", Error::CodeWindow),
        ("--system --code-window x", Error::CodeWindow),
        ("--system --code-window 0", Error::CodeWindow),
        ("--system --code-window 48", Error::CodeWindow),
        ("--system --code-window 1000", Error::CodeWindow),
        ("--system --code-window 2064", Error::CodeWindow),
        ("--system --code-window 0x600", Error::CodeWindow),
        (
            "--system --code-window 1024 --code-window 1024",
            Error::DuplicateCodeWindow,
        ),
    ] {
        assert_eq!(System::parse(text).err(), Some(error), "{text}");
    }
}

#[test]
fn guest_pes_bound_the_executors_that_run_them() {
    for text in [
        "--system --guest-pes",
        "--system --guest-pes 0",
        "--system --guest-pes 8 --guest-pes 8",
    ] {
        assert!(System::parse(text).is_err(), "{text}");
    }
    let options = System::parse(&format!("--system --preboot {DISK} --guest-pes 8")).unwrap();
    assert_eq!(options.guest_pes, Some(8));
    assert!(options.to_string().contains("--guest-pes 8"));
    assert_eq!(options.assigned_native_cores(64, None), Ok(8));
    assert_eq!(options.assigned_native_cores(2, None), Ok(2));
    assert_eq!(options.assigned_native_cores(64, Some(4)), Ok(4));
}

#[test]
fn preboot_checks_the_live_count_before_starting_aps() {
    let preboot = format!("--system --preboot {DISK} --guest-pes 8");
    let options = System::parse(&preboot).unwrap();
    assert_eq!(options.validate_native_cores(64, 8, 8), Ok(()));
    assert_eq!(options.validate_native_cores(2, 2, 8), Ok(()));
    assert!(options.validate_native_cores(64, 8, 6).is_err());
    assert!(options.validate_native_cores(64, 8, 10).is_err());
    assert!(options.validate_native_cores(2, 2, 10).is_err());
    let options = System::parse(&format!("{preboot} --native-cores 4")).unwrap();
    assert_eq!(options.validate_native_cores(64, 4, 8), Ok(()));
    assert!(options.validate_native_cores(64, 4, 2).is_err());
    let options = System::parse(&format!("{preboot} --serialized")).unwrap();
    assert_eq!(options.validate_native_cores(64, 1, 10), Ok(()));
}

#[test]
fn exact_roster_distinguishes_verified_disk_authored_and_diagnostic_sources() {
    let assigned = |options: &str, available, prepared| {
        System::parse(options)
            .unwrap()
            .assigned_native_cores(available, prepared)
    };
    let preboot = format!("--system --preboot {DISK}");
    assert!(assigned(&preboot, 64, None).is_err());
    assert_eq!(
        assigned(&format!("{preboot} --native-cores 3"), 64, None),
        Ok(3)
    );
    assert_eq!(
        assigned(&format!("{preboot} --serialized"), 64, None),
        Ok(1)
    );
    assert_eq!(assigned("--system", 64, None), Ok(1));
    assert_eq!(assigned("--system", 64, Some(8)), Ok(8));
    assert_eq!(assigned("--system --native-cores 3", 64, Some(8)), Ok(3));
    assert!(assigned("--system --native-cores 9", 64, Some(8)).is_err());
    assert!(assigned("--system --native-cores 3", 2, Some(8)).is_err());
    assert!(assigned("--system --native-cores 3", 64, None).is_err());
    assert_eq!(assigned("--system --serialized", 64, Some(8)), Ok(1));
    assert!(assigned("--system --serialized --native-cores 4", 64, Some(8)).is_err());
    assert_eq!(
        assigned("--system --native-shared-check --native-cores 4", 64, None),
        Ok(4)
    );
    assert!(assigned("--system --native-bootstrap-check", 64, None).is_err());
}

#[test]
fn invalid_lines_refuse() {
    for text in [
        "--storage x",
        "--system --unknown 1",
        "--system --install p.json",
        "--system --ecid 0",
        "--system --safe-boot",
        "--system --network-check ftp://h/p",
        "--system --dram-base 0x1000",
        "--system --storage-ram 1 --storage-ram 2",
    ] {
        assert!(System::parse(text).is_err(), "{text}");
    }
}

#[test]
fn the_battery_options_round_trip_and_refuse_a_second_or_a_malformed_one() {
    assert_eq!(System::default().battery, None);
    assert!(!System::default().to_string().contains("battery"));
    assert_eq!(
        round_trip("--system --battery").battery,
        Some(Battery::Host)
    );
    assert_eq!(
        System::parse("--system --battery").unwrap().to_string(),
        "--system --battery"
    );
    for (text, percent, state) in [
        ("80,charging", 80, TestState::Charging),
        ("1,charging", 1, TestState::Charging),
        ("5,discharging", 5, TestState::Discharging),
        ("100,full", 100, TestState::Full),
    ] {
        let line = format!("--system {TEST_BATTERY} {text}");
        assert_eq!(
            round_trip(&line).battery,
            Some(Battery::Test { percent, state })
        );
        assert_eq!(System::parse(&line).unwrap().to_string(), line);
    }
    for text in [
        "--system --battery --battery",
        "--system --battery --test-battery 1,charging",
        "--system --test-battery 1,charging --battery",
        "--system --test-battery 1,charging --test-battery 2,charging",
    ] {
        assert_eq!(
            System::parse(text).unwrap_err(),
            Error::DuplicateBattery,
            "{text}"
        );
    }
    for value in [
        "",
        "80",
        "101,charging",
        "-1,charging",
        "80,full",
        "80,idle",
        "80,charging,more",
        "80,Charging",
        ",charging",
        // Discharging below 5% the test battery is at or near its 3% low
        // capacity: critical at boot, and macOS shuts down.
        "4,discharging",
        "0,discharging",
        // At 0 the SMC takes no battery: macOS shuts down on an empty one.
        "0,charging",
    ] {
        let text = format!("--system --test-battery {value}");
        assert_eq!(
            System::parse(&text).unwrap_err(),
            Error::TestBattery,
            "{text}"
        );
    }
}

#[test]
fn numbers_take_decimal_and_separated_hex() {
    assert_eq!(number("0x1_0000"), Ok(0x10000));
    assert_eq!(number("42"), Ok(42));
    assert!(number("0xg").is_err());
    assert_eq!(
        vec![Base::parse("x3+0x10").unwrap()],
        [Base::Register(3, 16)]
    );
}

#[test]
fn amx_native_is_on_unless_turned_off() {
    let default = round_trip("--system");
    assert_eq!(default.amx_model(), AmxModel::NONE);
    assert!(!default.to_string().contains(AMX_NATIVE));
    let off = round_trip(&format!("--system {AMX_NATIVE} off"));
    assert_eq!(off.amx_model(), AmxModel::ALL);
    assert!(off.to_string().contains("--amx-native off"));
    assert_eq!(
        round_trip(&format!("--system {AMX_NATIVE} on")).amx_model(),
        AmxModel::NONE
    );
    // Named operations, by number, printed in number order.
    let some = round_trip(&format!(
        "--system {AMX_NATIVE} off:vecfp,fma16,fms16,fma16"
    ));
    assert_eq!(some.amx_model(), AmxModel(1 << 19 | 1 << 16 | 1 << 15));
    assert!(
        some.to_string()
            .contains("--amx-native off:fma16,fms16,vecfp")
    );
    for text in [
        "--system --amx-native",
        "--system --amx-native no",
        "--system --amx-native off:",
        "--system --amx-native off:fma32,",
        "--system --amx-native off:ldx",
        "--system --amx-native off:set",
        "--system --amx-native off:FMA32",
        "--system --amx-native on:fma32",
    ] {
        assert_eq!(System::parse(text), Err(Error::AmxNative), "{text}");
    }
    assert_eq!(
        System::parse("--system --amx-native off --amx-native on"),
        Err(Error::DuplicateAmxNative)
    );
}

mod constraints;
mod fork_lines;
mod grammar;

#[test]
fn host_check_bluetooth_check_is_off_unless_named_and_round_trips() {
    assert!(!HostCheck::parse("--host-check").unwrap().bluetooth_check);
    assert!(
        !HostCheck::parse("--host-check --keep-lines")
            .unwrap()
            .bluetooth_check
    );
    let options =
        HostCheck::parse("--host-check --keep-lines --bluetooth-check --deadline 600").unwrap();
    assert!(options.bluetooth_check && options.keep_lines);
    assert_eq!(
        options.to_string(),
        "--host-check --bluetooth-check --deadline 600 --keep-lines"
    );
    assert_eq!(HostCheck::parse(&options.to_string()), Ok(options));
    assert_eq!(
        HostCheck::parse("--host-check --bluetooth-check --bluetooth-check").unwrap(),
        HostCheck {
            bluetooth_check: true,
            ..HostCheck::default()
        }
    );
    // Without it, the line prints as before.
    assert_eq!(
        HostCheck::parse("--host-check --key-seconds 5")
            .unwrap()
            .to_string(),
        "--host-check --key-seconds 5"
    );
}

#[test]
fn a_core_trial_reaches_no_disk() {
    const TRIAL: &str =
        "--system --bundle m2u --storage-ram 0x200000000 --native-cores 16 --core-trial";
    let options = round_trip(TRIAL);
    assert!(options.core_trial);
    assert!(!System::default().core_trial);
    assert_eq!(options.native_cores, Some(16));
    assert!(options.storage.is_none() && options.nor.is_none() && options.preboot.is_none());
    // Its order in the canonical line does not matter to the grammar.
    assert_eq!(
        System::parse(
            "--system --core-trial --native-cores 16 --storage-ram 0x200000000 --bundle m2u"
        ),
        Ok(options.clone())
    );
    // Without the flag nothing changes, and the line has no word for it.
    let plain = System::parse("--system --bundle m2u --storage-ram 0x200000000").unwrap();
    assert!(!plain.core_trial);
    assert!(!plain.to_string().contains(CORE_TRIAL));
    let refused = Error::Constraint(Constraint::CoreTrial);
    // What it needs: a bundle, RAM-backed disks and an explicit PE count.
    for missing in [
        "--system --storage-ram 0x200000000 --native-cores 16 --core-trial",
        "--system --bundle m2u --native-cores 16 --core-trial",
        "--system --bundle m2u --storage-ram 0x200000000 --core-trial",
    ] {
        assert_eq!(System::parse(missing).err(), Some(refused), "{missing}");
    }
    // Every option that can reach a disk is refused beside it.
    for added in [
        format!("--nor {DISK}"),
        format!("--preboot {DISK}"),
        "--external-disks".to_string(),
        "--usb-storage A".to_string(),
        "--wifi home".to_string(),
    ] {
        let text = format!("{TRIAL} {added}");
        assert_eq!(System::parse(&text).err(), Some(refused), "{text}");
    }
    // `--storage` cannot stand beside `--storage-ram` at all; on its own
    // it is refused as the trial's.
    let storage = format!("--system --bundle m2u --storage {DISK} --native-cores 16 --core-trial");
    assert_eq!(System::parse(&storage).err(), Some(refused), "{storage}");
    // An install or a restore study needs `--storage`, so either refusal
    // stops it before anything opens.
    for text in [
        format!(
            "--system --bundle m2u --storage {DISK} --nor {DISK} --install p.json \
             --native-cores 16 --core-trial"
        ),
        format!(
            "--system --bundle m2u --storage {DISK} --restore-study --restore-source {DISK} \
             --restore-manifest m.plist --restore-identity 2 --native-cores 16 --core-trial"
        ),
    ] {
        assert_eq!(System::parse(&text).err(), Some(refused), "{text}");
    }
    assert!(refused.message().contains("--core-trial writes nothing"));
}

/// The scaling test's boots: a Mac mini M2 disk boot (setup's line, with
/// `--guest-pes 8`) on a 16-core, 32-thread PC, given `--native-cores` 4, 6
/// and 8. Each reserves that many host cores before the Preboot is read,
/// passes the live check against the disk's 8 CPUs, and skips the CPU-count
/// check the count stands in for. Past the board's 8 the boot stops at that
/// check, before any AP starts.
#[test]
fn a_disk_boot_runs_on_four_six_or_eight_native_cores() {
    const LAPTOP_THREADS: usize = 32;
    const MAC_MINI_CPUS: usize = 8;
    let line = format!("--system --storage {DISK} --nor {DISK} --preboot {DISK} --guest-pes 8");
    let plain = System::parse(&line).unwrap();
    assert_eq!(plain.assigned_native_cores(LAPTOP_THREADS, None), Ok(8));
    for count in [4, 6, 8] {
        let options = round_trip(&format!("{line} --native-cores {count}"));
        assert_eq!(options.guest_pes, Some(8));
        assert_eq!(
            options.assigned_native_cores(LAPTOP_THREADS, None),
            Ok(count),
            "{count}"
        );
        assert_eq!(
            options.validate_native_cores(LAPTOP_THREADS, count, MAC_MINI_CPUS),
            Ok(()),
            "{count}"
        );
    }
    let over = System::parse(&format!("{line} --native-cores 9")).unwrap();
    assert_eq!(over.assigned_native_cores(LAPTOP_THREADS, None), Ok(9));
    assert!(
        over.validate_native_cores(LAPTOP_THREADS, 9, MAC_MINI_CPUS)
            .is_err()
    );
}

/// Tier 2 is off unless `--tier2 on` or `--tier2-check` asks for it, and
/// both are hand-written: a line without them parses as it did.
#[test]
fn tier2_runs_only_when_asked() {
    let plain = System::parse("--system").unwrap();
    assert_eq!(
        (plain.tier2, plain.tier2_check, plain.tier2_runs()),
        (None, false, false)
    );
    for (line, tier2, check, runs) in [
        ("--system --tier2 on", Some(true), false, true),
        ("--system --tier2 off", Some(false), false, false),
        ("--system --tier2-check", None, true, true),
        ("--system --tier2 on --tier2-check", Some(true), true, true),
    ] {
        let options = System::parse(line).unwrap();
        assert_eq!(
            (options.tier2, options.tier2_check, options.tier2_runs()),
            (tier2, check, runs),
            "{line}"
        );
        assert_eq!(round_trip(line).tier2, tier2, "{line}");
    }
    for (line, error) in [
        ("--system --tier2", Error::Tier2),
        ("--system --tier2 yes", Error::Tier2),
        ("--system --tier2 on --tier2 off", Error::DuplicateTier2),
        ("--system --tier2 off --tier2-check", Error::Tier2CheckOff),
    ] {
        assert_eq!(System::parse(line).err(), Some(error), "{line}");
    }
}
