//! The MacBook Air M2 persona's host-only gate: Apple's own 26B123
//! DeviceTrees, prepared as setup prepares a restore bundle and checked as
//! setup checks it (`stela-cli` `setup::prepare`, `board::board` on the
//! prepared tree). `STELA_TEST_PERSONA_ADT_DIR` names a directory holding
//! the decoded trees as `j473.adt` and `j413.adt` (the IPSW's
//! `Firmware/all_flash/DeviceTree.<board>ap.im4p`, IM4P payload unwrapped);
//! they are Apple's files and are never committed. The kernel, SPTM, TXM
//! and trust cache are this module's synthetic images: only the tree is
//! Apple's.
use super::*;
use stela_soc_data::board;

/// The decoded 26B123 tree of `board`, checked against its digest.
fn original(board: &str, digest: &str) -> &'static [u8] {
    let directory = std::env::var("STELA_TEST_PERSONA_ADT_DIR").unwrap();
    let bytes = std::fs::read(format!("{directory}/{board}.adt")).unwrap();
    assert_eq!(sha256(&bytes), digest, "{board}.adt is not the 26B123 tree");
    Box::leak(bytes.into_boxed_slice())
}

fn j473() -> &'static [u8] {
    original(
        "j473",
        "375e5f929a60a8eeb38d8266365177efeada82dcd5e8d899dd21e3b72caf7395",
    )
}

fn j413() -> &'static [u8] {
    original(
        "j413",
        "28c7b46c931664fbfe591991f7e5f5d3a30c2b5ba9b4b41b0274137643d20f58",
    )
}

/// The DeviceTree of an erase restore bundle for `model` (`ApBoardID`
/// `board_id`) prepared from `tree`, as `boot prepare --restore` lays it
/// out.
fn prepared(tree: &'static [u8], model: &str, class: &str, board_id: u32) -> Vec<u8> {
    prepared_with_ledger(tree, model, class, board_id).0
}

/// [`prepared`], with the bundle's admission ledger (`hidden_devices`).
fn prepared_with_ledger(
    tree: &'static [u8],
    model: &str,
    class: &str,
    board_id: u32,
) -> (Vec<u8>, Vec<String>) {
    let input = |bytes: &'static [u8], name: &str| Input {
        bytes,
        source: source(name),
    };
    let leak = |v: Vec<u8>| -> &'static [u8] { Box::leak(v.into_boxed_slice()) };
    let inputs = Inputs {
        target: Target {
            product_type: model.into(),
            device_class: class.into(),
            product_version: "27.2".into(),
            product_build: "26B123".into(),
            variant: "Customer Erase Install (IPSW)".into(),
            chip_id: Some(0x8112),
            board_id: Some(board_id),
        },
        kernel_collection: input(leak(kernel()), "KernelCache").into(),
        auxiliary_collection: None,
        sptm: input(leak(sptm()), "Ap,SecurePageTableMonitor"),
        txm: input(leak(txm()), "Ap,TrustedExecutionMonitor"),
        device_tree: input(tree, "DeviceTree"),
        static_trust_cache: input(leak(trust_cache()), "StaticTrustCache"),
        sep_firmware: None,
        boot_manifest: None,
        cryptex1_manifest: None,
        extra_trust_caches: vec![],
        board_firmware: vec![],
        ramdisk: Some(input(leak(vec![0x5a; 100]), "RestoreRamDisk")),
        root_hash: None,
        pivot_root_hash: None,
        firmware_version: None,
    };
    let options = Options {
        dram_base: 0x8_0000_0000,
        dram_size: 0x2_0000_0000,
        phys_base: 0x8_0000_0000,
        boot_args: "-v serial=3 rd=md0".into(),
        osenvironment: Some(boot_policy::OsEnvironment::DarwinosRamdisk),
        boot_command: Some(handoff::BootCommand::TetheredRestore),
        root: Some(crate::root_selection::Root::Ramdisk),
        ecid: Some(0x1234_5678_9abc),
        ..Options::default()
    };
    let bundle = prepare(&inputs, &options).unwrap();
    let (_, tree) = bundle
        .files
        .iter()
        .find(|(name, _)| name == DEVICE_TREE_FILE)
        .unwrap();
    (tree.clone(), bundle.manifest.hidden_devices.clone())
}

/// The paths of the nodes that keep their `compatible`, in tree order.
fn admitted(tree: &[u8]) -> Vec<String> {
    fn walk(node: &crate::apple_device_tree::Node, path: &str, paths: &mut Vec<String>) {
        if node.property("compatible").is_some() {
            paths.push(if path.is_empty() {
                "/".into()
            } else {
                path.into()
            });
        }
        for child in &node.children {
            walk(child, &format!("{path}/{}", child.name().unwrap()), paths);
        }
    }
    let mut paths = vec![];
    walk(&DeviceTree::parse(tree).unwrap().root, "", &mut paths);
    paths
}

/// The persona's first gate: the engine builds a board from the prepared
/// J413AP tree, the same SoC model as J473AP's, with its two USB-C HPM dies,
/// and the PMGR accepts the bundle's admission ledger as `stela.efi` applies
/// it before entry (`stela-boot` `admit_board`). The MagSafe die `hpm5`
/// (I2C 0x3a, no `compatible`) has no model, so no target answers there.
#[test]
#[ignore = "oracle: STELA_TEST_PERSONA_ADT_DIR"]
fn the_engine_builds_a_board_from_the_prepared_j413_tree() {
    for (tree, model, class, board_id) in [
        (j473(), "Mac14,3", "j473ap", 0x24),
        (j413(), "Mac14,2", "j413ap", 0x28),
    ] {
        let (tree, ledger) = prepared_with_ledger(tree, model, class, board_id);
        let mut found = board::board(&tree).unwrap();
        found
            .power_states
            .as_mut()
            .unwrap()
            .apply_admission(&tree, &ledger)
            .unwrap_or_else(|error| panic!("{model}: {error}"));
    }
    let tree = prepared(j413(), "Mac14,2", "j413ap", 0x28);
    let found = board::board(&tree).unwrap();
    assert_eq!(found.cpus.chip.name(), "apple-m2");
    assert_eq!(found.cpus.processors.len(), 8);
    assert!(found.smc.is_some() && found.dcp.is_some());
    assert_eq!(found.usbs.len(), 2);
    let hpm: Vec<(u8, &str, Option<&str>)> = found
        .i2cs
        .iter()
        .flat_map(|bus| &bus.targets)
        .map(|t| (t.address, t.compatible.as_str(), t.firmware.as_deref()))
        .collect();
    assert_eq!(
        hpm,
        [
            (0x6b, board::HPM_MANAGER, None),
            (0x38, board::HPM, Some("HPM,46-P01-AP")),
            (0x3f, board::HPM, Some("HPM,46-P01-AP")),
        ]
    );
    assert_eq!(found.firmware_files(), ["USB-C_HPM,46-P01-AP.bin"]);
}

/// The Mac mini's prepared tree, and the admission pass alone, come out
/// byte for byte as they did before the persona's edits (digests taken at
/// `fork/next` 21e10c4a1): the edits fire only on MacBook tree features, so
/// `boot_bundle::REVISION` needs no bump.
#[test]
#[ignore = "oracle: STELA_TEST_PERSONA_ADT_DIR"]
fn the_mac_mini_tree_prepares_as_it_did_before_the_persona() {
    assert_eq!(
        sha256(&prepared(j473(), "Mac14,3", "j473ap", 0x24)),
        "ef1f66ed1646469aa989fe7a7daffd23dc9719a32a1db3f322ff1216ef01b088"
    );
    let mut tree = DeviceTree::parse(j473()).unwrap();
    let ledger = crate::devices::hide_unmodeled(&mut tree);
    assert_eq!(
        sha256(&tree.serialize()),
        "a54b58d63473f43e18c48df308a884f46844d81861ad9315b31327931c80d6ea"
    );
    assert_eq!(ledger.len(), 145);
}

/// Every device J473AP admits, J413AP admits too, plus the camera's I2C
/// controller (its targets stay hidden, as J473AP's audio buses' do). What
/// J413AP hides beyond J473AP is listed: the review artefact of the
/// persona's admission. The MagSafe `hpm5` is still in the tree (removing
/// it is the next change).
#[test]
#[ignore = "oracle: STELA_TEST_PERSONA_ADT_DIR"]
fn j413_admits_what_j473_admits() {
    let mini = prepared(j473(), "Mac14,3", "j473ap", 0x24);
    let air = prepared(j413(), "Mac14,2", "j413ap", 0x28);
    let (mini_admitted, air_admitted) = (admitted(&mini), admitted(&air));
    let missing: Vec<&String> = mini_admitted
        .iter()
        .filter(|path| !air_admitted.contains(path))
        .collect();
    assert!(missing.is_empty(), "J413AP hides {missing:?}");
    let added: Vec<&String> = air_admitted
        .iter()
        .filter(|path| !mini_admitted.contains(path))
        .collect();
    assert_eq!(added, ["/arm-io/i2c4"]);

    let ledger = |tree: &'static [u8]| {
        let mut tree = DeviceTree::parse(tree).unwrap();
        crate::devices::hide_unmodeled(&mut tree)
    };
    let mini_hidden = ledger(j473());
    let only_air: Vec<String> = ledger(j413())
        .into_iter()
        .filter(|entry| !mini_hidden.contains(entry))
        .collect();
    assert_eq!(
        only_air,
        [
            "/arm-io/spi2",
            "/arm-io/spi2/mesa",
            "/arm-io/spi4",
            "/arm-io/spi4/dp855",
            "/arm-io/i2c1/audio-speaker-left-tweeter",
            "/arm-io/i2c3/audio-speaker-right-woofer",
            "/arm-io/i2c3/audio-speaker-right-tweeter",
            "/arm-io/i2c3/audio-codec-output",
            "/arm-io/fpwm1",
            "/arm-io/alc0",
            "/arm-io/alc0/audio-leap-mic",
            "/arm-io/aop/iop-aop-nub/aop-audio",
            "/arm-io/aop/iop-aop-nub/aop-audio/audio-pdm2",
            "/arm-io/aop/iop-aop-nub/aop-audio/dc-800000",
            "/arm-io/aop/iop-aop-nub/aop-audio/dc-2400000",
            "/arm-io/aop/iop-aop-nub/aop-audio/audio-hp",
            "/arm-io/aop/iop-aop-nub/aop-audio/audio-lp-mic-in",
            "/arm-io/smc/iop-smc-nub/smc-charger-util-0",
            "/arm-io/smc/iop-smc-nub/smc-charger-util-1",
            "/arm-io/dockchannel-mtp",
            "/arm-io/dockchannel-mtp/mtp-transport",
            "/arm-io/mtp",
            "/arm-io/mtp/iop-mtp-nub",
            "/arm-io/dart-mtp",
            "/arm-io/dart-mtp/mapper-mtp",
            "/arm-io/mtp-aop-mux",
            "/arm-io/isp",
            "/arm-io/dart-isp",
            "/arm-io/dart-isp/mapper-isp",
            "/product partially-occluded-display",
        ]
    );

    let air = DeviceTree::parse(&air).unwrap();
    assert!(
        air.property("/product", "partially-occluded-display")
            .is_none()
    );
    let tcon = air.node("/arm-io/spi4/dp855").unwrap();
    assert!(tcon.property("compatible").is_none() && tcon.property("device_type").is_none());
    let magsafe = air.node("/arm-io/i2c0/hpmBusManager/hpm5").unwrap();
    assert_eq!(
        magsafe.property("hpm-class-type").unwrap().value,
        0xbu32.to_le_bytes()
    );
}
