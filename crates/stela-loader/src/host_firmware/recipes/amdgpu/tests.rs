use super::*;
use crate::host_firmware::{MAX_IMAGES, MAX_TOTAL_BYTES, recipes::Plan};
use alloc::{string::String, vec::Vec};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

/// Whether only families Stela leaves out load the file `member`: one
/// named after such a family and after no family it drives.
fn left_out_file(member: &str) -> bool {
    let Some(name) = member.strip_prefix("amdgpu/") else {
        return false;
    };
    let names = |(_, _, stems): &(&str, u8, &[&str])| stems.iter().any(|stem| named(name, stem));
    CHIPS.iter().any(|chip| left_out(chip.0) && names(chip))
        && !CHIPS.iter().any(|chip| !left_out(chip.0) && names(chip))
}

fn names(gpu: AmdGpu) -> Vec<&'static str> {
    gpu.files().map(|recipe| recipe.image.member).collect()
}

#[test]
fn files_are_pinned_once_in_name_order_under_the_amdgpu_notice() {
    assert!(
        AMDGPU
            .windows(2)
            .all(|pair| pair[0].image.member < pair[1].image.member)
    );
    for recipe in AMDGPU {
        let member = recipe.image.member;
        assert!(
            member.starts_with("amdgpu/") && member.ends_with(".bin"),
            "{member}"
        );
        assert_eq!(recipe.license.member, "LICENSES/LICENSE.amdgpu");
        assert_eq!(recipe.image.revision, GSP_REVISION);
        assert_eq!(recipe.image.source, LINUX_FIRMWARE);
        assert!(
            recipe.image.sha256.len() == 64
                && recipe.image.sha256.bytes().all(|b| b.is_ascii_hexdigit()),
            "{member}"
        );
        assert!(!UNPUBLISHED.contains(&member), "{member}");
        // No file only a family Stela leaves out loads is pinned.
        assert!(!left_out_file(member), "{member}");
    }
    assert!(UNPUBLISHED.windows(2).all(|pair| pair[0] < pair[1]));
    assert!(!UNPUBLISHED.iter().any(|member| left_out_file(member)));
    // A request is answered by its exact name and amdgpu's target only.
    let request = |name: &str, hardware: &str| {
        let json = serde_json::json!({"schema":1,"complete":true,"requirements":[{
            "name":name,"target":{"driver":"amdgpu","hardware":hardware,
            "revision":"linux-firmware","board":null},"minimum_bytes":1}]});
        let inventory = Inventory::parse(&serde_json::to_vec(&json).unwrap()).unwrap();
        Plan::select(&inventory)
            .ok()
            .map(|plan| plan.selections()[0].recipe.image.length)
    };
    assert_eq!(request("amdgpu/gc_10_3_6_ce.bin", "amd"), Some(263296));
    assert_eq!(
        request("amdgpu/polaris10_mc.bin", "amd").map(|_| ()),
        Some(())
    );
    assert_eq!(request("amdgpu/gc_10_3_6_ce.bin", "nvidia"), None);
    assert_eq!(request("amdgpu/gc_11_5_4_pfp.bin", "amd"), None);
}

#[test]
fn every_file_is_one_some_device_may_load() {
    let gpus = DEVICES
        .iter()
        .map(|(device, _)| AmdGpu::of(*device))
        .chain([AmdGpu::Discovered])
        .collect::<Vec<_>>();
    for recipe in AMDGPU {
        let member = recipe.image.member;
        assert!(
            gpus.iter().any(|gpu| gpu.may_load(member)),
            "{member} is no device's"
        );
    }
    // Each of a family's names matches one of its files, pinned or not
    // published (Cyan Skillfish's first revision has only an unpublished
    // `gpu_info`).
    for (chip, _, stems) in CHIPS.iter().filter(|(chip, ..)| !left_out(chip)) {
        for stem in *stems {
            assert!(
                AMDGPU
                    .iter()
                    .map(|recipe| recipe.image.member)
                    .chain(UNPUBLISHED.iter().copied())
                    .any(|member| named(&member[7..], stem)),
                "{chip} {stem}"
            );
        }
    }
}

#[test]
fn tables_are_sorted_and_every_family_has_its_names() {
    assert!(DEVICES.windows(2).all(|pair| pair[0].0 < pair[1].0));
    assert!(LEFT_TO_RADEON.windows(2).all(|pair| pair[0] < pair[1]));
    for (device, chip) in DEVICES.iter().chain(QUIRKS) {
        assert!(
            CHIPS.iter().any(|(known, ..)| known == chip),
            "{device:04x} {chip}"
        );
        assert!(
            LEFT_TO_RADEON.binary_search(device).is_err(),
            "{device:04x}"
        );
    }
    for (device, _) in QUIRKS {
        assert!(DEVICES.iter().any(|(id, _)| id == device), "{device:04x}");
    }
}

#[test]
fn a_device_gets_the_files_amdgpu_may_ask_it_for() {
    // The 9800X3D's and the 7000 series' graphics, matched by class: their
    // IP blocks' files (graphics 10.3.6, PSP 13.0.5, SDMA 5.2.6, DCN 3.1.5,
    // VCN 3.1.2) are among every versioned file, beside Van Gogh's, which
    // only discovery reaches; no family's own files are.
    for device in [0x13c0, 0x164e] {
        let gpu = AmdGpu::of(device);
        assert_eq!(gpu, AmdGpu::Discovered);
        assert_eq!(gpu.family(), None);
        let files = names(gpu);
        for member in [
            "amdgpu/gc_10_3_6_ce.bin",
            "amdgpu/gc_10_3_6_pfp.bin",
            "amdgpu/gc_10_3_6_me.bin",
            "amdgpu/gc_10_3_6_mec.bin",
            "amdgpu/gc_10_3_6_mec2.bin",
            "amdgpu/gc_10_3_6_rlc.bin",
            "amdgpu/psp_13_0_5_toc.bin",
            "amdgpu/psp_13_0_5_ta.bin",
            "amdgpu/sdma_5_2_6.bin",
            "amdgpu/dcn_3_1_5_dmcub.bin",
            "amdgpu/vcn_3_1_2.bin",
            "amdgpu/dcn_3_5_dmcub.bin",
            "amdgpu/umsch_mm_4_0_0.bin",
            "amdgpu/vangogh_pfp.bin",
        ] {
            assert!(files.contains(&member), "{member}");
        }
        assert!(
            !files
                .iter()
                .any(|member| member.starts_with("amdgpu/polaris"))
        );
        assert!(!files.contains(&"amdgpu/vce_1_0_0.bin"));
        assert!(!files.contains(&"amdgpu/banks_k_2_smc.bin"));
    }
    // Polaris 10 by ID; the ID quirks may make Polaris 11 a Polaris 10.
    let polaris = names(AmdGpu::of(0x67df));
    assert!(!polaris.is_empty() && polaris.iter().all(|m| m.starts_with("amdgpu/polaris10_")));
    assert!(polaris.contains(&"amdgpu/polaris10_k_mc.bin"));
    let quirked = names(AmdGpu::of(0x67ff));
    assert!(quirked.contains(&"amdgpu/polaris11_mc.bin"));
    assert!(quirked.contains(&"amdgpu/polaris10_mc.bin"));
    // Raven by ID loads Raven 2's and Picasso's by revision, not Renoir's.
    let raven = names(AmdGpu::of(0x15dd));
    for member in [
        "amdgpu/raven_pfp.bin",
        "amdgpu/raven2_pfp.bin",
        "amdgpu/picasso_pfp.bin",
    ] {
        assert!(raven.contains(&member), "{member}");
    }
    assert!(!raven.iter().any(|m| m.starts_with("amdgpu/renoir")));
    // Navi 21 (Sienna Cichlid) by ID.
    let navi21 = names(AmdGpu::of(0x73bf));
    assert!(navi21.contains(&"amdgpu/sienna_cichlid_smc.bin"));
    assert!(
        navi21
            .iter()
            .all(|m| m.starts_with("amdgpu/sienna_cichlid_"))
    );
    // A device amdgpu leaves to radeon loads none of its files.
    let radeon = AmdGpu::of(LEFT_TO_RADEON[0]);
    assert_eq!(radeon, AmdGpu::LeftToRadeon);
    assert_eq!(radeon.files().count(), 0);
    assert!(require_amdgpu(&mut Inventory::default(), LEFT_TO_RADEON[0]).is_err());
}

#[test]
fn a_family_whose_vulkan_device_cannot_serve_metal_is_left_out() {
    // Southern Islands (GFX6), Sea Islands (GFX7) and the compute
    // accelerators RADV opens no device on: named, never given a file.
    for (device, chip) in [
        (0x6798, "TAHITI"),
        (0x6660, "HAINAN"),
        (0x6649, "BONAIRE"),
        (0x1304, "KAVERI"),
        (0x9830, "KABINI"),
        (0x67b0, "HAWAII"),
        (0x9850, "MULLINS"),
        (0x738c, "ARCTURUS"),
        (0x7408, "ALDEBARAN"),
    ] {
        let gpu = AmdGpu::of(device);
        assert_eq!(gpu, AmdGpu::LeftOut(chip), "{device:04x}");
        assert_eq!(gpu.family(), None);
        assert_eq!(gpu.files().count(), 0);
        assert!(require_amdgpu(&mut Inventory::default(), device).is_err());
        assert!(require_amdgpu_discovered(&mut Inventory::default(), device, &[]).is_err());
    }
    // The families left out are those of the generations before the first
    // that serves, and the accelerators; every later family is driven.
    let out: Vec<&str> = CHIPS
        .iter()
        .filter(|(chip, ..)| left_out(chip))
        .map(|(chip, ..)| *chip)
        .collect();
    assert_eq!(
        out,
        [
            "TAHITI",
            "PITCAIRN",
            "VERDE",
            "OLAND",
            "HAINAN",
            "BONAIRE",
            "KAVERI",
            "KABINI",
            "HAWAII",
            "MULLINS",
            "ARCTURUS",
            "ALDEBARAN"
        ]
    );
    for (chip, generation, _) in CHIPS {
        assert_eq!(
            left_out(chip),
            *generation < amd_metal::FIRST_GENERATION
                || amd_metal::NO_VULKAN_FAMILIES.contains(chip),
            "{chip}"
        );
    }
    // Their files, and those alone, are no longer pinned: a file a driven
    // family shares stays.
    for member in [
        "amdgpu/tahiti_mc.bin",
        "amdgpu/si58_mc.bin",
        "amdgpu/vce_1_0_0.bin",
        "amdgpu/banks_k_2_smc.bin",
        "amdgpu/kaveri_sdma.bin",
        "amdgpu/arcturus_sos.bin",
    ] {
        assert!(left_out_file(member), "{member}");
        assert!(AMDGPU.iter().all(|recipe| recipe.image.member != member));
    }
    for member in [
        "amdgpu/topaz_mc.bin",
        "amdgpu/polaris10_mc.bin",
        "amdgpu/vega20_sos.bin",
        "amdgpu/gc_10_3_6_pfp.bin",
        "other/tahiti_mc.bin",
    ] {
        assert!(!left_out_file(member), "{member}");
    }
    // Topaz, the first family of GFX8, is driven.
    assert_eq!(AmdGpu::of(0x6900), AmdGpu::Named("TOPAZ", None));
}

#[test]
fn the_largest_set_fits_one_boots_firmware() {
    let mut largest = (0, 0);
    for gpu in DEVICES
        .iter()
        .map(|(device, _)| AmdGpu::of(*device))
        .filter(|gpu| !matches!(gpu, AmdGpu::LeftOut(_)))
        .chain([AmdGpu::Discovered])
    {
        let mut inventory = Inventory::default();
        let device = match gpu {
            AmdGpu::Named(..) => {
                DEVICES
                    .iter()
                    .find(|(device, _)| AmdGpu::of(*device) == gpu)
                    .unwrap()
                    .0
            }
            _ => 0x13c0,
        };
        require_amdgpu(&mut inventory, device).unwrap();
        let plan = Plan::select(&inventory).unwrap();
        let bytes: usize = plan
            .selections()
            .iter()
            .map(|s| s.recipe.image.length)
            .sum();
        largest = largest.max((plan.selections().len(), bytes));
    }
    // The discovered set: about 220 files, 49 MiB, which leaves room for a
    // second GPU's and the network's files in one boot's budget.
    assert!(largest.0 <= MAX_IMAGES / 2, "{largest:?}");
    assert!(largest.1 <= MAX_TOTAL_BYTES / 4, "{largest:?}");
}

// The rest compares the tables with the driver's sources, when the
// checkout holds them (`cargo xtask gpu fetch`).

fn amd() -> Option<PathBuf> {
    let root =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../third_party/linux/drivers/gpu/drm/amd");
    if root.join("amdgpu/amdgpu_drv.c").is_file() {
        Some(root)
    } else {
        std::eprintln!(
            "{} is not checked out: amdgpu's tables not compared",
            root.display()
        );
        None
    }
}

fn between<'a>(text: &'a str, start: &str, end: &str) -> &'a str {
    text.split_once(start)
        .and_then(|(_, rest)| rest.split_once(end))
        .unwrap_or_else(|| panic!("no {start:?} closed by {end:?}"))
        .0
}

fn hex(word: &str) -> u16 {
    u16::from_str_radix(word.trim().trim_start_matches("0x"), 16).unwrap()
}

#[test]
fn the_device_tables_are_amdgpus() {
    let Some(amd) = amd() else { return };
    let driver = fs::read_to_string(amd.join("amdgpu/amdgpu_drv.c")).unwrap();
    let mut listed = BTreeMap::new();
    for line in between(&driver, "struct pci_device_id pciidlist[] = {", "{0, 0, 0}").lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("{0x1002, ") else {
            continue;
        };
        let device = hex(rest.split(',').next().unwrap());
        let chip = rest.split("CHIP_").nth(1).unwrap();
        let chip = chip
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .next()
            .unwrap();
        assert!(
            listed.insert(device, String::from(chip)).is_none(),
            "{device:04x}"
        );
    }
    let ours: BTreeMap<u16, String> = DEVICES
        .iter()
        .map(|(device, chip)| (*device, String::from(*chip)))
        .collect();
    assert_eq!(ours, listed, "amdgpu's pciidlist changed");
    let radeon: BTreeSet<u16> = between(&driver, "amdgpu_unsupported_pciidlist[] = {", "};")
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|word| word.starts_with("0x"))
        .map(hex)
        .collect();
    assert_eq!(
        LEFT_TO_RADEON.iter().copied().collect::<BTreeSet<_>>(),
        radeon
    );
    let quirks: BTreeSet<(u16, String)> = between(&driver, "asic_type_quirks[] = {", "};")
        .lines()
        .filter_map(|line| line.trim().strip_prefix('{'))
        .map(|line| {
            let fields: Vec<&str> = line.split(',').map(str::trim).collect();
            (
                hex(fields[0]),
                String::from(fields[2].trim_start_matches("CHIP_").trim_end_matches('}')),
            )
        })
        .collect();
    let ours: BTreeSet<(u16, String)> = QUIRKS
        .iter()
        .map(|(device, chip)| (*device, String::from(*chip)))
        .collect();
    assert_eq!(ours, quirks, "amdgpu's revision quirks changed");
    // Every family of `enum amd_asic_type` but the discovery marker, in order.
    let types = fs::read_to_string(amd.join("../../../../include/drm/amd_asic_type.h")).unwrap();
    let families: Vec<String> = between(&types, "enum amd_asic_type {", "};")
        .lines()
        .filter_map(|line| line.trim().strip_prefix("CHIP_"))
        .map(|rest| String::from(rest.split([' ', ',', '\t']).next().unwrap()))
        .filter(|name| name != "IP_DISCOVERY" && name != "LAST")
        .collect();
    let ours: Vec<String> = CHIPS.iter().map(|(chip, ..)| String::from(*chip)).collect();
    assert_eq!(ours, families, "amd_asic_type changed");
    // Each family's graphics generation: the oldest graphics block of the
    // source that sets its IP blocks (`amdgpu_device_ip_early_init`), which
    // is discovery's for the families no case names.
    let device = fs::read_to_string(amd.join("amdgpu/amdgpu_device.c")).unwrap();
    let oldest = |list: &str| -> u8 {
        fs::read_to_string(amd.join(alloc::format!("amdgpu/{list}.c")))
            .unwrap()
            .split("&gfx_v")
            .skip(1)
            .filter_map(|rest| rest.split('_').next()?.parse().ok())
            .min()
            .unwrap_or_else(|| panic!("{list}.c adds no graphics block"))
    };
    let mut generations = BTreeMap::new();
    let (mut pending, mut default) = (Vec::new(), None);
    for line in between(&device, "static int amdgpu_device_ip_early_init(", "\n}\n").lines() {
        let line = line.trim();
        if let Some(chip) = line
            .strip_prefix("case CHIP_")
            .and_then(|rest| rest.strip_suffix(':'))
        {
            pending.push(String::from(chip));
        } else if let Some((call, _)) = line.split_once("_set_ip_blocks(adev)") {
            let list = call
                .rsplit(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .next()
                .unwrap();
            if pending.is_empty() {
                default = Some(oldest(list));
                break;
            }
            for chip in pending.drain(..) {
                generations.insert(chip, oldest(list));
            }
        }
    }
    let default = default.expect("a default IP block list");
    for (chip, generation, _) in CHIPS {
        assert_eq!(
            *generation,
            generations.get(*chip).copied().unwrap_or(default),
            "{chip}'s graphics generation"
        );
    }
}

/// Sources of amdgpu that declare files but that Stela's Kconfig profile
/// does not build, with the symbol that would build them.
const NOT_BUILT: [(&str, &str); 1] = [("amdgpu/isp_v4_1_1.c", "CONFIG_DRM_AMD_ISP")];

/// Every file with `extension` under `directory`.
fn sources_of(directory: &Path, extension: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = alloc::vec![directory.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|known| known == extension) {
                found.push(path);
            }
        }
    }
    found
}

#[test]
fn every_file_amdgpus_built_units_declare_is_pinned_or_unpublished() {
    let Some(amd) = amd() else { return };
    let autoconf = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../stela-linux/src/compat/generated/autoconf.h"),
    )
    .unwrap();
    for (_, symbol) in NOT_BUILT {
        assert!(
            !autoconf.contains(&format!("#define {symbol} ")),
            "{symbol} is built now"
        );
    }
    let files = sources_of(&amd, "c");
    // A source's own definitions first (amdgpu_uvd.c and amdgpu_vce.c both
    // define FIRMWARE_BONAIRE), then those of the headers.
    let mut shared = BTreeMap::new();
    let mut declared = Vec::new();
    for path in &files {
        let text = fs::read_to_string(path).unwrap();
        let mut defines = BTreeMap::new();
        for line in text.lines() {
            let mut words = line.split_whitespace();
            if words.next() == Some("#define")
                && let (Some(name), Some(value)) = (words.next(), words.next())
                && value.starts_with('"')
            {
                defines.insert(String::from(name), String::from(value.trim_matches('"')));
            }
        }
        let relative = path
            .strip_prefix(&amd)
            .unwrap()
            .to_str()
            .unwrap()
            .replace('\\', "/");
        if NOT_BUILT.iter().any(|(source, _)| *source == relative) {
            continue;
        }
        for argument in text.split("MODULE_FIRMWARE(").skip(1) {
            let argument = String::from(argument.split(')').next().unwrap().trim());
            let local = defines.get(&argument).cloned();
            declared.push((argument, local));
        }
        shared.extend(defines);
    }
    let mut headers = Vec::new();
    for path in sources_of(&amd, "h") {
        let text = fs::read_to_string(path).unwrap();
        headers.push(text);
    }
    let mut names = BTreeSet::new();
    for (argument, local) in declared {
        let name = match (argument.strip_prefix('"'), local) {
            (Some(literal), _) => String::from(literal.trim_end_matches('"')),
            (None, Some(local)) => local,
            (None, None) => shared
                .get(&argument)
                .cloned()
                .or_else(|| {
                    headers.iter().find_map(|text| {
                        text.lines().find_map(|line| {
                            let mut words = line.split_whitespace();
                            (words.next() == Some("#define") && words.next() == Some(&argument))
                                .then(|| words.next())
                                .flatten()
                                .map(|value| String::from(value.trim_matches('"')))
                        })
                    })
                })
                .unwrap_or_else(|| panic!("{argument} is defined nowhere")),
        };
        names.insert(name);
    }
    // The files only a family Stela leaves out loads are declared by
    // units amdgpu builds for the others too (`amdgpu_uvd.c`), and are
    // pinned nowhere.
    assert!(names.iter().any(|name| left_out_file(name)));
    let ours: BTreeSet<String> = AMDGPU
        .iter()
        .map(|recipe| String::from(recipe.image.member))
        .chain(UNPUBLISHED.iter().map(|name| String::from(*name)))
        .chain(names.iter().filter(|name| left_out_file(name)).cloned())
        .collect();
    let missing: Vec<_> = names.difference(&ours).collect();
    let extra: Vec<_> = ours.difference(&names).collect();
    assert!(
        missing.is_empty() && extra.is_empty(),
        "amdgpu declares {missing:?} that have no recipe, and no longer {extra:?}"
    );
    // Every declared file is one some device of the table may load.
    for name in &names {
        let loaded = DEVICES
            .iter()
            .map(|(device, _)| AmdGpu::of(*device))
            .chain([AmdGpu::Discovered])
            .any(|gpu| gpu.may_load(name));
        assert!(
            loaded || name.ends_with("/ip_discovery.bin") || left_out_file(name),
            "{name} is no device's"
        );
    }
}

#[test]
fn every_legacy_name_is_a_familys() {
    let Some(amd) = amd() else { return };
    let ucode = fs::read_to_string(amd.join("amdgpu/amdgpu_ucode.c")).unwrap();
    let naming = between(
        &ucode,
        "static const char *amdgpu_ucode_legacy_naming(",
        "\n}\n",
    );
    for literal in naming.split("return \"").skip(1) {
        let name = literal.split('"').next().unwrap();
        let covered = CHIPS.iter().any(|(_, _, stems)| {
            stems.iter().any(|stem| {
                name == *stem || name.strip_prefix(stem).is_some_and(|r| r.starts_with('_'))
            })
        });
        assert!(
            covered,
            "amdgpu_ucode_legacy_naming's {name:?} is no family's"
        );
    }
}

/// The files a discovered device with these blocks gets.
fn discovered(ips: &[(u16, u8, u8, u8)]) -> Vec<&'static str> {
    let ips: Vec<DiscoveredIp> = ips
        .iter()
        .map(|&(hw_id, major, minor, revision)| DiscoveredIp {
            hw_id,
            major,
            minor,
            revision,
        })
        .collect();
    AMDGPU
        .iter()
        .map(|recipe| recipe.image.member)
        .filter(|member| discovered_may_load(&ips, member))
        .collect()
}

#[test]
fn a_discovered_device_gets_only_its_blocks_versions_files() {
    // Raphael: GC 10.3.6, PSP and SMU 13.0.5, SDMA 5.2.6, VCN 3.1.2,
    // DCN 3.1.5.
    let raphael = discovered(&[
        (11, 10, 3, 6),
        (255, 13, 0, 5),
        (1, 13, 0, 5),
        (42, 5, 2, 6),
        (12, 3, 1, 2),
        (271, 3, 1, 5),
    ]);
    for member in [
        "amdgpu/gc_10_3_6_pfp.bin",
        "amdgpu/gc_10_3_6_rlc.bin",
        "amdgpu/psp_13_0_5_toc.bin",
        "amdgpu/sdma_5_2_6.bin",
        "amdgpu/vcn_3_1_2.bin",
        "amdgpu/dcn_3_1_5_dmcub.bin",
    ] {
        assert!(raphael.contains(&member), "{member}");
    }
    assert!(
        raphael.iter().all(|member| {
            [
                "gc_10_3_6_",
                "psp_13_0_5_",
                "smu_13_0_5",
                "sdma_5_2_6",
                "vcn_3_1_2",
                "dcn_3_1_5_",
            ]
            .iter()
            .any(|prefix| member["amdgpu/".len()..].starts_with(prefix))
        }),
        "{raphael:?}"
    );
    // Strix Point: a two-number DCN name, VPE and the media scheduler.
    let strix = discovered(&[
        (11, 11, 5, 0),
        (255, 14, 0, 0),
        (42, 6, 1, 0),
        (12, 4, 0, 5),
        (21, 6, 1, 0),
        (271, 3, 5, 0),
    ]);
    for member in [
        "amdgpu/gc_11_5_0_me.bin",
        "amdgpu/psp_14_0_0_toc.bin",
        "amdgpu/sdma_6_1_0.bin",
        "amdgpu/vcn_4_0_5.bin",
        "amdgpu/vpe_6_1_0.bin",
        "amdgpu/dcn_3_5_dmcub.bin",
        "amdgpu/umsch_mm_4_0_0.bin",
    ] {
        assert!(strix.contains(&member), "{member}");
    }
    assert!(!strix.contains(&"amdgpu/dcn_3_5_1_dmcub.bin"));
    assert!(!strix.iter().any(|member| member.contains("vangogh")));
    // Van Gogh's files by its graphics version.
    let vangogh = discovered(&[(11, 10, 3, 1), (255, 11, 5, 0)]);
    assert!(vangogh.contains(&"amdgpu/vangogh_pfp.bin"));
    // A few files, against the superset of every IP-versioned file.
    assert!(raphael.len() < 30 && strix.len() < 40);
    assert!(names(AmdGpu::Discovered).len() > 150);
}

#[test]
fn every_discovered_file_is_attributed_to_a_block_or_a_family() {
    // Each file the superset holds is one some discovery table selects, so
    // narrowing drops none a device could ask for.
    for member in names(AmdGpu::Discovered) {
        let name = &member["amdgpu/".len()..];
        let attributed = BLOCK_IPS.iter().any(|(block, _)| {
            name.strip_prefix(block)
                .and_then(|rest| rest.strip_prefix('_'))
                .and_then(file_version)
                .is_some()
        }) || named(name, UMSCH.0)
            || DISCOVERED_CHIPS.iter().any(|(chip, _)| {
                CHIPS
                    .iter()
                    .find(|(known, ..)| known == chip)
                    .is_some_and(|(_, _, stems)| stems.iter().any(|stem| named(name, stem)))
            });
        assert!(attributed, "{member}");
    }
    // Every family only discovery reaches has its graphics version here.
    for (chip, ..) in CHIPS
        .iter()
        .filter(|(chip, ..)| !DEVICES.iter().any(|(_, named)| named == chip))
    {
        assert!(
            DISCOVERED_CHIPS.iter().any(|(known, _)| known == chip),
            "{chip}"
        );
    }
}
