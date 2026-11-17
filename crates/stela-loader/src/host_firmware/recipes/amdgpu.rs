//! amdgpu's firmware: every file its built units declare
//! (`MODULE_FIRMWARE`) that linux-firmware holds, and the files one device
//! may ask for. amdgpu names a file after its family for the devices its
//! PCI ID table names (`polaris10_mc.bin`), and after the version of the IP
//! block for the devices it identifies by their IP discovery table
//! (`gc_11_0_0_pfp.bin`), which only the device holds. So a device the
//! table names gets its family's files, and one it matches by class gets
//! every file named after an IP version, of which its probe loads those
//! its discovery table names.
use super::{Artifact, Consumer, Error, GSP_REVISION, LINUX_FIRMWARE, Recipe, Request};
use crate::host_firmware::requirements::Inventory;
use alloc::format;
use stela_driver_core::{
    amd_metal,
    firmware::{Requirement, Target},
};

/// linux-firmware's notice for its `amdgpu` directory (`WHENCE`: "Licence:
/// Redistributable. See LICENSE.amdgpu for details.").
const LICENSE: Artifact = Artifact {
    source: LINUX_FIRMWARE,
    revision: GSP_REVISION,
    member: "LICENSES/LICENSE.amdgpu",
    length: 2938,
    sha256: "572872598565dc3513470de971a32bf9db301f47afeef3636345eadae33b2eee",
};

/// A file of linux-firmware's `amdgpu` directory, under its notice, at the
/// revision the GSP-RM files are pinned at.
const fn file(member: &'static str, length: usize, sha256: &'static str) -> Recipe {
    Recipe {
        image: Artifact {
            source: LINUX_FIRMWARE,
            revision: GSP_REVISION,
            member,
            length,
            sha256,
        },
        license: &LICENSE,
        consumer: Consumer::Amdgpu,
    }
}

/// Each family of `enum amd_asic_type` with its graphics generation and
/// the names its files start with (`<name>_` or `<name>.bin`), as its
/// blocks' sources choose them: the family's own name, and the names a
/// revision or a board picks within it. The generation is the oldest
/// graphics block the family's IP block list holds (`gfx_v6_0` in `si.c`,
/// `gfx_v7_*` in `cik.c`, `gfx_v8_*` in `vi.c`, `gfx_v9_0` and later
/// through discovery), which decides whether Stela drives the family
/// ([`left_out`]).
/// Southern Islands load `si58_mc.bin` on boards with that memory
/// (`gmc_v6_0.c`), Hainan `banks_k_2_smc.bin` on one board (`si_dpm.c`),
/// and Tahiti, Pitcairn and Verde the video encoder's `vce_1_0_0.bin`
/// (`amdgpu_vce.c`). Raven loads Raven 2's and Picasso's files by revision,
/// Renoir Green Sardine's, Cyan Skillfish its second revision's
/// (`amdgpu_ucode_legacy_naming`).
const CHIPS: &[(&str, u8, &[&str])] = &[
    ("TAHITI", 6, &["tahiti", "si58", "vce_1_0_0"]),
    ("PITCAIRN", 6, &["pitcairn", "si58", "vce_1_0_0"]),
    ("VERDE", 6, &["verde", "si58", "vce_1_0_0"]),
    ("OLAND", 6, &["oland", "si58"]),
    ("HAINAN", 6, &["hainan", "si58", "banks_k_2"]),
    ("BONAIRE", 7, &["bonaire"]),
    ("KAVERI", 7, &["kaveri"]),
    ("KABINI", 7, &["kabini"]),
    ("HAWAII", 7, &["hawaii"]),
    ("MULLINS", 7, &["mullins"]),
    ("TOPAZ", 8, &["topaz"]),
    ("TONGA", 8, &["tonga"]),
    ("FIJI", 8, &["fiji"]),
    ("CARRIZO", 8, &["carrizo"]),
    ("STONEY", 8, &["stoney"]),
    ("POLARIS10", 8, &["polaris10"]),
    ("POLARIS11", 8, &["polaris11"]),
    ("POLARIS12", 8, &["polaris12"]),
    ("VEGAM", 8, &["vegam"]),
    ("VEGA10", 9, &["vega10"]),
    ("VEGA12", 9, &["vega12"]),
    ("VEGA20", 9, &["vega20"]),
    ("RAVEN", 9, &["raven", "raven2", "picasso"]),
    ("ARCTURUS", 9, &["arcturus"]),
    ("RENOIR", 9, &["renoir", "green_sardine"]),
    ("ALDEBARAN", 9, &["aldebaran"]),
    ("NAVI10", 9, &["navi10"]),
    ("CYAN_SKILLFISH", 9, &["cyan_skillfish", "cyan_skillfish2"]),
    ("NAVI14", 9, &["navi14"]),
    ("NAVI12", 9, &["navi12"]),
    ("SIENNA_CICHLID", 9, &["sienna_cichlid"]),
    ("NAVY_FLOUNDER", 9, &["navy_flounder"]),
    ("VANGOGH", 9, &["vangogh"]),
    ("DIMGREY_CAVEFISH", 9, &["dimgrey_cavefish"]),
    ("BEIGE_GOBY", 9, &["beige_goby"]),
    ("YELLOW_CARP", 9, &["yellow_carp"]),
];

/// The IP blocks whose files amdgpu names after the version discovery
/// reports, `<block>_<major>_<minor>[_<revision>]`: graphics, security
/// processor, power management, DMA, video, video processing, the display
/// microcontroller and the media scheduler (`amdgpu_ucode_ip_version_decode`,
/// `amdgpu_dm.c`, `amdgpu_umsch_mm.c`).
const IP_BLOCKS: &[&str] = &["gc", "psp", "smu", "sdma", "vcn", "vpe", "dcn", "umsch_mm"];

/// The `revision` quirks of amdgpu's table that give a device another
/// family than its entry (`asic_type_quirks`): device 0x67ff is Polaris 11
/// or, by revision, Polaris 10.
const QUIRKS: &[(u16, &str)] = &[(0x67ff, "POLARIS10")];

/// What amdgpu may load on one AMD display function.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AmdGpu {
    /// Its table names the device: its family's files, and those of the
    /// family a revision quirk may give it.
    Named(&'static str, Option<&'static str>),
    /// Its table matches the device by class and its probe identifies it by
    /// its IP discovery table: every file named after an IP version, and
    /// the files of the families only discovery reaches.
    Discovered,
    /// amdgpu leaves it to an older driver.
    LeftToRadeon,
    /// Its table names the device, of this family, which Stela leaves out:
    /// RADV's device on it cannot serve the Metal device ([`left_out`]).
    /// It is not claimed, so it loads no file.
    LeftOut(&'static str),
}

/// Whether Stela leaves the family `chip` out, as its Vulkan device cannot
/// serve the Metal device (`stela_driver_core::amd_metal`): no device of
/// it is claimed, and none of its files is pinned.
pub fn left_out(chip: &str) -> bool {
    CHIPS
        .iter()
        .find(|(known, ..)| *known == chip)
        .is_some_and(|(_, generation, _)| !amd_metal::serves(chip, *generation))
}

impl AmdGpu {
    /// The device with PCI device ID `device`.
    pub fn of(device: u16) -> Self {
        if LEFT_TO_RADEON.binary_search(&device).is_ok() {
            return Self::LeftToRadeon;
        }
        match DEVICES.binary_search_by_key(&device, |(id, _)| *id) {
            Ok(index) if left_out(DEVICES[index].1) => Self::LeftOut(DEVICES[index].1),
            Ok(index) => Self::Named(
                DEVICES[index].1,
                QUIRKS
                    .iter()
                    .find(|(id, _)| *id == device)
                    .map(|(_, chip)| *chip),
            ),
            Err(_) => Self::Discovered,
        }
    }

    /// Whether amdgpu may ask this device for `member`.
    pub fn may_load(self, member: &str) -> bool {
        let Some(name) = member.strip_prefix("amdgpu/") else {
            return false;
        };
        let family = |chip: &str| {
            CHIPS
                .iter()
                .find(|(known, ..)| *known == chip)
                .is_some_and(|(_, _, stems)| stems.iter().any(|stem| named(name, stem)))
        };
        match self {
            Self::Named(chip, quirk) => family(chip) || quirk.is_some_and(family),
            Self::Discovered => {
                versioned(name)
                    || CHIPS
                        .iter()
                        .filter(|(chip, ..)| !DEVICES.iter().any(|(_, named)| named == chip))
                        .any(|(chip, ..)| family(chip))
            }
            Self::LeftToRadeon | Self::LeftOut(_) => false,
        }
    }

    /// The files amdgpu may ask this device for.
    pub fn files(self) -> impl Iterator<Item = &'static Recipe> {
        AMDGPU
            .iter()
            .filter(move |recipe| self.may_load(recipe.image.member))
    }

    /// The family as amdgpu names it (`amdgpu_asic_name`), when its table
    /// names the device.
    pub fn family(self) -> Option<&'static str> {
        match self {
            Self::Named(chip, _) => Some(chip),
            _ => None,
        }
    }
}

/// Whether the file `name` (without `amdgpu/`) starts with `stem` as a
/// whole word: `<stem>_...` or `<stem>.bin`.
fn named(name: &str, stem: &str) -> bool {
    name.strip_prefix(stem)
        .is_some_and(|rest| rest.starts_with('_') || rest == ".bin")
}

/// Whether the file `name` is named after an IP block's version.
fn versioned(name: &str) -> bool {
    IP_BLOCKS.iter().any(|block| {
        name.strip_prefix(block)
            .and_then(|rest| rest.strip_prefix('_'))
            .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
    })
}

#[cfg(test)]
mod tests;

/// One IP block as the device's IP discovery table lists it: its hardware
/// ID (`soc15_hw_ip.h`) and version.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DiscoveredIp {
    pub hw_id: u16,
    pub major: u8,
    pub minor: u8,
    pub revision: u8,
}

/// Each block of [`IP_BLOCKS`] named after one block's version, with that
/// block's hardware ID: graphics (GC), the security processor (MP0), power
/// management (MP1), DMA (SDMA0), video (VCN), video processing (VPE) and
/// the display microcontroller (DMU, `amdgpu_dm.c`'s DCN version).
const BLOCK_IPS: &[(&str, u16)] = &[
    ("gc", 11),
    ("psp", 255),
    ("smu", 1),
    ("sdma", 42),
    ("vcn", 12),
    ("vpe", 21),
    ("dcn", 271),
];

/// The media scheduler's file, which `amdgpu_umsch_mm.c` loads for VCN
/// 4.0.5 and 4.0.6.
const UMSCH: (&str, u16, &[(u8, u8, u8)]) = ("umsch_mm_4_0_0", 12, &[(4, 0, 5), (4, 0, 6)]);

/// The families only discovery reaches, with the graphics version
/// amdgpu's discovery gives that family by (`amdgpu_discovery.c`:
/// GC 10.3.1 is `CHIP_VANGOGH`), whose files then carry its name
/// (`amdgpu_ucode_legacy_naming`).
const DISCOVERED_CHIPS: &[(&str, (u8, u8, u8))] = &[("VANGOGH", (10, 3, 1))];

/// The version a file named after a block's version carries: up to three
/// numbers after `<block>_`, a missing revision being 0
/// (`dcn_3_5_dmcub.bin` is DCN 3.5.0's).
fn file_version(rest: &str) -> Option<(u8, u8, u8)> {
    let rest = rest.strip_suffix(".bin").unwrap_or(rest);
    let mut numbers = rest.split('_').map_while(|token| token.parse::<u8>().ok());
    let major = numbers.next()?;
    let minor = numbers.next()?;
    Some((major, minor, numbers.next().unwrap_or(0)))
}

/// Whether amdgpu asks a device whose discovery table lists `ips` for
/// `member`: a file named after a block's version when the table lists
/// that block at that version, the media scheduler's for its VCN versions,
/// and a discovery-only family's files when its graphics version is the
/// table's.
pub fn discovered_may_load(ips: &[DiscoveredIp], member: &str) -> bool {
    let Some(name) = member.strip_prefix("amdgpu/") else {
        return false;
    };
    let listed = |hw_id: u16, version: (u8, u8, u8)| {
        ips.iter()
            .any(|ip| ip.hw_id == hw_id && (ip.major, ip.minor, ip.revision) == version)
    };
    let (umsch, umsch_ip, umsch_versions) = UMSCH;
    if named(name, umsch) {
        return umsch_versions
            .iter()
            .any(|&version| listed(umsch_ip, version));
    }
    for &(block, hw_id) in BLOCK_IPS {
        if let Some(rest) = name
            .strip_prefix(block)
            .and_then(|rest| rest.strip_prefix('_'))
            && let Some(version) = file_version(rest)
        {
            return listed(hw_id, version);
        }
    }
    DISCOVERED_CHIPS.iter().any(|&(chip, graphics)| {
        listed(11, graphics)
            && CHIPS
                .iter()
                .find(|(known, ..)| *known == chip)
                .is_some_and(|(_, _, stems)| stems.iter().any(|stem| named(name, stem)))
    })
}

/// Requires the files amdgpu asks for on the AMD GPU with PCI device ID
/// `device` whose IP discovery table lists `ips`: [`require_amdgpu`] for a
/// device its table names, which loads its family's files whatever its
/// discovery table says, else those of [`discovered_may_load`].
pub fn require_amdgpu_discovered(
    inventory: &mut Inventory,
    device: u16,
    ips: &[DiscoveredIp],
) -> Result<(), Error> {
    if AmdGpu::of(device) != AmdGpu::Discovered {
        return require_amdgpu(inventory, device);
    }
    for recipe in AMDGPU
        .iter()
        .filter(|recipe| discovered_may_load(ips, recipe.image.member))
    {
        inventory.observe(Requirement {
            name: recipe.image.member,
            target: TARGET,
            minimum_bytes: 1,
        })?;
    }
    Ok(())
}

/// The binding amdgpu's files take: Linux's names, as for nouveau's.
const TARGET: Target<'static> = Target {
    driver: "amdgpu",
    hardware: "amd",
    revision: "linux-firmware",
    board: None,
};

/// amdgpu's request for a file, by the name the driver gives it.
pub(super) fn select(request: &Request) -> Option<&'static Recipe> {
    if request.target.hardware != TARGET.hardware || request.target.revision != TARGET.revision {
        return None;
    }
    AMDGPU
        .binary_search_by_key(&request.name.as_str(), |recipe| recipe.image.member)
        .ok()
        .map(|index| &AMDGPU[index])
}

/// The family of the AMD GPU with PCI device ID `device` as amdgpu names
/// it, when its table names the device.
pub fn amd_gpu_family(device: u16) -> Option<&'static str> {
    AmdGpu::of(device).family()
}

/// Requires the files amdgpu may ask for on the AMD GPU with PCI device ID
/// `device`.
pub fn require_amdgpu(inventory: &mut Inventory, device: u16) -> Result<(), Error> {
    let gpu = AmdGpu::of(device);
    match gpu {
        AmdGpu::LeftToRadeon => {
            return Err(Error(format!(
                "{device:04x} is left by amdgpu to an older driver"
            )));
        }
        AmdGpu::LeftOut(chip) => {
            return Err(Error(format!(
                "{device:04x} ({chip}) is left out: its Vulkan device cannot serve the Metal \
                 device"
            )));
        }
        AmdGpu::Named(..) | AmdGpu::Discovered => {}
    }
    for recipe in gpu.files() {
        inventory.observe(Requirement {
            name: recipe.image.member,
            target: TARGET,
            minimum_bytes: 1,
        })?;
    }
    Ok(())
}

// Generated from amdgpu at Stela's Linux pin and linux-firmware at
// GSP_REVISION; `tests::amdgpu_*` compare it with both.

/// Every file amdgpu's built units declare (`MODULE_FIRMWARE`) that
/// linux-firmware holds at the pin, by name, but those only families Stela
/// leaves out load ([`left_out`]).
pub const AMDGPU: &[Recipe] = &[
    file(
        "amdgpu/beige_goby_ce.bin",
        263296,
        "13aecf0a22dbc3071ade01355b27a4f90a0f46fc699064ab63e08392e65bbeab",
    ),
    file(
        "amdgpu/beige_goby_dmcub.bin",
        114164,
        "286458c7911172734dd514631dcfba9690c2242f24f9be72afd6b56c0341974a",
    ),
    file(
        "amdgpu/beige_goby_me.bin",
        263424,
        "a133027a81b728409ffcd38ca1d37b87c40f24d1d336697ff80b04ff36b6287c",
    ),
    file(
        "amdgpu/beige_goby_mec.bin",
        268160,
        "387f8d2926c8b81cc87a6a200879ba334a373aa04f757d233c2af8b4a3b005fa",
    ),
    file(
        "amdgpu/beige_goby_mec2.bin",
        268160,
        "387f8d2926c8b81cc87a6a200879ba334a373aa04f757d233c2af8b4a3b005fa",
    ),
    file(
        "amdgpu/beige_goby_pfp.bin",
        263424,
        "3b655d0dea35ccbd1352414e64bc933f7ff4f84eda40b382681863fa35c20a85",
    ),
    file(
        "amdgpu/beige_goby_rlc.bin",
        130472,
        "9208c221ef15175cd52c66c0473f603544e5cff2c2d00914dadd6913ef5f2c03",
    ),
    file(
        "amdgpu/beige_goby_sdma.bin",
        34048,
        "61c4352b22f28f493ced87d1725fc2c3f985fb3889ae8da13da91bc507683f60",
    ),
    file(
        "amdgpu/beige_goby_smc.bin",
        244934,
        "f3489486b8cbd2e4cb5298cd5fbc8a90f1d885e07f8bd356addafad0be5303d5",
    ),
    file(
        "amdgpu/beige_goby_sos.bin",
        205984,
        "cdee142c0f33d9a3f956edf04019a11a4dd3a1c48eea6da91913fafa045857f0",
    ),
    file(
        "amdgpu/beige_goby_ta.bin",
        259328,
        "744d1a60888e23af672ce13923dadfe21d35c122e64d4a0629f9bfc26eae832b",
    ),
    file(
        "amdgpu/beige_goby_vcn.bin",
        582800,
        "2f3cbd54cc9fad35665c06079f61e175dae3dca34d971bfeb73e4ab3efd7bf65",
    ),
    file(
        "amdgpu/carrizo_ce.bin",
        8832,
        "332db9aa5d5cd3c20c464036713d68fc7ed0a4d57fad0db5f8c0843c10398e99",
    ),
    file(
        "amdgpu/carrizo_me.bin",
        17024,
        "adb926ea4fc46a329d198ea690d8d5e98eb55d7511985561047566b6f80d7a9e",
    ),
    file(
        "amdgpu/carrizo_mec.bin",
        262784,
        "f5b1b485bdeb1d966ce6b6c7e0eadd08ce93d88a456944cc5b344869043cc4c1",
    ),
    file(
        "amdgpu/carrizo_mec2.bin",
        262784,
        "f5b1b485bdeb1d966ce6b6c7e0eadd08ce93d88a456944cc5b344869043cc4c1",
    ),
    file(
        "amdgpu/carrizo_pfp.bin",
        17024,
        "1b49b4f0bc61c12901e4d5d09fa63c4a7b27a8474ce6424da99e80f581e9783a",
    ),
    file(
        "amdgpu/carrizo_rlc.bin",
        18836,
        "873b4de251ed22f3d106f23aad3588be53afd44bde658422208327be88439a0d",
    ),
    file(
        "amdgpu/carrizo_sdma.bin",
        10624,
        "c8d3eadacddbaf54557690610dcedefb0445eadd3b6e8dbd3cb67602e90110ac",
    ),
    file(
        "amdgpu/carrizo_sdma1.bin",
        10624,
        "c8d3eadacddbaf54557690610dcedefb0445eadd3b6e8dbd3cb67602e90110ac",
    ),
    file(
        "amdgpu/carrizo_uvd.bin",
        271712,
        "02ff77ca171a9ccb2f051fe7580f86e76b3474430e70d22ffd41908c438864e2",
    ),
    file(
        "amdgpu/carrizo_vce.bin",
        175840,
        "a962c43e1143d1719f097760de4a21290c370b3c1878edd2a59e9131dea88293",
    ),
    file(
        "amdgpu/cyan_skillfish2_ce.bin",
        263296,
        "5946efbf7e46ccfe7e9f965da56c35b1187469d1c1a49c8a9eff433eb3ab4723",
    ),
    file(
        "amdgpu/cyan_skillfish2_me.bin",
        263424,
        "d4ed4d968de0d7720f7c8215d42fdae727bf99cdf43cee6f8f864dc2584b17e2",
    ),
    file(
        "amdgpu/cyan_skillfish2_mec.bin",
        268592,
        "b1c1f843a8faaa2de537d6ba05052a3776b5a1b73f8a3c3f5ed9d568489776a4",
    ),
    file(
        "amdgpu/cyan_skillfish2_mec2.bin",
        268592,
        "b1c1f843a8faaa2de537d6ba05052a3776b5a1b73f8a3c3f5ed9d568489776a4",
    ),
    file(
        "amdgpu/cyan_skillfish2_pfp.bin",
        263424,
        "50e56dc1913571ff589425d2b059bddfc5e8a2b4bc22d3eb78cac75d1e0479a1",
    ),
    file(
        "amdgpu/cyan_skillfish2_rlc.bin",
        25344,
        "20acefdb6128a36275f4382425a109a7c9927f6053eab685bb51164ff1d18cfb",
    ),
    file(
        "amdgpu/cyan_skillfish2_sdma.bin",
        33792,
        "15d0d3626da7f2513f03b13bb7e02eeeeccab276276ae27d0b6067b5f25e9e95",
    ),
    file(
        "amdgpu/cyan_skillfish2_sdma1.bin",
        33792,
        "bd1c0b0f6a6a4f17ede034f2c844b6553fc444e08915c7bb500c09fde59d6255",
    ),
    file(
        "amdgpu/dcn_3_1_4_dmcub.bin",
        350624,
        "454d3eb73045d0a46f9014c42c76df9e65ae9e5da45b8c331584443fe49a8516",
    ),
    file(
        "amdgpu/dcn_3_1_5_dmcub.bin",
        239648,
        "035015964e5a33514201b50f43cc0643550cbee811be20da9f5e31ef6f405d76",
    ),
    file(
        "amdgpu/dcn_3_1_6_dmcub.bin",
        215568,
        "4bb41cc282d4817b33a62d4bc5ed6787a097a171fd5558498c6f0bc39db7552a",
    ),
    file(
        "amdgpu/dcn_3_2_0_dmcub.bin",
        284432,
        "82713e08816638b8135a3e1b8ce670762a7174c623e10cdcd18ea036604e85bb",
    ),
    file(
        "amdgpu/dcn_3_2_1_dmcub.bin",
        232608,
        "3648f36b7d0ffbb0fa29ddb61b5637d8fc837669093b5bb9098bdc69fed5e010",
    ),
    file(
        "amdgpu/dcn_3_5_1_dmcub.bin",
        522656,
        "dca51a4a697eaa2f79e971b5e04d03892102e1a15caa4d007c8e14bc9d40fb80",
    ),
    file(
        "amdgpu/dcn_3_5_dmcub.bin",
        522784,
        "a111d24fe39e53032aa1eaa5001cfbfafe02133acad346504778077c5a99c0fd",
    ),
    file(
        "amdgpu/dcn_3_6_dmcub.bin",
        536224,
        "48c90b5baa35d7b4cf3614bb58cd167a84965d6252c64705ebfbe9b1136e9046",
    ),
    file(
        "amdgpu/dcn_4_0_1_dmcub.bin",
        368288,
        "74b567655ac89bfe03389b660e973dc3c067aebd464c6389a071662cfbe6237f",
    ),
    file(
        "amdgpu/dcn_4_2_dmcub.bin",
        452688,
        "835de09f844a20e06424fca98ee145c870ca3176286488a11186a71f2ef5b65c",
    ),
    file(
        "amdgpu/dimgrey_cavefish_ce.bin",
        263296,
        "5ca83eb729c449d14a474c1bd5d29cdd3ba331f678a2013c4abf3b56a7b2bccb",
    ),
    file(
        "amdgpu/dimgrey_cavefish_dmcub.bin",
        114164,
        "286458c7911172734dd514631dcfba9690c2242f24f9be72afd6b56c0341974a",
    ),
    file(
        "amdgpu/dimgrey_cavefish_me.bin",
        263424,
        "2a7987abc4e5b358588e3db667096fd8021a0b6e75d79a4b953695561e6361b3",
    ),
    file(
        "amdgpu/dimgrey_cavefish_mec.bin",
        268160,
        "68bda4aa94d7408943080952c881560f853bdb13192857d7363e2d85d135e84c",
    ),
    file(
        "amdgpu/dimgrey_cavefish_mec2.bin",
        268160,
        "68bda4aa94d7408943080952c881560f853bdb13192857d7363e2d85d135e84c",
    ),
    file(
        "amdgpu/dimgrey_cavefish_pfp.bin",
        263424,
        "94a9a434c100209aa0b9d4ff5b023e91943d14a3d73e652b7ae0309d8a6ea8b9",
    ),
    file(
        "amdgpu/dimgrey_cavefish_rlc.bin",
        137232,
        "4ad937c4c2e165acf9ab47dec33a801bc75901b85615921eb23835594ba4eeca",
    ),
    file(
        "amdgpu/dimgrey_cavefish_sdma.bin",
        34048,
        "7f645b4aaed29dab5db54f5dd6ced67e16bffa0e877769f88b4040a8dbe93f9c",
    ),
    file(
        "amdgpu/dimgrey_cavefish_smc.bin",
        244902,
        "cc822ff5d95b8f8fbc0856138c08e462027dcd90c0ac6cf33d5ce34888731d00",
    ),
    file(
        "amdgpu/dimgrey_cavefish_sos.bin",
        210416,
        "ec4ac3c065b814ad164eb6d19f2009c225baed0eb52a7bcbaae324c4b0be11b2",
    ),
    file(
        "amdgpu/dimgrey_cavefish_ta.bin",
        259328,
        "744d1a60888e23af672ce13923dadfe21d35c122e64d4a0629f9bfc26eae832b",
    ),
    file(
        "amdgpu/dimgrey_cavefish_vcn.bin",
        582800,
        "2f3cbd54cc9fad35665c06079f61e175dae3dca34d971bfeb73e4ab3efd7bf65",
    ),
    file(
        "amdgpu/fiji_ce.bin",
        8852,
        "615693b2736f13c4ef3cd9220efe4d55df3c5d82fe128d3f1b34a45edba65fbd",
    ),
    file(
        "amdgpu/fiji_me.bin",
        17044,
        "953747f5b93bd743bb75747b950be3e4ccbe481ac1f7110a58d399ac840f158a",
    ),
    file(
        "amdgpu/fiji_mec.bin",
        262824,
        "cd1133103874ce368c4f46eeb38fe293caad5f77e4fee8567f6f6be9c47687c4",
    ),
    file(
        "amdgpu/fiji_mec2.bin",
        262824,
        "cd1133103874ce368c4f46eeb38fe293caad5f77e4fee8567f6f6be9c47687c4",
    ),
    file(
        "amdgpu/fiji_pfp.bin",
        17044,
        "91bda514a4d0d846d48321aa4d3c92ff1049fe53cbf3e007686553a29a9018de",
    ),
    file(
        "amdgpu/fiji_rlc.bin",
        16636,
        "f0fa903f16502cff35dc073a77c1ef382f4218ec2928f23a173400888f90400d",
    ),
    file(
        "amdgpu/fiji_sdma.bin",
        10644,
        "b5cf6b3a3b7e6839a68a92ac8651d53d0ae41e1caee28c68155b2d7865f1cf4c",
    ),
    file(
        "amdgpu/fiji_sdma1.bin",
        10644,
        "1c5ab71e854cc59e4998559ed07c436d05b2a97b0df0a51a3924b1c240398949",
    ),
    file(
        "amdgpu/fiji_smc.bin",
        129604,
        "fd13fe6b32cef9129f1b75f46b014babcf4075ebc8a715bf19da573be8b68223",
    ),
    file(
        "amdgpu/fiji_uvd.bin",
        266768,
        "b7401cfda1087ee5cf71acef19163f311c71775802b331ca82b9177119e4d97b",
    ),
    file(
        "amdgpu/fiji_vce.bin",
        161024,
        "0fe1a2e4e2e4f6f8d5600d8c13cb60a8bc87089cd37c766fef2f95ccd5e277ac",
    ),
    file(
        "amdgpu/gc_10_3_6_ce.bin",
        263296,
        "ed5fe8488d16ecc0923ff34afe8aa67177d5e1f3242e1de48ef1643b263ba33b",
    ),
    file(
        "amdgpu/gc_10_3_6_me.bin",
        263424,
        "776d2299bc4f3abffd4a7999f5a21a4e38aced8b6b4c199a83610dbabf08176d",
    ),
    file(
        "amdgpu/gc_10_3_6_mec.bin",
        268592,
        "7003c4a77537e9edaf67064104cd9371fac38a84f71f948349140b28d3c210e8",
    ),
    file(
        "amdgpu/gc_10_3_6_mec2.bin",
        268592,
        "7003c4a77537e9edaf67064104cd9371fac38a84f71f948349140b28d3c210e8",
    ),
    file(
        "amdgpu/gc_10_3_6_pfp.bin",
        263424,
        "042f5d2d223aac6a62b500a47d0d0bf33984200110da0ffca4fe5df9a96571c0",
    ),
    file(
        "amdgpu/gc_10_3_6_rlc.bin",
        177104,
        "acfbac75c0dcfbfe40e222640ef17eb3dc8d206d30bc3863f275f2dd1cb132a5",
    ),
    file(
        "amdgpu/gc_10_3_7_ce.bin",
        263296,
        "50415ecc31306104d3f8bf1c19fc7680e1b478f85a48445b4d02ba8dca5576de",
    ),
    file(
        "amdgpu/gc_10_3_7_me.bin",
        263424,
        "9eb0b56e9bcc9dad5d53437b162226fcb37e5df102832260f1232832f3658edf",
    ),
    file(
        "amdgpu/gc_10_3_7_mec.bin",
        268160,
        "ee58a523375bcf5b89400b32b801f95e182b632a26bce4f2bed5c07928d486dc",
    ),
    file(
        "amdgpu/gc_10_3_7_mec2.bin",
        268160,
        "ee58a523375bcf5b89400b32b801f95e182b632a26bce4f2bed5c07928d486dc",
    ),
    file(
        "amdgpu/gc_10_3_7_pfp.bin",
        263424,
        "3ae29aac3f424f7de97f82ce7158beba69509afb2dcbf1a428dc315df474a524",
    ),
    file(
        "amdgpu/gc_10_3_7_rlc.bin",
        177088,
        "a02585ebe3b36d942e883057119572d9497600c52fc65b8a523487eb65d874f2",
    ),
    file(
        "amdgpu/gc_11_0_0_imu.bin",
        132352,
        "b4f8fc056b45709a6abf48e7885fb1b4ab8d3cc092cbfa2c554a78564a6403bc",
    ),
    file(
        "amdgpu/gc_11_0_0_imu_kicker.bin",
        132352,
        "8e8a7bbf49efe00ddb9b9594fba3290d598820c7b8453700dc5e606c5f308755",
    ),
    file(
        "amdgpu/gc_11_0_0_me.bin",
        315120,
        "f274dc44529d749b1a9fa06276b8f32a2968a16ae801896bd11c53b234cc8e1b",
    ),
    file(
        "amdgpu/gc_11_0_0_mec.bin",
        406528,
        "da5903d0d11566f4c808a8b9e8f7abba0a86959610e5f598d508ebd15d1d3ee8",
    ),
    file(
        "amdgpu/gc_11_0_0_mes.bin",
        286336,
        "3b30fd16d1a0df5006afab78d1117ad6b3b2bdc9c01712070ece20648d362dec",
    ),
    file(
        "amdgpu/gc_11_0_0_mes1.bin",
        220480,
        "8740d9a3bcdcd0be0f0482ec7db79007a11a6e8357aa957673286cb13df8dac0",
    ),
    file(
        "amdgpu/gc_11_0_0_mes_2.bin",
        260624,
        "d29eaf0b99af83785f9d8cfe3ce791f0fb1ea426b2f5f0a756a50d3edb06c957",
    ),
    file(
        "amdgpu/gc_11_0_0_pfp.bin",
        232096,
        "d57b34a2bd45c1c944834df16655612f5b3e34f59649cab138cc3095891f61b3",
    ),
    file(
        "amdgpu/gc_11_0_0_rlc.bin",
        185376,
        "dabd49039772d02f5fd5e48dc21d35ad52a6b1283b470dabca86ca159c4c7c8e",
    ),
    file(
        "amdgpu/gc_11_0_0_rlc_1.bin",
        185248,
        "c667e7016261848ce3240f15196ae0f031428ca47b2d759e534653115c235d99",
    ),
    file(
        "amdgpu/gc_11_0_0_rlc_kicker.bin",
        185376,
        "731c63b908749f229896d3fdd0cace97bcd92e622030cf21209d73e98cea9316",
    ),
    file(
        "amdgpu/gc_11_0_1_imu.bin",
        132352,
        "ac71f4eec713fc35b4a1fe27531e3eb04edd81eeac2cef64df01ac50d8510805",
    ),
    file(
        "amdgpu/gc_11_0_1_me.bin",
        263424,
        "62c5e749ad848941e5d8b85f66eeb2b969ee82919724dfd94dbf6e354be8befc",
    ),
    file(
        "amdgpu/gc_11_0_1_mec.bin",
        268160,
        "b0a4006b3934adc1d7996e6d901206dea37203b4839debe4a5ed4e393a615cbb",
    ),
    file(
        "amdgpu/gc_11_0_1_mes.bin",
        287712,
        "4844bb3873ba1c141aa4f1374ab7ceca0c832f769e58efae82e25380bebfd5b9",
    ),
    file(
        "amdgpu/gc_11_0_1_mes1.bin",
        235600,
        "3a8b60ea88b7adbbc71da0ddee0370d5d65c1d66186fd4f4ba9d16a854b24db8",
    ),
    file(
        "amdgpu/gc_11_0_1_mes_2.bin",
        266976,
        "962f56c7e661479e3cabf89c1fcd6344f10cfa8b8d1f672fc445bb21414c1392",
    ),
    file(
        "amdgpu/gc_11_0_1_pfp.bin",
        263424,
        "449c72cae1c57838185197ad4003aeee7ccbdba892a28bb2624d9e85f57a2df1",
    ),
    file(
        "amdgpu/gc_11_0_1_rlc.bin",
        157040,
        "5f07dc1f0a75ecd9cb56d805ea869184a50ed9e43d811ebf833b8906534650ef",
    ),
    file(
        "amdgpu/gc_11_0_2_imu.bin",
        132352,
        "9befca62b0b0cfd252c3df4a9edca295526f4d43821cd99a6326454995a6ca2d",
    ),
    file(
        "amdgpu/gc_11_0_2_me.bin",
        198624,
        "86b48ea19b01a7b5529da3eb1d65f5848b39159f1313c05cddd18feb73dd9651",
    ),
    file(
        "amdgpu/gc_11_0_2_mec.bin",
        272720,
        "eabe3bcfd2dc3520cec7f2205b5666c2e2ef4b5aeda0ac6c02b456b3794d10d7",
    ),
    file(
        "amdgpu/gc_11_0_2_mes.bin",
        286336,
        "e41e0a0a950f55f1bf8fa260972e67e9e06ba1fdf06125dcea46b92dd1d57276",
    ),
    file(
        "amdgpu/gc_11_0_2_mes1.bin",
        172256,
        "ac0db48108090dd64b0f1dd6f9c6fe4e6912ea17432914ec1fc5aaba8be5e4dc",
    ),
    file(
        "amdgpu/gc_11_0_2_mes_2.bin",
        202112,
        "ba078ce093244ac5e086d5968e912cb70676212f3557b164691f62660047d4bb",
    ),
    file(
        "amdgpu/gc_11_0_2_pfp.bin",
        107968,
        "7fb5ae619395caafda0697310cce04a07e8583035dd517c660d43e45cdbe0dc2",
    ),
    file(
        "amdgpu/gc_11_0_2_rlc.bin",
        178848,
        "b43eb2fd0600f50a1a5796bc9983d6b39b5c20960234920f5e89cb362193e0b8",
    ),
    file(
        "amdgpu/gc_11_0_3_imu.bin",
        132352,
        "beaf704d5acdf4623456b0d0cbcea8b8e428058340cd922a259a9045f5c457a3",
    ),
    file(
        "amdgpu/gc_11_0_3_me.bin",
        198624,
        "bd6082c77e46e3a486971fe89387e58176a4bfeb305947178decbbd66d4ceacc",
    ),
    file(
        "amdgpu/gc_11_0_3_mec.bin",
        272720,
        "cbe3923386648d279890b7da481c4fd5f0a40c1851894fe636a1f371cde126ab",
    ),
    file(
        "amdgpu/gc_11_0_3_mes1.bin",
        172064,
        "246284786bfb5d30bbe4a5270c15d1186afffa68be1d7edf3780c7962872e6be",
    ),
    file(
        "amdgpu/gc_11_0_3_mes_2.bin",
        202176,
        "ea46b434b2662dbd4ba84cc4e5c63cec1571b7516b929080e87a113a04e207d7",
    ),
    file(
        "amdgpu/gc_11_0_3_pfp.bin",
        107968,
        "981818a6f611afd24f4f48dacd4ba6ba589f9f97bf36ac64300130ec97052b39",
    ),
    file(
        "amdgpu/gc_11_0_3_rlc.bin",
        179104,
        "890d8e0123efb40c0179dd8ac3e9af073a0b87cbbccfec1db54e5ed2315a8d39",
    ),
    file(
        "amdgpu/gc_11_0_4_imu.bin",
        132352,
        "ac71f4eec713fc35b4a1fe27531e3eb04edd81eeac2cef64df01ac50d8510805",
    ),
    file(
        "amdgpu/gc_11_0_4_me.bin",
        263424,
        "e310c7293da21f8042fb60b867cc97107191e21104e1a73523fa7bc3f1176e5f",
    ),
    file(
        "amdgpu/gc_11_0_4_mec.bin",
        268160,
        "9b73415db92b185450ff769db4f66636794343e9f968335d62fea6c8142f5e38",
    ),
    file(
        "amdgpu/gc_11_0_4_mes.bin",
        287712,
        "4844bb3873ba1c141aa4f1374ab7ceca0c832f769e58efae82e25380bebfd5b9",
    ),
    file(
        "amdgpu/gc_11_0_4_mes1.bin",
        234928,
        "a213a350546abaab3c76fa79271d5e551dc3df56cf8e2cb3d28754bacf508347",
    ),
    file(
        "amdgpu/gc_11_0_4_mes_2.bin",
        266976,
        "c6f3be38e4ce7194f7e9f4d074ac33393ca53616083c526401563669fc79e986",
    ),
    file(
        "amdgpu/gc_11_0_4_pfp.bin",
        263424,
        "e475f7e1afd6d475c164f109446bdb76c37bb02db45b945fe17ceb7a80cd984f",
    ),
    file(
        "amdgpu/gc_11_0_4_rlc.bin",
        159152,
        "257ced82d7bec41249b06592ee0c44fb8f9262de2c6af9c52dc6f6a8a702063e",
    ),
    file(
        "amdgpu/gc_11_5_0_imu.bin",
        132352,
        "469add57cafead90ab1953d6039cd8e39bc50dfd78aa5fd78f019ccf66a0af41",
    ),
    file(
        "amdgpu/gc_11_5_0_me.bin",
        263424,
        "338019a1fcdab39729e3f492ffc9f5970c2c81b12c8a4f431494ca28cfdadedf",
    ),
    file(
        "amdgpu/gc_11_5_0_mec.bin",
        268160,
        "5e022bae6638967d82e2b1077e3024f52bc83b3cb850aa31fba51469c7517c4c",
    ),
    file(
        "amdgpu/gc_11_5_0_mes1.bin",
        238560,
        "c0ae823eb2f3801ba4f0115ff79711ed90a21e224a1f6b430b65ff5496f4225f",
    ),
    file(
        "amdgpu/gc_11_5_0_mes_2.bin",
        266944,
        "f1ba70e8f45f0f1eb1c5a450b766adf5fc4f772c255d49a64682a6577153f5db",
    ),
    file(
        "amdgpu/gc_11_5_0_pfp.bin",
        263424,
        "82ccf0265d841351183b011a79422799431f0c11f6d11165d64d7dfe404bda31",
    ),
    file(
        "amdgpu/gc_11_5_0_rlc.bin",
        166528,
        "0dc8b6ef5530a4a53938c8baa0d49cd458607d95233237859fa98d44feb3e985",
    ),
    file(
        "amdgpu/gc_11_5_1_imu.bin",
        132352,
        "0aaca8a01b2237fca1b3c0cd082b5e12a271df334ff368bbf8e2be17f192b785",
    ),
    file(
        "amdgpu/gc_11_5_1_me.bin",
        263424,
        "4c4dd30c22d4f7f2c5d3a19c645f505e30cdac115a91c65791e2651b22932175",
    ),
    file(
        "amdgpu/gc_11_5_1_mec.bin",
        268160,
        "e49964d5e58686c53e66d98d4e3b9fab70e98fad3b28379c6e60aed03c83ee80",
    ),
    file(
        "amdgpu/gc_11_5_1_mes1.bin",
        238720,
        "02edbd9fe20ff73977046c5159a92187447dd5e9cb11b0a7be6343e2df40db20",
    ),
    file(
        "amdgpu/gc_11_5_1_mes_2.bin",
        267232,
        "588c11015b4bfdd81ff814c96d059552d204942f1beb772aeb5d46d006e35945",
    ),
    file(
        "amdgpu/gc_11_5_1_pfp.bin",
        263424,
        "633404d8db1dc03fe997f7d0d0e15ef908069727abaf9de55841be3f3c97348b",
    ),
    file(
        "amdgpu/gc_11_5_1_rlc.bin",
        161040,
        "92731ecabbeb77865fb71787b4268dc738a58779f1190bdc2056482cb88a08f6",
    ),
    file(
        "amdgpu/gc_11_5_2_imu.bin",
        132352,
        "fb684842839c61a0706a19df8e15eb8afc17e20c14267eb71e7d7d824c180acf",
    ),
    file(
        "amdgpu/gc_11_5_2_me.bin",
        263424,
        "cab2999186d26c0e9a3d46b5a43d2854d88be880cb764c096ad2b43038566384",
    ),
    file(
        "amdgpu/gc_11_5_2_mec.bin",
        268160,
        "9691d7bff5d2c933d8eecb7d171635612a76a2dd1441cffcd65a8a02bdb5a2c5",
    ),
    file(
        "amdgpu/gc_11_5_2_mes1.bin",
        238176,
        "b1bae04df479014e601c8a67ea17befd81ab67fd9300aa604085172ef816b9e8",
    ),
    file(
        "amdgpu/gc_11_5_2_mes_2.bin",
        266944,
        "cb5cd48191809aa8a2290a79d5e3fc2a540b2ed1f57a753c2f0f28915743d1cd",
    ),
    file(
        "amdgpu/gc_11_5_2_pfp.bin",
        263424,
        "baee1456dd1800cdaedd4998c2dd7d76cdc0cf0ec928679fe67b02485905ea2c",
    ),
    file(
        "amdgpu/gc_11_5_2_rlc.bin",
        162096,
        "c9ad70b8ac309257cb8929bb6b4efa6b551ec1e5229d7a419332a9797f31fc9e",
    ),
    file(
        "amdgpu/gc_11_5_3_imu.bin",
        132352,
        "fb684842839c61a0706a19df8e15eb8afc17e20c14267eb71e7d7d824c180acf",
    ),
    file(
        "amdgpu/gc_11_5_3_me.bin",
        263424,
        "94e2d74e834725b3d51e03e830160e95c56f3a31e93f5d61c852afd8fe8cc779",
    ),
    file(
        "amdgpu/gc_11_5_3_mec.bin",
        268160,
        "d368f3886b9245dd0d21d57fccfd8aa7e872c2564e23f292abe735348121277e",
    ),
    file(
        "amdgpu/gc_11_5_3_mes1.bin",
        238176,
        "b1bae04df479014e601c8a67ea17befd81ab67fd9300aa604085172ef816b9e8",
    ),
    file(
        "amdgpu/gc_11_5_3_mes_2.bin",
        266944,
        "cb5cd48191809aa8a2290a79d5e3fc2a540b2ed1f57a753c2f0f28915743d1cd",
    ),
    file(
        "amdgpu/gc_11_5_3_pfp.bin",
        263424,
        "fee840b049b5e082215df72a93fad80a64f07ef6f638408a2d56fae97449a2cb",
    ),
    file(
        "amdgpu/gc_11_5_3_rlc.bin",
        162064,
        "10a68940c6258d5818d9c05fd98eb0ccc8d5aee99b2769fbad30e5abd0d9327e",
    ),
    file(
        "amdgpu/gc_11_7_0_imu.bin",
        132864,
        "778a62f561f0b2bbe66c59326d8ac5788678af11a14242a377f08372f3b25bb3",
    ),
    file(
        "amdgpu/gc_11_7_0_me.bin",
        263680,
        "5199e72c28985a50a2b15b13baf7d7978f563b66beae1b2da3cda7f920b4849d",
    ),
    file(
        "amdgpu/gc_11_7_0_mec.bin",
        268672,
        "d7aa1f49559afda2411acd9079ff9199200e0b89b4dfa5f66d438de7b4be80fc",
    ),
    file(
        "amdgpu/gc_11_7_0_mes1.bin",
        237808,
        "a852c8adf91b4065b9329cdcb92774af12e7e233439d21adb4b7bbab4d6c7568",
    ),
    file(
        "amdgpu/gc_11_7_0_mes_2.bin",
        267200,
        "eaca51268190d1ed997cf96da3f210cadb1baf0eaeb82c9ac52de4b26aaf78cf",
    ),
    file(
        "amdgpu/gc_11_7_0_pfp.bin",
        263680,
        "66a88ac80205ba9b3875265f4aaefdc321972cd622885028fa5d80caa7e21bbd",
    ),
    file(
        "amdgpu/gc_11_7_0_rlc.bin",
        162336,
        "ed76a04a93303cb45c4b5c5af83b898d29327582261c124a6eef7689dd37a31d",
    ),
    file(
        "amdgpu/gc_11_7_1_imu.bin",
        132864,
        "778a62f561f0b2bbe66c59326d8ac5788678af11a14242a377f08372f3b25bb3",
    ),
    file(
        "amdgpu/gc_11_7_1_me.bin",
        263680,
        "5dddec574078b40c2e0c8b00f857185a7cad403fc90eda1a15b54f88fe9ef807",
    ),
    file(
        "amdgpu/gc_11_7_1_mec.bin",
        268672,
        "d9d089696a187431d5a00cb8ab15903c369ed316694f6f06b06577275b28c528",
    ),
    file(
        "amdgpu/gc_11_7_1_mes1.bin",
        237808,
        "a852c8adf91b4065b9329cdcb92774af12e7e233439d21adb4b7bbab4d6c7568",
    ),
    file(
        "amdgpu/gc_11_7_1_mes_2.bin",
        267200,
        "eaca51268190d1ed997cf96da3f210cadb1baf0eaeb82c9ac52de4b26aaf78cf",
    ),
    file(
        "amdgpu/gc_11_7_1_pfp.bin",
        263680,
        "12951177d6a8e5a4f122067306bb593036a2cd5859f777ad25628ce08dd0b308",
    ),
    file(
        "amdgpu/gc_11_7_1_rlc.bin",
        162112,
        "70848cab82ad8bdd80cfb6371233e9cf4afba649bcf5b5b59b8d44c172adddc4",
    ),
    file(
        "amdgpu/gc_12_0_0_imu.bin",
        132352,
        "aa15e5b3156bffc45e0c50bccbcd364fbd3f958531b695b7487a803d780b8328",
    ),
    file(
        "amdgpu/gc_12_0_0_me.bin",
        267568,
        "c288e326b9a5ccccac377455db3c25ace047a42a8e9e5a2239b5554dd2a9a9e7",
    ),
    file(
        "amdgpu/gc_12_0_0_mec.bin",
        259584,
        "9b0788eb46efa7d53a1d87cfe3031cd403ec1eef04314af2a55257883a4560d1",
    ),
    file(
        "amdgpu/gc_12_0_0_mes.bin",
        636048,
        "f8d4abff1de7c7c4be111aba44769e69cdae1819ab34800eb27ae25edd1847db",
    ),
    file(
        "amdgpu/gc_12_0_0_mes1.bin",
        609840,
        "0db493cc00ca19b90278284235e4714f15dc2cc49bb86d276feadeb17a5c6c82",
    ),
    file(
        "amdgpu/gc_12_0_0_pfp.bin",
        159616,
        "cb6eb3ce40c79b0ada234b70ed038983627af78287afd7e5e26123dbcf7e79f8",
    ),
    file(
        "amdgpu/gc_12_0_0_rlc.bin",
        150704,
        "6436b582734a413456fff3d3c7195e71cc9e78a7ed31ee21c83ffd6fae1ad186",
    ),
    file(
        "amdgpu/gc_12_0_0_toc.bin",
        2048,
        "edad661e1c7bd350a86285513fbdeeb33ca4e80c06f1ea7f37d344ba1e2761dd",
    ),
    file(
        "amdgpu/gc_12_0_0_uni_mes.bin",
        299408,
        "309d60ad4a0210f8a68e27cae88beddb22b5ae900d70848b1e8ca0eac9d1decd",
    ),
    file(
        "amdgpu/gc_12_0_1_imu.bin",
        132352,
        "b3b301fb636efc77b63ce4d2ced0f90c851d03c19681852faa45598e6f5773fd",
    ),
    file(
        "amdgpu/gc_12_0_1_imu_kicker.bin",
        132352,
        "874968fa78a0f758f13340b0fbfd0f5edeee124cdd1fb46537bf3ab78bc5c595",
    ),
    file(
        "amdgpu/gc_12_0_1_me.bin",
        267600,
        "fd5eb0f205a3bb7ec41f42e83a54bdc69151706b02bbee073fa775b8270e4bec",
    ),
    file(
        "amdgpu/gc_12_0_1_mec.bin",
        259584,
        "83fe5e7bffe639ec9771219cde5c9809689168f769b147c86dfd5a1ca991c1a2",
    ),
    file(
        "amdgpu/gc_12_0_1_mes.bin",
        643536,
        "306c99718564ff613813b0c15a8e0dd1797488fac01de60702fac0d079c1731d",
    ),
    file(
        "amdgpu/gc_12_0_1_mes1.bin",
        611920,
        "b873c0e354b94e6ae10831dd883013689a4fb04259c4cd46a9b9ddef4c29ddd5",
    ),
    file(
        "amdgpu/gc_12_0_1_pfp.bin",
        159616,
        "490bcd26a62e389b3f58acf5c087d324d2324b3188797ff6a36b7a6cdf1ca9ff",
    ),
    file(
        "amdgpu/gc_12_0_1_rlc.bin",
        150848,
        "6ba4459532246a5c415d3cb33c9b1248294e48f67b827e2accb292a8d1a5c0ec",
    ),
    file(
        "amdgpu/gc_12_0_1_rlc_kicker.bin",
        150848,
        "6d6a7168e3226b70bffa351f651bfd817375c493def8467a3a53c0ae9b4fb3ff",
    ),
    file(
        "amdgpu/gc_12_0_1_toc.bin",
        2048,
        "edad661e1c7bd350a86285513fbdeeb33ca4e80c06f1ea7f37d344ba1e2761dd",
    ),
    file(
        "amdgpu/gc_12_0_1_uni_mes.bin",
        299408,
        "309d60ad4a0210f8a68e27cae88beddb22b5ae900d70848b1e8ca0eac9d1decd",
    ),
    file(
        "amdgpu/gc_9_4_3_mec.bin",
        268736,
        "e95decb3bd17611ea905282b584efee9355dcb172abfb6c4b07767fe41a07265",
    ),
    file(
        "amdgpu/gc_9_4_3_rlc.bin",
        38708,
        "54cbd0de3a0ec35d2e58e992babeee2a237f870ccdf37e734652e4daeeba59d5",
    ),
    file(
        "amdgpu/gc_9_4_3_sjt_mec.bin",
        268736,
        "7c84ebdfc38a405248915ba0b97ac28228bfb0ad79de3c112e78a6f013c306d5",
    ),
    file(
        "amdgpu/gc_9_4_4_mec.bin",
        268736,
        "ec06b093582c48c4bae8200344a7587d162dad9714ea8990a4915d4951c78ed7",
    ),
    file(
        "amdgpu/gc_9_4_4_rlc.bin",
        43388,
        "e0c3585c72f8136670ca63e607fba32c1ae4948f493f13e33fc4d466bd6318a8",
    ),
    file(
        "amdgpu/gc_9_4_4_sjt_mec.bin",
        268736,
        "c4fd17d01e58a761a8422eabdc4253590795f8633e52f913a4d7959ae7531e44",
    ),
    file(
        "amdgpu/gc_9_5_0_mec.bin",
        268736,
        "0c39078c53e10e99538901df5fc14e7f1b1f3639ea825b1b3126ae87a28b2464",
    ),
    file(
        "amdgpu/gc_9_5_0_rlc.bin",
        38700,
        "9b1268f5751153fe57f527c9acb417bfa53ed42c9bc083c9d3da2ba61fe5fdc4",
    ),
    file(
        "amdgpu/green_sardine_asd.bin",
        209408,
        "ebe32baa51bc1fa04dc4df6d7e1f90a3ebb6b0e8931aeb40fc838e0ba2e2c192",
    ),
    file(
        "amdgpu/green_sardine_ce.bin",
        36608,
        "3bba10cff48ec51eb63e78078456596258c9fea2e8b38d91d02be66c0aca70fd",
    ),
    file(
        "amdgpu/green_sardine_dmcub.bin",
        121608,
        "1c9d74c58dd14dacd6d4a330dcf9820b2263d6a7d39fa54c826636a797b4b6a8",
    ),
    file(
        "amdgpu/green_sardine_me.bin",
        69376,
        "671af908c8d631e659895d62db2e6bffc1d2c87d6db4c08246be0216f468fb9c",
    ),
    file(
        "amdgpu/green_sardine_mec.bin",
        268224,
        "0e4c6712f75af494c3fd70386bc07739cab3aafd77c4fe8efb46c0bc1ae4eba6",
    ),
    file(
        "amdgpu/green_sardine_mec2.bin",
        268224,
        "0e4c6712f75af494c3fd70386bc07739cab3aafd77c4fe8efb46c0bc1ae4eba6",
    ),
    file(
        "amdgpu/green_sardine_pfp.bin",
        85760,
        "94e1474b3c8d422e35661d2d584297d891464c173ebf4ad45536277bdf7bdcdc",
    ),
    file(
        "amdgpu/green_sardine_rlc.bin",
        39928,
        "66f4397c2e9d19af245f9da290c8678eb1a51cc815fa7864a9383c2cd0db319d",
    ),
    file(
        "amdgpu/green_sardine_sdma.bin",
        17408,
        "cba8658ea950a99115ca46ee88c9622240632a0ca1731abbcd18b6d9be9e09de",
    ),
    file(
        "amdgpu/green_sardine_ta.bin",
        37632,
        "53ad33b376b85b90f858c8cc3b4bc6c709135b4bd37340ed69b1f00fecd6626b",
    ),
    file(
        "amdgpu/green_sardine_vcn.bin",
        407232,
        "5b32a9f1ee1176babe56910fa1fa52df938b60665febbeab1ce0ee38cb4d5c98",
    ),
    file(
        "amdgpu/navi10_asd.bin",
        209408,
        "e0474d0afa4c284d6429c384fb4a6a88576e302d04444549fe5c2c02be89f665",
    ),
    file(
        "amdgpu/navi10_ce.bin",
        263296,
        "f29dc48a371fc27a41a9c29afbbe4668c5655a73d917c3ff833c2dbbe546fd3a",
    ),
    file(
        "amdgpu/navi10_me.bin",
        263424,
        "44bbf5fa1d0fc04a1b5ebc5159b8139a93c8a5026ffa5801c3ce17ab0a51fd42",
    ),
    file(
        "amdgpu/navi10_mec.bin",
        268160,
        "5883403c6bdfdf10b4ad7870b6111f95fb09374f937f964d7bd9f4bc72743b0f",
    ),
    file(
        "amdgpu/navi10_mec2.bin",
        268160,
        "5883403c6bdfdf10b4ad7870b6111f95fb09374f937f964d7bd9f4bc72743b0f",
    ),
    file(
        "amdgpu/navi10_pfp.bin",
        263424,
        "6245765e4dfd86bff9ae99abdf1829f1d68e64ff71ea4c34d23c6f197f14aba4",
    ),
    file(
        "amdgpu/navi10_rlc.bin",
        43952,
        "1f52478fb78dee904361b93ad67955fcff151bb7a7772bbfbd2c448c814be1f5",
    ),
    file(
        "amdgpu/navi10_sdma.bin",
        33792,
        "1f06e4691b730191ba9e22647c6c4bec231dbe5ca0e1bfcacf35fb7b790119f0",
    ),
    file(
        "amdgpu/navi10_sdma1.bin",
        33792,
        "1f474ed127b33cc41fb610e9c3b276b6bc9688b700d4e2987cdc72603eebc429",
    ),
    file(
        "amdgpu/navi10_smc.bin",
        267970,
        "49ed339d0ff71fe64347380ee12b00c3c69a546f454bcd4323c411c72fe6fb2e",
    ),
    file(
        "amdgpu/navi10_sos.bin",
        184512,
        "3112ea8519545ac89c1623e2e08e841a6a9bc31c2aab2f9b787979e51277e395",
    ),
    file(
        "amdgpu/navi10_ta.bin",
        37632,
        "9e566ad3a6c6de678ea0e77d37e8a1c5630296ea62a1b162e03766a432f5cea4",
    ),
    file(
        "amdgpu/navi10_vcn.bin",
        407232,
        "20cda63482852a8b58e63077fd48a500130642b09a2e0829e1c6edba29ae539b",
    ),
    file(
        "amdgpu/navi12_asd.bin",
        209408,
        "e0474d0afa4c284d6429c384fb4a6a88576e302d04444549fe5c2c02be89f665",
    ),
    file(
        "amdgpu/navi12_ce.bin",
        263296,
        "629cb46be3d7a41047436489a0ff1fb278301d972063edefb8ad6bf1aa23dea4",
    ),
    file(
        "amdgpu/navi12_dmcu.bin",
        23904,
        "e9a720b9cfcd41578c8095a02218e4d3ca152b212cd050a26fcc252c9b94eacf",
    ),
    file(
        "amdgpu/navi12_gpu_info.bin",
        772,
        "9b84a246377405267f24fc4fcd002f2d42dda9ea473fc13e800c359a6efbc3a7",
    ),
    file(
        "amdgpu/navi12_me.bin",
        263424,
        "b03aa30b4dda3173d1bec0cfa30bcc3ad759ea1c19b5d80ee54b2edbc571f4eb",
    ),
    file(
        "amdgpu/navi12_mec.bin",
        268160,
        "71ba78d34879d0292d09221a3a624012e7f24f7d97efbf1549e078b1804a83ec",
    ),
    file(
        "amdgpu/navi12_mec2.bin",
        268160,
        "71ba78d34879d0292d09221a3a624012e7f24f7d97efbf1549e078b1804a83ec",
    ),
    file(
        "amdgpu/navi12_pfp.bin",
        263424,
        "8f977d40665aa1d4fed9f74b0a44f07555a008f125c4e5e6527a10ee51c5d555",
    ),
    file(
        "amdgpu/navi12_rlc.bin",
        43720,
        "c6d790e83db903cde2fd78e635e080fd0e5cd2bfcbf560d1dacd839f0a2c758b",
    ),
    file(
        "amdgpu/navi12_sdma.bin",
        33792,
        "d2b0c674c777912e8581f9275b3555f4fafda09dd957bd5ddaedc438f6ce6a33",
    ),
    file(
        "amdgpu/navi12_sdma1.bin",
        33792,
        "f657869e0a315a33e056955ad5a9496542ac57ef505db94023753cb2fe56acb3",
    ),
    file(
        "amdgpu/navi12_smc.bin",
        264586,
        "b91ec559e28160a96a92ca48d8d7e9ebc7aafdab22857ddda5daf13a5d97a550",
    ),
    file(
        "amdgpu/navi12_sos.bin",
        204656,
        "a93ea08dfe4b6f90b241f713f8268392b2e0340898ab8d4800c02b4ee91a9223",
    ),
    file(
        "amdgpu/navi12_ta.bin",
        37632,
        "9e566ad3a6c6de678ea0e77d37e8a1c5630296ea62a1b162e03766a432f5cea4",
    ),
    file(
        "amdgpu/navi12_vcn.bin",
        407232,
        "20cda63482852a8b58e63077fd48a500130642b09a2e0829e1c6edba29ae539b",
    ),
    file(
        "amdgpu/navi14_asd.bin",
        209408,
        "e0474d0afa4c284d6429c384fb4a6a88576e302d04444549fe5c2c02be89f665",
    ),
    file(
        "amdgpu/navi14_ce.bin",
        263296,
        "4d239c4e8b3e90088955ece6ca80d6e840503f151f9d987c172a76025029fd5f",
    ),
    file(
        "amdgpu/navi14_ce_wks.bin",
        263296,
        "ad71bd45d2633a6edcbc0ad5e3678d89aafd6ac43b534f926631ac10cf9561bf",
    ),
    file(
        "amdgpu/navi14_me.bin",
        263424,
        "bc1288d34084ebca95bd27b882421776c2d0481ec0597733903507a884db55de",
    ),
    file(
        "amdgpu/navi14_me_wks.bin",
        263424,
        "e54cdc80c4200d79e4192770c9525317a793d7ebe4e9aee428aa9f973bc9bb93",
    ),
    file(
        "amdgpu/navi14_mec.bin",
        268160,
        "d2f023d29c361dfe1e0ff37d707dc3aa70b715b2359a2dbcaf5603d6fa1f2c61",
    ),
    file(
        "amdgpu/navi14_mec2.bin",
        268160,
        "d2f023d29c361dfe1e0ff37d707dc3aa70b715b2359a2dbcaf5603d6fa1f2c61",
    ),
    file(
        "amdgpu/navi14_mec2_wks.bin",
        268160,
        "a9de11614b93155160bd871ad02701a5b57f1511b52842b90b55ba08d44e607a",
    ),
    file(
        "amdgpu/navi14_mec_wks.bin",
        268160,
        "a9de11614b93155160bd871ad02701a5b57f1511b52842b90b55ba08d44e607a",
    ),
    file(
        "amdgpu/navi14_pfp.bin",
        263424,
        "2da97a1c3677a2d2f30b277be14ea702b9100fc884d2181c1978ff7e186e911c",
    ),
    file(
        "amdgpu/navi14_pfp_wks.bin",
        263424,
        "4e6af22e38a9ea14194bf61a2ce285dda646c5296ae131205a96e83eefe1ccca",
    ),
    file(
        "amdgpu/navi14_rlc.bin",
        42856,
        "e439e49583634005e0455d78565d97382c2c751e5a1ce079e1c724d4e9f32560",
    ),
    file(
        "amdgpu/navi14_sdma.bin",
        33792,
        "b86560000000a8cb7b1cd0f9f7211d204b547d2849212c7f051b63cba9f177ab",
    ),
    file(
        "amdgpu/navi14_sdma1.bin",
        33792,
        "b69fb6cee714091e026fd39b72386b42718098e3da4cf1e90e748cba23e00144",
    ),
    file(
        "amdgpu/navi14_smc.bin",
        264586,
        "ffffaa9068ff481b653dc129abb13b7723a3d3d480b81685e90d0bf84751ea7a",
    ),
    file(
        "amdgpu/navi14_sos.bin",
        184176,
        "bb1ab24b6f4ce4b537e8555402bfc15f2d07bc46285c8dee8e5c0ba888c41653",
    ),
    file(
        "amdgpu/navi14_ta.bin",
        37632,
        "9e566ad3a6c6de678ea0e77d37e8a1c5630296ea62a1b162e03766a432f5cea4",
    ),
    file(
        "amdgpu/navi14_vcn.bin",
        407232,
        "20cda63482852a8b58e63077fd48a500130642b09a2e0829e1c6edba29ae539b",
    ),
    file(
        "amdgpu/navy_flounder_ce.bin",
        263296,
        "6d62d58b9996135852e33610108906ba3c4f69026f20cfa4f455f0d50af23fca",
    ),
    file(
        "amdgpu/navy_flounder_dmcub.bin",
        114164,
        "286458c7911172734dd514631dcfba9690c2242f24f9be72afd6b56c0341974a",
    ),
    file(
        "amdgpu/navy_flounder_me.bin",
        263424,
        "73c895203323ae1630b2b59688c6df2829499fd18543c3d2f3bfd94c861d4dd3",
    ),
    file(
        "amdgpu/navy_flounder_mec.bin",
        268160,
        "f615b304e42a1ffe30a3f78a4c4dbb1cc28ceaeaa2ca11b1ab3c425a9b03d9e6",
    ),
    file(
        "amdgpu/navy_flounder_mec2.bin",
        268160,
        "f615b304e42a1ffe30a3f78a4c4dbb1cc28ceaeaa2ca11b1ab3c425a9b03d9e6",
    ),
    file(
        "amdgpu/navy_flounder_pfp.bin",
        263424,
        "549cb69a1e270bdb0355271f47b4e3f466fca6c64dd300ba3d4ae4dbb9036108",
    ),
    file(
        "amdgpu/navy_flounder_rlc.bin",
        137336,
        "5a016db2f7c7a00b2e6d09d824dd6d2422d8df74a40a5fe0ee523804008e5284",
    ),
    file(
        "amdgpu/navy_flounder_sdma.bin",
        34048,
        "232a6016e81e62559411de2fcd8532587da3474dd7b480fd8fc09e0db93f6b5d",
    ),
    file(
        "amdgpu/navy_flounder_smc.bin",
        244902,
        "8c9642ce854447f6ec453b1679e34b6c961cd481eaf01f9295d25710b88bdc78",
    ),
    file(
        "amdgpu/navy_flounder_sos.bin",
        218608,
        "bdb7342c92b218dc56ffcfb8cbc06d6f3e1e8eedfe7f4ae27393c42247f34751",
    ),
    file(
        "amdgpu/navy_flounder_ta.bin",
        259328,
        "744d1a60888e23af672ce13923dadfe21d35c122e64d4a0629f9bfc26eae832b",
    ),
    file(
        "amdgpu/navy_flounder_vcn.bin",
        582800,
        "2f3cbd54cc9fad35665c06079f61e175dae3dca34d971bfeb73e4ab3efd7bf65",
    ),
    file(
        "amdgpu/picasso_asd.bin",
        197120,
        "e84bcd8bc22af833006d5d0d1df85dc155d8288dae03dbafc68146106f7ba55c",
    ),
    file(
        "amdgpu/picasso_ce.bin",
        9344,
        "7770a26a59c84048f109765410d64dbfdefc313a448a400b92fabcee3f4eff7a",
    ),
    file(
        "amdgpu/picasso_gpu_info.bin",
        316,
        "69d1efeb5189ede14e4713b6bbf5d08fb26dfabcc46aa3d8aecb6d95f5146854",
    ),
    file(
        "amdgpu/picasso_ip_discovery.bin",
        628,
        "b783460daf796f3a545824f4b7a65ff2255f8542e5f46232d5eece6725e69b41",
    ),
    file(
        "amdgpu/picasso_me.bin",
        17536,
        "2e955e860ccbacbf2163c467da53410b186cebfc1cf30c41b3e6db5a76017a51",
    ),
    file(
        "amdgpu/picasso_mec.bin",
        268064,
        "c51f958b8379d4890fc66251ab3a04ea0b24e8871f2f7b09ad7b41ea22f1d899",
    ),
    file(
        "amdgpu/picasso_mec2.bin",
        268064,
        "c51f958b8379d4890fc66251ab3a04ea0b24e8871f2f7b09ad7b41ea22f1d899",
    ),
    file(
        "amdgpu/picasso_pfp.bin",
        21632,
        "7e05268b13337c3aa1701126f9052f7fdd7dd7d9bb3166c2cee2abd85b9dc183",
    ),
    file(
        "amdgpu/picasso_rlc.bin",
        39140,
        "fac145d69ea654c75d7c0e307cf0ae2ea2aabb5d68977b0b27da2c6fa0f3ae63",
    ),
    file(
        "amdgpu/picasso_rlc_am4.bin",
        39140,
        "919c8a8d0d9fe0bd02129173f235efb63cebbe7014c9e32de088c6485cf61410",
    ),
    file(
        "amdgpu/picasso_sdma.bin",
        17408,
        "c7f19468456f1054fb7858f9bce82ce7245c455fe7e049f9e9397989d104372d",
    ),
    file(
        "amdgpu/picasso_ta.bin",
        46080,
        "93063b85f968b3c72ea03ca7fcfe05fd06374937db3817cfedcb4c61eb2687f2",
    ),
    file(
        "amdgpu/picasso_vcn.bin",
        369376,
        "d376048d547b418effa4f357a4d1cddd418d101874f6a679144ab4df069f3aeb",
    ),
    file(
        "amdgpu/polaris10_ce.bin",
        8852,
        "d1664bd98b69bf21c2fb4e6654860e6648d2a202eeb2175e718982ffb7be1dc5",
    ),
    file(
        "amdgpu/polaris10_ce_2.bin",
        8852,
        "790ca90f5afa001a858909619a222442bfaec1a1f63c47396d21e0097609e24a",
    ),
    file(
        "amdgpu/polaris10_k2_smc.bin",
        130228,
        "b5d34a4b0ea0d0c9c53dddcac93e9e01fe759f0ad9a0b0d0b4905e5a6b254d03",
    ),
    file(
        "amdgpu/polaris10_k_mc.bin",
        32832,
        "78a0e88d669cf91c09cd027ac3b5bd0c08ac126a8488092c567ecda21824e9ad",
    ),
    file(
        "amdgpu/polaris10_k_smc.bin",
        130244,
        "98fa36d74fa933a73419257daae924fda290a7208aa42584a9dd89e1cc44e05d",
    ),
    file(
        "amdgpu/polaris10_mc.bin",
        32732,
        "fff3926e5e5903a87d40d6f6b7c756bd8ecf6fd9c47fced867add1fdc9fe11b6",
    ),
    file(
        "amdgpu/polaris10_me.bin",
        17044,
        "db550bf611ed416a93057b207014ee78b0abe90e5474995b56e915133ff4d1b0",
    ),
    file(
        "amdgpu/polaris10_me_2.bin",
        17044,
        "35c778c82c006a28f129186c414c37d7acbdb23fefb4cb84c1f705886f372fdd",
    ),
    file(
        "amdgpu/polaris10_mec.bin",
        262824,
        "c19c1e2e36b47c4a8c25d601f5ae04a26458d98c3000f851d1be5c3a0a424b07",
    ),
    file(
        "amdgpu/polaris10_mec2.bin",
        262824,
        "c19c1e2e36b47c4a8c25d601f5ae04a26458d98c3000f851d1be5c3a0a424b07",
    ),
    file(
        "amdgpu/polaris10_mec2_2.bin",
        262824,
        "67546c49317168a5ab33ba85503e9893f2dcd974f995402054a306dac246b64f",
    ),
    file(
        "amdgpu/polaris10_mec_2.bin",
        262824,
        "d4914f1166e3b7af212837aaf8f393a155ce3f61f48bb08279c743a2ddea0b6a",
    ),
    file(
        "amdgpu/polaris10_pfp.bin",
        17044,
        "066cf60af4cf266eb302c8d327513159d964e1bef9dacbb65a6b07483bbd440a",
    ),
    file(
        "amdgpu/polaris10_pfp_2.bin",
        17044,
        "bf1ca44ceefd73df8a8e350540d8758d9041e9e2b9cd0bd77e647ed7151ed9eb",
    ),
    file(
        "amdgpu/polaris10_rlc.bin",
        23488,
        "a5d4c2d3acc5c3c4988b95855fe21a5f682552b2b065f76439370835178ccbaf",
    ),
    file(
        "amdgpu/polaris10_sdma.bin",
        12692,
        "e27b3ec976f87e32883b9190e653b20d27f9e77da8dd2d77eeb10c35ee9f1e61",
    ),
    file(
        "amdgpu/polaris10_sdma1.bin",
        12692,
        "23dd73eb1316d40a681841b4d3d116c5cae6d8317cebe8f5e1f9e5dec6de5c87",
    ),
    file(
        "amdgpu/polaris10_smc.bin",
        130216,
        "f7ac568d6e2098a26fc61fc321df1330f9d8952e9b0b47e0309f1d8e6b6727fb",
    ),
    file(
        "amdgpu/polaris10_smc_sk.bin",
        130196,
        "09cc712ec2d30cac29e4b0606c3844d7eae15d6663f73d3f48a695e7324491c1",
    ),
    file(
        "amdgpu/polaris10_uvd.bin",
        375424,
        "e05664b587a1585d98db3097cf4a55119ee4af2da68e65d3d43b3f57065a95fb",
    ),
    file(
        "amdgpu/polaris10_vce.bin",
        166816,
        "3a6bffa5f94054b3593bbe1ad1ba07bde62fa3c4f0734c8cb9f39d901331b810",
    ),
    file(
        "amdgpu/polaris11_ce.bin",
        8852,
        "d1664bd98b69bf21c2fb4e6654860e6648d2a202eeb2175e718982ffb7be1dc5",
    ),
    file(
        "amdgpu/polaris11_ce_2.bin",
        8852,
        "790ca90f5afa001a858909619a222442bfaec1a1f63c47396d21e0097609e24a",
    ),
    file(
        "amdgpu/polaris11_k2_smc.bin",
        130228,
        "83dd69a87fdbba335b266d9970cb51c837f89fce33d24b493a8d4db5909d923a",
    ),
    file(
        "amdgpu/polaris11_k_mc.bin",
        32832,
        "64a2749ae228a5cd2084a877615519a0d18811068f803411545dd9774549b7cf",
    ),
    file(
        "amdgpu/polaris11_k_smc.bin",
        130228,
        "0224af84c74efe8a437ccac8c45f534bca8127ff532cef711ce7157c2ac9bd52",
    ),
    file(
        "amdgpu/polaris11_mc.bin",
        33104,
        "b75c34f2b5866128e007c81d4b077f07f21c724cebe8f19b6db5526a02435b6c",
    ),
    file(
        "amdgpu/polaris11_me.bin",
        17044,
        "784ecf696e9a59b0baec4f010631c32f6bb0971a7965705660ca9244e728992b",
    ),
    file(
        "amdgpu/polaris11_me_2.bin",
        17044,
        "75dbf9d8d38955de4589e6a2fa20ae22815ebbaa7cd4686ea756b5b519db39c4",
    ),
    file(
        "amdgpu/polaris11_mec.bin",
        262824,
        "7ad4a0b49e07f1ac9acf1a468385b6fb40f3cf078d048339ff42fbe896306333",
    ),
    file(
        "amdgpu/polaris11_mec2.bin",
        262824,
        "7ad4a0b49e07f1ac9acf1a468385b6fb40f3cf078d048339ff42fbe896306333",
    ),
    file(
        "amdgpu/polaris11_mec2_2.bin",
        262824,
        "672b3cc2c5cc12d771b73a031a72641072706c23138a3182964e19ab6546a059",
    ),
    file(
        "amdgpu/polaris11_mec_2.bin",
        262824,
        "47a976f67dcfb01287e0e8cc1ae252a1e89c1c816e26a4f5f0c8b48b5cbea754",
    ),
    file(
        "amdgpu/polaris11_pfp.bin",
        17044,
        "ad2ca7d837f09f94491056af88e2d03de55d773a0f86fac238ebb9e13a0a88f8",
    ),
    file(
        "amdgpu/polaris11_pfp_2.bin",
        17044,
        "22bf71e495e2b7d1c05956f59070151b27ac638dcfa6c35d7e71d9d0183cd08b",
    ),
    file(
        "amdgpu/polaris11_rlc.bin",
        22688,
        "29fa9e98117f3ade4471987af3398c9d106df04605f8287f045e0950fe6a1fa1",
    ),
    file(
        "amdgpu/polaris11_sdma.bin",
        12692,
        "e27b3ec976f87e32883b9190e653b20d27f9e77da8dd2d77eeb10c35ee9f1e61",
    ),
    file(
        "amdgpu/polaris11_sdma1.bin",
        12692,
        "23dd73eb1316d40a681841b4d3d116c5cae6d8317cebe8f5e1f9e5dec6de5c87",
    ),
    file(
        "amdgpu/polaris11_smc.bin",
        130196,
        "20142b588ada975151efb15e8f3efde254f6e7a453204d99659c314e43954aca",
    ),
    file(
        "amdgpu/polaris11_smc_sk.bin",
        130196,
        "efa5e768ad6c7e2f754e6bb6b73248d7bd8e7306798eb9054b0f3f7a14223616",
    ),
    file(
        "amdgpu/polaris11_uvd.bin",
        375424,
        "e05664b587a1585d98db3097cf4a55119ee4af2da68e65d3d43b3f57065a95fb",
    ),
    file(
        "amdgpu/polaris11_vce.bin",
        166816,
        "3a6bffa5f94054b3593bbe1ad1ba07bde62fa3c4f0734c8cb9f39d901331b810",
    ),
    file(
        "amdgpu/polaris12_32_mc.bin",
        32876,
        "555a01b4833c9cb62da126f80fb7c5ce15a7ef827ea6b8bc0f725c2a8f4b0ce9",
    ),
    file(
        "amdgpu/polaris12_ce.bin",
        8852,
        "5f051f19c5749ecd5daa94d9f542a25cd693274af6e394da048b1a0469bac87e",
    ),
    file(
        "amdgpu/polaris12_ce_2.bin",
        8852,
        "a4758bf9e8286d9bae39a59c1f579a0a68c398a37f4720c0c70392461d2d19fe",
    ),
    file(
        "amdgpu/polaris12_k_mc.bin",
        32832,
        "d7afe2e7d7800ae9439111e7fe4bc436c4ccf45ab23dcdef5deb02a4a8a56cc4",
    ),
    file(
        "amdgpu/polaris12_k_smc.bin",
        130388,
        "3ea2727dad6e11846565410fc5ed8cab133b788f5e24475bfdca4e68e86daff0",
    ),
    file(
        "amdgpu/polaris12_mc.bin",
        32608,
        "b0fe44950422879f0509fb62a7f7aa0e75c7f4ddfe6257a8864b2049e72fae78",
    ),
    file(
        "amdgpu/polaris12_me.bin",
        17044,
        "d18fd8dda14e0e6527d119866cf74aab143c6d70f6ad0906d6f3b391d79525ce",
    ),
    file(
        "amdgpu/polaris12_me_2.bin",
        17044,
        "8953b50da0d365e79b6bec1253e9ad56097451718de01ddaf8d4683b46e4a13c",
    ),
    file(
        "amdgpu/polaris12_mec.bin",
        262824,
        "799b3e73fd260b6312821db3f4344be7874824a846511760f9746baebd3832f0",
    ),
    file(
        "amdgpu/polaris12_mec2.bin",
        262824,
        "799b3e73fd260b6312821db3f4344be7874824a846511760f9746baebd3832f0",
    ),
    file(
        "amdgpu/polaris12_mec2_2.bin",
        262824,
        "11dc9315750b09ef43b5c3d3d59f078b47d054764ad0ce985ec4a3a484f25d9a",
    ),
    file(
        "amdgpu/polaris12_mec_2.bin",
        262824,
        "1e0ce977d29288e3f499b0ab0f5e8af43f5cabbef4b36e067a086a976f6e9636",
    ),
    file(
        "amdgpu/polaris12_pfp.bin",
        17044,
        "93506a0a72a45b6bb48dd06149a6eebf96d36991f32dc4ed67d3682cda920e22",
    ),
    file(
        "amdgpu/polaris12_pfp_2.bin",
        17044,
        "7b80b023b18010fa66ef3af5a19fa556bcee4d72d5eaf5b6aacc263e7c7b3706",
    ),
    file(
        "amdgpu/polaris12_rlc.bin",
        16660,
        "f087f8585ec1d4bc12b48caee0adeb9c170aacd99b8ef1505dbb4ad5ccfbb8c1",
    ),
    file(
        "amdgpu/polaris12_sdma.bin",
        12692,
        "e27b3ec976f87e32883b9190e653b20d27f9e77da8dd2d77eeb10c35ee9f1e61",
    ),
    file(
        "amdgpu/polaris12_sdma1.bin",
        12692,
        "23dd73eb1316d40a681841b4d3d116c5cae6d8317cebe8f5e1f9e5dec6de5c87",
    ),
    file(
        "amdgpu/polaris12_smc.bin",
        130388,
        "4b8eb326a7fbd391554fe50bc43cf04a917b01a806884ea324f5c3fe88a6ec77",
    ),
    file(
        "amdgpu/polaris12_uvd.bin",
        375424,
        "e05664b587a1585d98db3097cf4a55119ee4af2da68e65d3d43b3f57065a95fb",
    ),
    file(
        "amdgpu/polaris12_vce.bin",
        166816,
        "3a6bffa5f94054b3593bbe1ad1ba07bde62fa3c4f0734c8cb9f39d901331b810",
    ),
    file(
        "amdgpu/psp_13_0_0_sos.bin",
        272400,
        "4a51299f6d0a15bbba9694419f7891e6accc01dbd2dd67c06add7bfd75a45ac6",
    ),
    file(
        "amdgpu/psp_13_0_0_sos_kicker.bin",
        273072,
        "7052ea225f0aca9917ce63dd79b681fdd7717b364aa5a1fa042db59efe7a2bf6",
    ),
    file(
        "amdgpu/psp_13_0_0_ta.bin",
        271616,
        "6bed2492c65a078a03cf28239f6edfe70909e22598e83b0187027ae1ee721087",
    ),
    file(
        "amdgpu/psp_13_0_0_ta_kicker.bin",
        259328,
        "e97903a17aba9955052d52efdb2cbe78f7a61138bd6bbd410f2031a799382141",
    ),
    file(
        "amdgpu/psp_13_0_10_sos.bin",
        360352,
        "0bcaaad9cd8578d3841ae69155a6bd4fc3ceae8f4fb5a6ba4f576e7ace94d1d9",
    ),
    file(
        "amdgpu/psp_13_0_10_ta.bin",
        267712,
        "821f632367ed2de893bdb1ef49be6274016463e8b5babf2ad43dff30f062327c",
    ),
    file(
        "amdgpu/psp_13_0_11_ta.bin",
        254976,
        "f4a2687e1cd940b016e308bc87b9b840f845526d9ef21147416d89c1c72a7944",
    ),
    file(
        "amdgpu/psp_13_0_11_toc.bin",
        2560,
        "11e3529d748e2b6b204681183360a6ead80665b8deda996dfcf35973f789fc3d",
    ),
    file(
        "amdgpu/psp_13_0_12_sos.bin",
        455472,
        "7113a165c75c232d4cb7193a920b503e0bf082689adde3b45fdc38f58bfd18b3",
    ),
    file(
        "amdgpu/psp_13_0_12_ta.bin",
        70976,
        "ba427a5e02cb6bc36a179fe04f5e106cfa5b352eb297f041717b7f53728a6a02",
    ),
    file(
        "amdgpu/psp_13_0_14_sos.bin",
        405824,
        "db863768cb25e806b68033e9237e0869f9f3603119df4d369ff4d80418d585d0",
    ),
    file(
        "amdgpu/psp_13_0_14_ta.bin",
        67904,
        "41491b6750574aede3e2d24b4ea800d4e993dcb1ad3773e8c9c856fc484912db",
    ),
    file(
        "amdgpu/psp_13_0_15_sos.bin",
        439088,
        "3b28d53e75a88131155e3931378ac8434eca4880ada9211d3b4e8915b6289583",
    ),
    file(
        "amdgpu/psp_13_0_15_ta.bin",
        49920,
        "307637c780b5dafa0f87f68499c0cc054d08afba0eea9b4081541087419606ee",
    ),
    file(
        "amdgpu/psp_13_0_4_ta.bin",
        254976,
        "f4a2687e1cd940b016e308bc87b9b840f845526d9ef21147416d89c1c72a7944",
    ),
    file(
        "amdgpu/psp_13_0_4_toc.bin",
        2560,
        "11e3529d748e2b6b204681183360a6ead80665b8deda996dfcf35973f789fc3d",
    ),
    file(
        "amdgpu/psp_13_0_5_ta.bin",
        246784,
        "0b7fc5a0bea6e3559df9791a57ae5a648ec6246679f8f8a6bc27defb2aa7a75e",
    ),
    file(
        "amdgpu/psp_13_0_5_toc.bin",
        1792,
        "2dfcc2b4d6b7f072a471da3852164560ae709bb563626997c41ef1ef17588bbe",
    ),
    file(
        "amdgpu/psp_13_0_6_sos.bin",
        787040,
        "17e448f91942db9d5fa32396d8f8928fe06ea56983668ccfe14723f1660df4dd",
    ),
    file(
        "amdgpu/psp_13_0_6_ta.bin",
        88960,
        "712567e725c1704a28967a8b93cf64756252c7f75a969d9d5d09b5820a7f0e2a",
    ),
    file(
        "amdgpu/psp_13_0_7_sos.bin",
        248496,
        "94db505fa6482f258c33a0a8d412050f6d843ab4ada368252e988f82f8a26fa8",
    ),
    file(
        "amdgpu/psp_13_0_7_ta.bin",
        246784,
        "0b7fc5a0bea6e3559df9791a57ae5a648ec6246679f8f8a6bc27defb2aa7a75e",
    ),
    file(
        "amdgpu/psp_13_0_8_ta.bin",
        234496,
        "13f92bd6d923849da20a5183cff0d3db6cd6e7c5214833c9e254275fb1db4815",
    ),
    file(
        "amdgpu/psp_13_0_8_toc.bin",
        1792,
        "d1cde732906d11bca2b8d7df3c3d4bcb5696501f7e570ceab8504a79dc94962f",
    ),
    file(
        "amdgpu/psp_14_0_0_ta.bin",
        254976,
        "f4a2687e1cd940b016e308bc87b9b840f845526d9ef21147416d89c1c72a7944",
    ),
    file(
        "amdgpu/psp_14_0_0_toc.bin",
        2560,
        "da82e510ba90b3939656ecd2092870e5eef1c1b71ad2a4b776bc9e57bae1f05f",
    ),
    file(
        "amdgpu/psp_14_0_1_ta.bin",
        254976,
        "f4a2687e1cd940b016e308bc87b9b840f845526d9ef21147416d89c1c72a7944",
    ),
    file(
        "amdgpu/psp_14_0_1_toc.bin",
        2560,
        "c629a9996fbdd487bd08d7d163fc953e03d197faee56d0984b7ca9875de25cb0",
    ),
    file(
        "amdgpu/psp_14_0_2_sos.bin",
        421216,
        "7b538448b57d4f9dd06b2eea90d4f86a16e65e3027cdecee8db71c2c5f1fa243",
    ),
    file(
        "amdgpu/psp_14_0_2_ta.bin",
        278764,
        "21b8898e56524240c19a1f834fcea3dde7d98c33044658a1641f0cb3c3b1bddd",
    ),
    file(
        "amdgpu/psp_14_0_3_sos.bin",
        435632,
        "28469a0857c813c54a0492423cdf0b0caf757428400036377e19c47e5af62478",
    ),
    file(
        "amdgpu/psp_14_0_3_sos_kicker.bin",
        435968,
        "1406eb4c19f2e7a11fa4426736f4f4871e4d9f1f7bd7e569e9cf04062edaad69",
    ),
    file(
        "amdgpu/psp_14_0_3_ta.bin",
        291372,
        "fff8b0b56f21bf1d34c8d1fcf6cc903c2dbb5ef88eb06157c9f08baf95124883",
    ),
    file(
        "amdgpu/psp_14_0_3_ta_kicker.bin",
        270624,
        "b3dbe9a69dfb3df071856927f686c0c8329f338c7d445bb11535cb7509317093",
    ),
    file(
        "amdgpu/psp_14_0_4_ta.bin",
        254976,
        "f4a2687e1cd940b016e308bc87b9b840f845526d9ef21147416d89c1c72a7944",
    ),
    file(
        "amdgpu/psp_14_0_4_toc.bin",
        2560,
        "bbb69c7d83f4400abdd644a9f16238ed0d35d453a2fcf57796ae70e1cd258d7d",
    ),
    file(
        "amdgpu/psp_14_0_5_ta.bin",
        254976,
        "f4a2687e1cd940b016e308bc87b9b840f845526d9ef21147416d89c1c72a7944",
    ),
    file(
        "amdgpu/psp_14_0_5_toc.bin",
        2560,
        "3f9c34ea742e3f7b71860a99113edbe95104a40cf80ca1f44dd5c7edb1a61242",
    ),
    file(
        "amdgpu/psp_15_0_0_ta.bin",
        283312,
        "a172c84a133506ee3e5e56616cfc3fbadf7d36f70b1fed427440e779e364f1a8",
    ),
    file(
        "amdgpu/psp_15_0_0_toc.bin",
        2816,
        "13d73170dcc248eb750a707a2c7f9a8b4c83f527b4c3b3f7e82401cd7cb11334",
    ),
    file(
        "amdgpu/psp_15_0_9_ta.bin",
        283312,
        "a172c84a133506ee3e5e56616cfc3fbadf7d36f70b1fed427440e779e364f1a8",
    ),
    file(
        "amdgpu/psp_15_0_9_toc.bin",
        2816,
        "3851953da81ec927265980b2ef2436729f5999152080581c9e051c5606424c8a",
    ),
    file(
        "amdgpu/raven2_asd.bin",
        193024,
        "3594c98bd86656e223c3bea55dc040ace63c349caaf792f103bd66a8728c57c1",
    ),
    file(
        "amdgpu/raven2_ce.bin",
        9344,
        "eb6f47850eccb751cb5f949c09550fc28b5ff8802c36d3b952a5a36c5d342098",
    ),
    file(
        "amdgpu/raven2_gpu_info.bin",
        316,
        "73e76d7e99a0fd5f63ed7d7fe8c91bc8fdd9b320cafdb7915b873f2851cdc576",
    ),
    file(
        "amdgpu/raven2_ip_discovery.bin",
        628,
        "3686618ca031c4284d22227a99454a28dc42e7371505a70c994c09d6e45f5c6e",
    ),
    file(
        "amdgpu/raven2_me.bin",
        17536,
        "1d5f3b34fdd27735dce1b62b20dfceadd98eaeb94f95684c095cc2ab6d2b6864",
    ),
    file(
        "amdgpu/raven2_mec.bin",
        268064,
        "d1e4d3a766e8e7a539431fdc84ad9b87cb51817a644bdc0c616ef782e62696c0",
    ),
    file(
        "amdgpu/raven2_mec2.bin",
        268064,
        "d1e4d3a766e8e7a539431fdc84ad9b87cb51817a644bdc0c616ef782e62696c0",
    ),
    file(
        "amdgpu/raven2_pfp.bin",
        21632,
        "29ec75eb4599bfd9a483d90c9b3deb7f1448d014974e387189a5e54f91a7ec28",
    ),
    file(
        "amdgpu/raven2_rlc.bin",
        38324,
        "370b453daf80ae22b40451b98454a4a2c9d1dd3168a5ab664509a25493070e73",
    ),
    file(
        "amdgpu/raven2_sdma.bin",
        17408,
        "d15abea9f0060f7d8771013176c69dfe6796bd2fa29b59a3fd7796cf12362187",
    ),
    file(
        "amdgpu/raven2_ta.bin",
        33536,
        "bc7a8b0fe15faa97fa6f7822263218ca94b387c16304bad7d674c166899a3a97",
    ),
    file(
        "amdgpu/raven2_vcn.bin",
        369376,
        "d376048d547b418effa4f357a4d1cddd418d101874f6a679144ab4df069f3aeb",
    ),
    file(
        "amdgpu/raven_asd.bin",
        193024,
        "d14657147433c7d1d94b90b0ffa2e1e08e23c1af78a237d104bf8c6320166b11",
    ),
    file(
        "amdgpu/raven_ce.bin",
        9344,
        "7770a26a59c84048f109765410d64dbfdefc313a448a400b92fabcee3f4eff7a",
    ),
    file(
        "amdgpu/raven_dmcu.bin",
        23184,
        "4b6a16833a49c6a08c46c37ba1f36f826d75eb9c7c5877bb9dde1fb3b5833e5a",
    ),
    file(
        "amdgpu/raven_gpu_info.bin",
        316,
        "69d1efeb5189ede14e4713b6bbf5d08fb26dfabcc46aa3d8aecb6d95f5146854",
    ),
    file(
        "amdgpu/raven_ip_discovery.bin",
        628,
        "b783460daf796f3a545824f4b7a65ff2255f8542e5f46232d5eece6725e69b41",
    ),
    file(
        "amdgpu/raven_kicker_rlc.bin",
        39140,
        "fac145d69ea654c75d7c0e307cf0ae2ea2aabb5d68977b0b27da2c6fa0f3ae63",
    ),
    file(
        "amdgpu/raven_me.bin",
        17536,
        "2e955e860ccbacbf2163c467da53410b186cebfc1cf30c41b3e6db5a76017a51",
    ),
    file(
        "amdgpu/raven_mec.bin",
        268064,
        "c51f958b8379d4890fc66251ab3a04ea0b24e8871f2f7b09ad7b41ea22f1d899",
    ),
    file(
        "amdgpu/raven_mec2.bin",
        268064,
        "c51f958b8379d4890fc66251ab3a04ea0b24e8871f2f7b09ad7b41ea22f1d899",
    ),
    file(
        "amdgpu/raven_pfp.bin",
        21632,
        "7e05268b13337c3aa1701126f9052f7fdd7dd7d9bb3166c2cee2abd85b9dc183",
    ),
    file(
        "amdgpu/raven_rlc.bin",
        39140,
        "4bd15ce9e27af4e0e6bcfac44ba3b219816073f88990469b5f807d8e33bd1479",
    ),
    file(
        "amdgpu/raven_sdma.bin",
        17408,
        "c7f19468456f1054fb7858f9bce82ce7245c455fe7e049f9e9397989d104372d",
    ),
    file(
        "amdgpu/raven_ta.bin",
        33536,
        "1a069344915518494bc9ebd33ea80fd1838e0d533630e84ebed59e8b853c6b8e",
    ),
    file(
        "amdgpu/raven_vcn.bin",
        369376,
        "d376048d547b418effa4f357a4d1cddd418d101874f6a679144ab4df069f3aeb",
    ),
    file(
        "amdgpu/renoir_asd.bin",
        209408,
        "ebe32baa51bc1fa04dc4df6d7e1f90a3ebb6b0e8931aeb40fc838e0ba2e2c192",
    ),
    file(
        "amdgpu/renoir_ce.bin",
        9344,
        "d98bbdfb47fc6134622ebc1f69ae0494dd3c1a5119a0b63d22f0c01bfe60b78e",
    ),
    file(
        "amdgpu/renoir_dmcub.bin",
        121608,
        "1c9d74c58dd14dacd6d4a330dcf9820b2263d6a7d39fa54c826636a797b4b6a8",
    ),
    file(
        "amdgpu/renoir_me.bin",
        17536,
        "f066821b6703dd3bbc203db7440978e274cd985da2f6b42980ddc41a7ebb0233",
    ),
    file(
        "amdgpu/renoir_mec.bin",
        268224,
        "0e4c6712f75af494c3fd70386bc07739cab3aafd77c4fe8efb46c0bc1ae4eba6",
    ),
    file(
        "amdgpu/renoir_pfp.bin",
        21632,
        "6551ea996a298fb9dd2072cdaa140187ee8e0b9f220bbd691fbe23c094a87539",
    ),
    file(
        "amdgpu/renoir_rlc.bin",
        39928,
        "66f4397c2e9d19af245f9da290c8678eb1a51cc815fa7864a9383c2cd0db319d",
    ),
    file(
        "amdgpu/renoir_sdma.bin",
        17408,
        "cba8658ea950a99115ca46ee88c9622240632a0ca1731abbcd18b6d9be9e09de",
    ),
    file(
        "amdgpu/renoir_ta.bin",
        46080,
        "6a35ce07ed4b19d3e9929d0c34266bc38e604d28a130f1df4fd296c510c6811e",
    ),
    file(
        "amdgpu/renoir_vcn.bin",
        407232,
        "5b32a9f1ee1176babe56910fa1fa52df938b60665febbeab1ce0ee38cb4d5c98",
    ),
    file(
        "amdgpu/sdma_4_4_2.bin",
        34304,
        "ff885711d2d5d75ceed51cf239e93c882584b918cd5d5d1ff58ee5aecc0c50ae",
    ),
    file(
        "amdgpu/sdma_4_4_4.bin",
        34304,
        "06a9d4d02c187844313a78469321d6091e59a334f0ce3b61d770d810c984d70b",
    ),
    file(
        "amdgpu/sdma_4_4_5.bin",
        34304,
        "e2a30faa9403933fbfba7ce8e9feba460fff6ecdb15304818d24c9f3eeaad0a6",
    ),
    file(
        "amdgpu/sdma_5_2_6.bin",
        34048,
        "3a163db00eb7e4752be8adbd61cf7dd8f08d924e59a6f798ced7dfcd89f340ed",
    ),
    file(
        "amdgpu/sdma_5_2_7.bin",
        34048,
        "16fe80dc866b323e15a06f51646ef0f036878ad34da66921fcdb8167207d6b2b",
    ),
    file(
        "amdgpu/sdma_6_0_0.bin",
        34560,
        "82cd01a24171af12de6c7ac4ee7471aa2dfcf51f9677e7bae9cd4c75e07761ee",
    ),
    file(
        "amdgpu/sdma_6_0_1.bin",
        34560,
        "708c2c2c45262c98ebe8e34e32c3f1ce8eb5b007bab560c9ea9b576a8e4d6768",
    ),
    file(
        "amdgpu/sdma_6_0_2.bin",
        34560,
        "16c374344d2894da751f7028f9ec1f7520035fa9548d8c750d99a00a4afa86c7",
    ),
    file(
        "amdgpu/sdma_6_0_3.bin",
        34560,
        "d47ac4db523aa35d77b27d57c35d4c04f431229dec6c0d667c36d98b985a6933",
    ),
    file(
        "amdgpu/sdma_6_1_0.bin",
        34560,
        "85f9f3714de68eee74cdf4852d709bc36a5c73a00e943b707bc2ce10d6b7bab4",
    ),
    file(
        "amdgpu/sdma_6_1_1.bin",
        34560,
        "e7b7a23923ab691665e6ad16bbc8431a92f7c049da4b0b19a82c45fba03d4979",
    ),
    file(
        "amdgpu/sdma_6_1_2.bin",
        34560,
        "5947d78eb308a3f6a62d772c5a6493b21439c73eac139f9c22f080f660b4f4f3",
    ),
    file(
        "amdgpu/sdma_6_1_3.bin",
        34560,
        "8c651f32cbf030b6239ecc44f01bc9f5d5a193f333e21f2103736aff33227361",
    ),
    file(
        "amdgpu/sdma_6_1_4.bin",
        35072,
        "577f3c3c5845d5627fd00b558395f64d38fce4af598fb3183474fd2e92ead767",
    ),
    file(
        "amdgpu/sdma_7_0_0.bin",
        47872,
        "beaafb53993a106edd392392d5896245ae2a957c6d0f495d0002eec72ad8ad38",
    ),
    file(
        "amdgpu/sdma_7_0_1.bin",
        47104,
        "73c29e1c1714ebc95d2221ba56e187910902891593010653bf9518937e414a59",
    ),
    file(
        "amdgpu/sienna_cichlid_ce.bin",
        263296,
        "156769265ed26c8cb14902a13e14871732245daeffb793bdf394c1bd3de76f00",
    ),
    file(
        "amdgpu/sienna_cichlid_dmcub.bin",
        114164,
        "286458c7911172734dd514631dcfba9690c2242f24f9be72afd6b56c0341974a",
    ),
    file(
        "amdgpu/sienna_cichlid_me.bin",
        263424,
        "fbdf0e27e1c232e5b0a69f52efd5692a1803ec2a0d77201ea678dc8bc6407116",
    ),
    file(
        "amdgpu/sienna_cichlid_mec.bin",
        268160,
        "ce7442ce45828e5726bb7b05d1fba7f228dd184f5c0a9daf6647a025540d7d39",
    ),
    file(
        "amdgpu/sienna_cichlid_mec2.bin",
        268160,
        "ce7442ce45828e5726bb7b05d1fba7f228dd184f5c0a9daf6647a025540d7d39",
    ),
    file(
        "amdgpu/sienna_cichlid_pfp.bin",
        263424,
        "fa904ab1609ab090d90edc0d11132fc9821a05800466b04a193c62d5a5c2d5ae",
    ),
    file(
        "amdgpu/sienna_cichlid_rlc.bin",
        132960,
        "19d19bce396b6dc725acb87cdf419e6151a1b2961ff43d6e026e17ecf20c83d0",
    ),
    file(
        "amdgpu/sienna_cichlid_sdma.bin",
        34048,
        "1aad9d95e1205d4a0c8da01670aa9fc6f77010ac04a321d8c644f581ada6c7ce",
    ),
    file(
        "amdgpu/sienna_cichlid_smc.bin",
        247396,
        "0eb99764288f764fd853d6973693ffa52419cc7ba62074aee85df4a3e8d0286d",
    ),
    file(
        "amdgpu/sienna_cichlid_sos.bin",
        227456,
        "67706b9e1545bf68850bf9cea97660af5972574ecb0ca30041174efbb59fac37",
    ),
    file(
        "amdgpu/sienna_cichlid_ta.bin",
        386816,
        "dc28d6a9abc3e3a1e9e8c4f787fc179a3e02bd1d5d6a72f178f88b1ad5e61569",
    ),
    file(
        "amdgpu/sienna_cichlid_vcn.bin",
        582800,
        "2f3cbd54cc9fad35665c06079f61e175dae3dca34d971bfeb73e4ab3efd7bf65",
    ),
    file(
        "amdgpu/smu_13_0_0.bin",
        292452,
        "3551e1ef1f0cf542d1a728bdff123142e7cf86b550974d229668b01804c3992d",
    ),
    file(
        "amdgpu/smu_13_0_0_kicker.bin",
        261888,
        "fb0764b94e3a63b318ba13dfdf684d37c9f1823ce0595e70983ecf029f99a042",
    ),
    file(
        "amdgpu/smu_13_0_10.bin",
        271520,
        "d1c59c86d7d2b021218fa6a5c24a64057d39a86e115c8be4b99938499baae28e",
    ),
    file(
        "amdgpu/smu_13_0_14.bin",
        1420,
        "a4f36de75fdcecd8000246762e027b4be489b6787afea57675225b0b39d35625",
    ),
    file(
        "amdgpu/smu_13_0_6.bin",
        3748,
        "ad7232264e8c57c2094244fbdd5a55d7a4575ffe9b44d229884bc0b6a44fb0b1",
    ),
    file(
        "amdgpu/smu_13_0_7.bin",
        287080,
        "6046f6379f42b4d3618151eace413d8c076ea374944afb770a8871ad79fcec55",
    ),
    file(
        "amdgpu/smu_14_0_2.bin",
        333236,
        "1b2054e3f710d1ab8dbdf6ff35914ad376b51caa6337831260c955add874b2ee",
    ),
    file(
        "amdgpu/smu_14_0_3.bin",
        333236,
        "7624724231b0986b59f59705e7c854c482193f6b3b591ebdda58cb62af81c010",
    ),
    file(
        "amdgpu/smu_14_0_3_kicker.bin",
        327424,
        "c9b0cbef51cc2bfcf28238b904b3e9e089e3e43b226b2bae9d5fcd7be76253e5",
    ),
    file(
        "amdgpu/stoney_ce.bin",
        8832,
        "fc3938a445fd744aab16523db079781afe0d064dcb416569a2c243fc256550ca",
    ),
    file(
        "amdgpu/stoney_me.bin",
        17024,
        "ad9ff687fe0bf670620ff6317382583bb2240ce65a866c02edb24a8697e47493",
    ),
    file(
        "amdgpu/stoney_mec.bin",
        262784,
        "a1c84d90fbf5fcd0b8186b43c0246455dd8c66237ff6c2a03481276ac16fc49a",
    ),
    file(
        "amdgpu/stoney_pfp.bin",
        17024,
        "0812c914e5daad0d65ae04651dbe6448004ca726dd18e478bfc45f354b46e8fc",
    ),
    file(
        "amdgpu/stoney_rlc.bin",
        17236,
        "ab7d3f9c47d9763795d267a0d51722c89f5a2042aba232db575f4c86eb5638c6",
    ),
    file(
        "amdgpu/stoney_sdma.bin",
        10624,
        "c23d03726ffdfb1fe253cfbfe36ab82ce5e69bede0d6c4157de71edb967a5fbb",
    ),
    file(
        "amdgpu/stoney_uvd.bin",
        277216,
        "f7474c77bd185b6c2df3c7d24cb656e8e0edb24f1e296cb09447e3e41501a498",
    ),
    file(
        "amdgpu/stoney_vce.bin",
        166816,
        "e763aa4f281bc7b2f8b435bb95601e691cc1a7737c10aaa27e59a9b15e94cfdb",
    ),
    file(
        "amdgpu/tonga_ce.bin",
        8852,
        "b4af0f8d8d3600f4b91b3e7e3021f7fa77eb539bbf6346d4e07d0b47311f6bf7",
    ),
    file(
        "amdgpu/tonga_k_smc.bin",
        130460,
        "9669b9e40d794dc4360c0a3d562cef715695b398a29d518378202183fc89b5b8",
    ),
    file(
        "amdgpu/tonga_mc.bin",
        31352,
        "c75b7389ba488acb751690b04b383d64358d7ead4d155380442ad691e73f77b5",
    ),
    file(
        "amdgpu/tonga_me.bin",
        17044,
        "c350d80cd2ce91f90606ef64e241c70e630056d964a602897cbd1c33758090b7",
    ),
    file(
        "amdgpu/tonga_mec.bin",
        262824,
        "101ae242c9d4811af89fe550d9272c2abfc672bd95ee6625b5823a9b3d14369f",
    ),
    file(
        "amdgpu/tonga_mec2.bin",
        262824,
        "101ae242c9d4811af89fe550d9272c2abfc672bd95ee6625b5823a9b3d14369f",
    ),
    file(
        "amdgpu/tonga_pfp.bin",
        17044,
        "d8735c931a26ded131afc998d4288c012687e1f266b8e8fd010dfe18b5cc9e92",
    ),
    file(
        "amdgpu/tonga_rlc.bin",
        14756,
        "2ae481966852b20215ced869f45b4eefd8c78db7302ecee0c9b23265d837dd86",
    ),
    file(
        "amdgpu/tonga_sdma.bin",
        10644,
        "6f5c9c79bdfd263dc95e83cdccc6ff0e262b7d0f08ef02cff8e8f43aa0f0384d",
    ),
    file(
        "amdgpu/tonga_sdma1.bin",
        10644,
        "3db6b4581bf597648d8ca030624cedad0782ecea0072586240a4daaad9ed13ab",
    ),
    file(
        "amdgpu/tonga_smc.bin",
        130460,
        "bc76b51ddf10bcb858944d78e526e31ec4880f6c99df6bf7c357b019f4d7b56c",
    ),
    file(
        "amdgpu/tonga_uvd.bin",
        325312,
        "52c6c703e3f2eea67469c5c28353a9a10316b8d89be773c7b1a9c30ce6e20eda",
    ),
    file(
        "amdgpu/tonga_vce.bin",
        161024,
        "a5b0d7bcaab9798f775aab23dbdbec66ecbdb8a6832e69b338530411a3bcafe8",
    ),
    file(
        "amdgpu/topaz_ce.bin",
        8832,
        "39b60959b08623bb085735174360a5f870c55f30bf52f19c89f7f7ccedab348d",
    ),
    file(
        "amdgpu/topaz_k_smc.bin",
        80544,
        "46c1332f49582860c7cb64482c357eba48f7c804998d4bfd045f7e5e4001a429",
    ),
    file(
        "amdgpu/topaz_mc.bin",
        32100,
        "236df987bcad6f9f8b09e0bd95c7b9c6a257640c01b936177b1dcf22dacdfb02",
    ),
    file(
        "amdgpu/topaz_me.bin",
        17024,
        "420ef37021ee1ffb3e4e410c4b6f537ff39895e13775e5ca1171e1bdf9dcf236",
    ),
    file(
        "amdgpu/topaz_mec.bin",
        262784,
        "f4545925999f6d75793b84843c8708aa7a247d7e78b6174f70386e6a588c4cca",
    ),
    file(
        "amdgpu/topaz_pfp.bin",
        17024,
        "d46247c24064b78443974799e934d01c880818c362788886e734ebca271b4fd3",
    ),
    file(
        "amdgpu/topaz_rlc.bin",
        8448,
        "8553ce7b6d172d77a872112607355b95ca921b2d1751f7b93f6f498144eabfba",
    ),
    file(
        "amdgpu/topaz_sdma.bin",
        8576,
        "1480cfdd6ce7c28ece8c56f0b393f451276dcd1e341d9849e7eb5a5fc1b396e6",
    ),
    file(
        "amdgpu/topaz_sdma1.bin",
        8576,
        "a19f95eadb7656ff5b046c3c64c477d618a63435e7de5895114aece20cbdb8a2",
    ),
    file(
        "amdgpu/topaz_smc.bin",
        80544,
        "753b8b9fdaf0b0e1d1468122fc4fb7ea30ce69c42dfe39b017d249584bde095c",
    ),
    file(
        "amdgpu/umsch_mm_4_0_0.bin",
        386656,
        "6062fdd774e89251d5990df6bf4eb0cf546ce8e26beae1667bcdbdfc6f93c7f8",
    ),
    file(
        "amdgpu/vangogh_asd.bin",
        209408,
        "9b39a722b45f7d7f1090d3651f560e6fafb88f54bc5424aafcaca6fe098074f8",
    ),
    file(
        "amdgpu/vangogh_ce.bin",
        263296,
        "de39ff5c1f45a4a3d2b7e0760a2ff64a593e5f3e03e82600c62e18bc72f48e01",
    ),
    file(
        "amdgpu/vangogh_dmcub.bin",
        86132,
        "3a0d453670c4312bb5ef8ddf0cfe9842ce08d0546be3d3846aad1a6013b3252a",
    ),
    file(
        "amdgpu/vangogh_me.bin",
        263424,
        "922444d873abfe592af8f3ea0faa61a053ce94269ad3dd19dbd72cb5a96eceeb",
    ),
    file(
        "amdgpu/vangogh_mec.bin",
        268160,
        "48ac7448a86f75dac168c19c0f4e10734cc2c5fa9fc7cb28aacfad5c59527170",
    ),
    file(
        "amdgpu/vangogh_mec2.bin",
        268160,
        "48ac7448a86f75dac168c19c0f4e10734cc2c5fa9fc7cb28aacfad5c59527170",
    ),
    file(
        "amdgpu/vangogh_pfp.bin",
        263424,
        "425359a89c32fa4b9ae7ce487f23a76e51c9003882185c30d7b167bbeefdc6ba",
    ),
    file(
        "amdgpu/vangogh_rlc.bin",
        45368,
        "a3512355132203d8aca122e246b3fa8dcf5aa13c064011e8ffedf5674656b58b",
    ),
    file(
        "amdgpu/vangogh_sdma.bin",
        135424,
        "7e81b24cb357e3373bf0d7b94bff53c8b0225fd87d07d6940d98241d9ec572fd",
    ),
    file(
        "amdgpu/vangogh_toc.bin",
        1792,
        "2ca90c91f7e1756d5c1bd5443007768c692fbe14a228eb5a87e47953c62f5d75",
    ),
    file(
        "amdgpu/vangogh_vcn.bin",
        575056,
        "3d282b29bff2ea1833e00cfb08bb6dd9b379f7e58090e0e87aaccb4c2a35d732",
    ),
    file(
        "amdgpu/vcn_3_1_2.bin",
        575056,
        "31176d1a359aad08d462895eb02184b5d36c8d7570afc380ec889a9c7ffe8cb4",
    ),
    file(
        "amdgpu/vcn_4_0_0.bin",
        403856,
        "5633a5212d41e1f3932c1e7e630b1f2c8c4d27bc53ce8149c3be75d1d832053c",
    ),
    file(
        "amdgpu/vcn_4_0_2.bin",
        403856,
        "5633a5212d41e1f3932c1e7e630b1f2c8c4d27bc53ce8149c3be75d1d832053c",
    ),
    file(
        "amdgpu/vcn_4_0_3.bin",
        359824,
        "205d8ef5c3ab19070b226abf82c5c39617233136a6d44ad056334fbf269b3cda",
    ),
    file(
        "amdgpu/vcn_4_0_4.bin",
        403856,
        "5633a5212d41e1f3932c1e7e630b1f2c8c4d27bc53ce8149c3be75d1d832053c",
    ),
    file(
        "amdgpu/vcn_4_0_5.bin",
        403856,
        "4481b9b377bc7c06727032b826456f3e8b9f896792c696de77f9bf8018b164c9",
    ),
    file(
        "amdgpu/vcn_4_0_6.bin",
        403856,
        "4481b9b377bc7c06727032b826456f3e8b9f896792c696de77f9bf8018b164c9",
    ),
    file(
        "amdgpu/vcn_4_0_6_1.bin",
        403856,
        "c5e9a4ef05d3c4afa77c5a8fabe5987bf8272907823ae5463f97e93a818a1566",
    ),
    file(
        "amdgpu/vcn_5_0_0.bin",
        440400,
        "9a0ab35bf157c835cbbc4a207ba819ea0111e2a65fe7f8c20910ffff3122f903",
    ),
    file(
        "amdgpu/vcn_5_0_1.bin",
        218256,
        "1ab656611d9aa9650e7a0c811adf30bbc9f95b93f5af54eba4842d73d99fae86",
    ),
    file(
        "amdgpu/vcn_5_3_0.bin",
        449296,
        "a52ba1fb434c41516621eba63e3c46c51786b6300b61f28a31974ac132fec634",
    ),
    file(
        "amdgpu/vega10_acg_smc.bin",
        262912,
        "f2c436c5b8963bffe734a2001f2b220e7f93ba1850913543c4e3c507aa40c82f",
    ),
    file(
        "amdgpu/vega10_asd.bin",
        205312,
        "c1cfd36cfe96b44abb87887ff59edaba7af643012771fb8c66ee63e1b5e2e04f",
    ),
    file(
        "amdgpu/vega10_ce.bin",
        9344,
        "6bb4c122368678975fa1108c44e7e6e643311fb332165bbde2743c76f888baea",
    ),
    file(
        "amdgpu/vega10_gpu_info.bin",
        316,
        "92c3d1d7422c9771a7ff7fc8457884407a1c455150fd70a37badc6cbf7504c58",
    ),
    file(
        "amdgpu/vega10_ip_discovery.bin",
        656,
        "f1b1f7d1d08adf3c76ae2eb7e85477d115894e2fb25f6403c1b13ca845eb115d",
    ),
    file(
        "amdgpu/vega10_me.bin",
        17536,
        "fdfc713030594d008dd861c1014a6fe87d02aeea24a37d016295de920f2cde6c",
    ),
    file(
        "amdgpu/vega10_mec.bin",
        268224,
        "f5f32cd54001b824326f739f44a57875186d0481b5aa19970b87cc4fdc3bb7fb",
    ),
    file(
        "amdgpu/vega10_mec2.bin",
        268224,
        "f5f32cd54001b824326f739f44a57875186d0481b5aa19970b87cc4fdc3bb7fb",
    ),
    file(
        "amdgpu/vega10_pfp.bin",
        21632,
        "bf8bf9e34e5b15b6d2378315b291d7f8227ec7c610004e72a5fc19724f0b1499",
    ),
    file(
        "amdgpu/vega10_rlc.bin",
        17152,
        "6b58ded2b3bdea5601dcb88e9b102a62ef2b29b5535d5bf6e0a56e30083fca48",
    ),
    file(
        "amdgpu/vega10_sdma.bin",
        17408,
        "c5e748cfbba6a655295d500880f11df435c8d4cb8db8f077a98579beadd4a13b",
    ),
    file(
        "amdgpu/vega10_sdma1.bin",
        17408,
        "1f7977027eff7c71ccb198341f7b94b1efa592dc6e8afd0f67ba4744dc7e118f",
    ),
    file(
        "amdgpu/vega10_smc.bin",
        262912,
        "14b4b055546d15136cd549277ef02b2434bbc97873765239dd7f4bd89af2c3e5",
    ),
    file(
        "amdgpu/vega10_sos.bin",
        166800,
        "083a03f42eb5098d5a7b9037bbd0c9f177fd555035cdc073089f59c7eb4f8ca5",
    ),
    file(
        "amdgpu/vega10_uvd.bin",
        380832,
        "88c76158c9937e22c94ffc6b18ae9a43e4305904809b96ec697e350f4753da66",
    ),
    file(
        "amdgpu/vega10_vce.bin",
        173856,
        "e6c98b3855db3f998aaa2fd4b2a91a12d950655424108c90a9b9131023eb3b85",
    ),
    file(
        "amdgpu/vega12_asd.bin",
        205312,
        "c1cfd36cfe96b44abb87887ff59edaba7af643012771fb8c66ee63e1b5e2e04f",
    ),
    file(
        "amdgpu/vega12_ce.bin",
        9344,
        "ce02ea459f3e4c7767ed0c57df05c254210e923a38f205a2ea55dd26dad3f6c3",
    ),
    file(
        "amdgpu/vega12_gpu_info.bin",
        316,
        "5ed873ed3818ee2992d21c907f6658e01948a1ab950304e9170c7d9933a19cca",
    ),
    file(
        "amdgpu/vega12_ip_discovery.bin",
        656,
        "dfbeed33284b175bce635d046a5e902bb65d25a4cee1fbc4188ca8df2a807a47",
    ),
    file(
        "amdgpu/vega12_me.bin",
        17536,
        "d95737a20998d429f893b15d80de62aa114fe6825aae60a00afca2838bdf09a6",
    ),
    file(
        "amdgpu/vega12_mec.bin",
        268224,
        "4b9da907b0e9a1788061ef7f96934de875a107a73c999bf83f70ca6d4b3351f4",
    ),
    file(
        "amdgpu/vega12_mec2.bin",
        268224,
        "4b9da907b0e9a1788061ef7f96934de875a107a73c999bf83f70ca6d4b3351f4",
    ),
    file(
        "amdgpu/vega12_pfp.bin",
        21632,
        "0af775b72758e07fbcc498241326d7475c4932ccff3eec7d1a90901c41b14221",
    ),
    file(
        "amdgpu/vega12_rlc.bin",
        28636,
        "a6f7fc4b038d78cc7ec82f474c91330c6180cf2813f0608ed37c8ab50ad1557c",
    ),
    file(
        "amdgpu/vega12_sdma.bin",
        17408,
        "d87f1befbfe685dec382a4a426171969306252971466c7dca695aaeb70d99c68",
    ),
    file(
        "amdgpu/vega12_sdma1.bin",
        17408,
        "3bec6a0ddf2fd986ea52593b62e2940fad94e52564ce60af9374d4e562f766d7",
    ),
    file(
        "amdgpu/vega12_smc.bin",
        262912,
        "512a4a19edcfcd89796e7acaac851850ffb2f6f002ac85b7f0ac841aebe39465",
    ),
    file(
        "amdgpu/vega12_sos.bin",
        166800,
        "54064ac7997437541f85781103077f291db60a57f481c25df4a6e62103fbf008",
    ),
    file(
        "amdgpu/vega12_uvd.bin",
        380832,
        "b43add6b8702a8152330c54340029088da489974d0482a998b29851d304ae186",
    ),
    file(
        "amdgpu/vega12_vce.bin",
        173856,
        "1e850cff69b9ba7998fa1dd01558b4dfd885ec66859d77f0fa01984cd6a9cd55",
    ),
    file(
        "amdgpu/vega20_asd.bin",
        205312,
        "80ed634a9cec99e3520a6ac6fcb652b940bf81ca19f6f9aabccd91e1c8710482",
    ),
    file(
        "amdgpu/vega20_ce.bin",
        9344,
        "aa168fc7d3d99c323fd445674eb0227bb5fa0072d95e3e95e9e16e3b3ba55c0f",
    ),
    file(
        "amdgpu/vega20_ip_discovery.bin",
        564,
        "9b473431322ba062489fb5285b3a4cc4bd93b52ca17aeba291644cd44c4b2668",
    ),
    file(
        "amdgpu/vega20_me.bin",
        17536,
        "a5889a7bb174437d11125565132de58e00a5ddf0b84104df96ed3930e5fd1c3c",
    ),
    file(
        "amdgpu/vega20_mec.bin",
        268224,
        "7275323c00200905f6c1efaadb7d18d5e9f76f0a3edfa06cc86fc8b2bb2e904e",
    ),
    file(
        "amdgpu/vega20_mec2.bin",
        268224,
        "7275323c00200905f6c1efaadb7d18d5e9f76f0a3edfa06cc86fc8b2bb2e904e",
    ),
    file(
        "amdgpu/vega20_pfp.bin",
        21632,
        "37bfd1994b757ff3cef7edb81d0ab969831f689d0cfcf3e9f2f63d7bf148f197",
    ),
    file(
        "amdgpu/vega20_rlc.bin",
        49436,
        "786416def28a7a2f2db4abd5cecae5d4694fe1368f011ce2b26e100f9f008a4a",
    ),
    file(
        "amdgpu/vega20_sdma.bin",
        17408,
        "1457d02c5c7bc0d69b7aa456867cd99d73789441b34ff2c6c982a87d9d600875",
    ),
    file(
        "amdgpu/vega20_sdma1.bin",
        17408,
        "d61b687e9f3bcd930b236f2922ff352de3209c99e026403be6c9a59f2add1b90",
    ),
    file(
        "amdgpu/vega20_smc.bin",
        262912,
        "ba35e00f1971b63044a0d6c7795f1db82ab4a9925747d73e77b4de9657d51dfc",
    ),
    file(
        "amdgpu/vega20_sos.bin",
        174992,
        "0b162ed3731537b410f3bb48fa89a0111752644abcd72e5c8ff6112c83b4a193",
    ),
    file(
        "amdgpu/vega20_ta.bin",
        90880,
        "aea10970b0530e2e11f84a5c0444315c78b109106a02a41b4eeaea3ac96d56dc",
    ),
    file(
        "amdgpu/vega20_uvd.bin",
        383520,
        "c04901f4ea6b3545446601eb2cd58121c371200b0588d6bb937ccaf36f1705df",
    ),
    file(
        "amdgpu/vega20_vce.bin",
        173856,
        "e6c98b3855db3f998aaa2fd4b2a91a12d950655424108c90a9b9131023eb3b85",
    ),
    file(
        "amdgpu/vegam_ce.bin",
        8852,
        "c0d4acb3f1bd1def58cee933103069ce2b6b6d9b6a63591bd811b51a05ac0d67",
    ),
    file(
        "amdgpu/vegam_me.bin",
        17044,
        "c63ac86b770df364dc23c50ed6360a4823ad0e37120df12e4fb74bf574e3b536",
    ),
    file(
        "amdgpu/vegam_mec.bin",
        262824,
        "dbbe48008aaa667b64e2fbd7e93975200ae89b62aadaa6387b5ef14259701c20",
    ),
    file(
        "amdgpu/vegam_mec2.bin",
        262824,
        "97bd777ed7ad8cf6f2825dfdde1ed508c9f4b824db0c336b4e1d4ccc6c1136a5",
    ),
    file(
        "amdgpu/vegam_pfp.bin",
        17044,
        "3c088e0cf4dadfe067c525a63337387659e1d81d8a5896ba6b8cbc5b36762950",
    ),
    file(
        "amdgpu/vegam_rlc.bin",
        16660,
        "c729f89ade4fcd05c390f7bbf1fcb93eda47ac6911027c707add844181004b47",
    ),
    file(
        "amdgpu/vegam_sdma.bin",
        12692,
        "9d964f4896b6713a1e41b8eca5ef665598290e4cc801d58fb2257845b7112040",
    ),
    file(
        "amdgpu/vegam_sdma1.bin",
        12692,
        "477f1eb3a796c1248f38fb98ff389e9bb49d7cae746556055484a60668f509a2",
    ),
    file(
        "amdgpu/vegam_smc.bin",
        130964,
        "d6d422d2717b59bb7e23597157c2d6978b066475b4872045aff4702f2fee95eb",
    ),
    file(
        "amdgpu/vegam_uvd.bin",
        375424,
        "e05664b587a1585d98db3097cf4a55119ee4af2da68e65d3d43b3f57065a95fb",
    ),
    file(
        "amdgpu/vegam_vce.bin",
        166816,
        "b063be1f8093b41f527eecc7c592ff92c06d1c9bd5d2b4560e9dcd065f49898a",
    ),
    file(
        "amdgpu/vpe_2_0_0.bin",
        34688,
        "7792566da5e7c94b84e20678807b74f78c64482ff2522afce65d70c57cec8ed4",
    ),
    file(
        "amdgpu/vpe_6_1_0.bin",
        25984,
        "10a900e43e8633bc1ebe03d2d88f9b3e2f96a43c1961b190023fa1d26423a41a",
    ),
    file(
        "amdgpu/vpe_6_1_1.bin",
        25984,
        "e0422f7fccecd2e33ca1b1c58ed188230ea67f9c957ba73fa57ef80df64ead51",
    ),
    file(
        "amdgpu/vpe_6_1_3.bin",
        25984,
        "e4545e48016f8cce1d41aef39e00d81c3c11a72a14f0ca4e0bd1a18f43b84bb9",
    ),
    file(
        "amdgpu/yellow_carp_ce.bin",
        263296,
        "bd1fd7a357d7c9aa489469166435aa2d3e2ee1b324d4be35e9a648c794281674",
    ),
    file(
        "amdgpu/yellow_carp_dmcub.bin",
        236048,
        "8d546bbbdcec355625dfb4bb4baef78841793c9c0209be42df5305855094f4eb",
    ),
    file(
        "amdgpu/yellow_carp_me.bin",
        263424,
        "e2eeb8958a08231813943dabffd0e18ab65e7c0338a36155b9e6584ce55df31f",
    ),
    file(
        "amdgpu/yellow_carp_mec.bin",
        268160,
        "9020014a34a9357b28058f4ecdf1f0d3ffbcac7265df04c544722ed5f7447e9b",
    ),
    file(
        "amdgpu/yellow_carp_mec2.bin",
        268160,
        "9020014a34a9357b28058f4ecdf1f0d3ffbcac7265df04c544722ed5f7447e9b",
    ),
    file(
        "amdgpu/yellow_carp_pfp.bin",
        263424,
        "b817f15a37ae080d5037a159732d60be24dda0bed74b6a3ba609d7c38f4d29a7",
    ),
    file(
        "amdgpu/yellow_carp_rlc.bin",
        178128,
        "bf41c78ecf4f3444ee14c917358485e55bfc5c496779821f1cad643b25a3da7f",
    ),
    file(
        "amdgpu/yellow_carp_sdma.bin",
        34048,
        "be8cdc90616c6aa1e92cf1307e18515958a5c6b80552e20fa4452e2e61c18fd4",
    ),
    file(
        "amdgpu/yellow_carp_ta.bin",
        246784,
        "0b7fc5a0bea6e3559df9791a57ae5a648ec6246679f8f8a6bc27defb2aa7a75e",
    ),
    file(
        "amdgpu/yellow_carp_toc.bin",
        1792,
        "9fc8da2f6106bf657222ca4bcca687e66e657e5710278c694d9d0e862221758d",
    ),
    file(
        "amdgpu/yellow_carp_vcn.bin",
        575056,
        "31176d1a359aad08d462895eb02184b5d36c8d7570afc380ec889a9c7ffe8cb4",
    ),
];

/// The files amdgpu's built units declare that linux-firmware does not hold
/// at the pin, nor at its newest commit of 2026-11-16: a GPU whose IP blocks
/// ask for one of them fails amdgpu's probe there, as on Linux.
pub const UNPUBLISHED: &[&str] = &[
    "amdgpu/cyan_skillfish_gpu_info.bin",
    "amdgpu/dcn_4_2_1_dmcub.bin",
    "amdgpu/gc_11_0_0_toc.bin",
    "amdgpu/gc_11_0_3_mes.bin",
    "amdgpu/gc_11_5_4_imu.bin",
    "amdgpu/gc_11_5_4_me.bin",
    "amdgpu/gc_11_5_4_mec.bin",
    "amdgpu/gc_11_5_4_mes1.bin",
    "amdgpu/gc_11_5_4_mes_2.bin",
    "amdgpu/gc_11_5_4_pfp.bin",
    "amdgpu/gc_11_5_4_rlc.bin",
    "amdgpu/gc_11_5_6_imu.bin",
    "amdgpu/gc_11_5_6_me.bin",
    "amdgpu/gc_11_5_6_mec.bin",
    "amdgpu/gc_11_5_6_mes1.bin",
    "amdgpu/gc_11_5_6_mes_2.bin",
    "amdgpu/gc_11_5_6_pfp.bin",
    "amdgpu/gc_11_5_6_rlc.bin",
    "amdgpu/gc_12_1_0_imu.bin",
    "amdgpu/gc_12_1_0_mec.bin",
    "amdgpu/gc_12_1_0_mes.bin",
    "amdgpu/gc_12_1_0_mes1.bin",
    "amdgpu/gc_12_1_0_rlc_1.bin",
    "amdgpu/gc_12_1_0_uni_mes.bin",
    "amdgpu/ip_discovery.bin",
    "amdgpu/navi12_cap.bin",
    "amdgpu/psp_15_0_5_ta.bin",
    "amdgpu/psp_15_0_5_toc.bin",
    "amdgpu/psp_15_0_8_toc.bin",
    "amdgpu/psp_15_0_8_toc_1.bin",
    "amdgpu/sdma_6_4_0.bin",
    "amdgpu/sdma_7_1_0.bin",
    "amdgpu/sienna_cichlid_cap.bin",
    "amdgpu/vcn_5_0_2.bin",
    "amdgpu/vega10_cap.bin",
    "amdgpu/vpe_2_2_0.bin",
];

/// `pciidlist`'s entries that name a device: its ID and its family
/// (`CHIP_*` without the prefix).
pub(super) const DEVICES: &[(u16, &str)] = &[
    (0x1304, "KAVERI"),
    (0x1305, "KAVERI"),
    (0x1306, "KAVERI"),
    (0x1307, "KAVERI"),
    (0x1309, "KAVERI"),
    (0x130a, "KAVERI"),
    (0x130b, "KAVERI"),
    (0x130c, "KAVERI"),
    (0x130d, "KAVERI"),
    (0x130e, "KAVERI"),
    (0x130f, "KAVERI"),
    (0x1310, "KAVERI"),
    (0x1311, "KAVERI"),
    (0x1312, "KAVERI"),
    (0x1313, "KAVERI"),
    (0x1315, "KAVERI"),
    (0x1316, "KAVERI"),
    (0x1317, "KAVERI"),
    (0x1318, "KAVERI"),
    (0x131b, "KAVERI"),
    (0x131c, "KAVERI"),
    (0x131d, "KAVERI"),
    (0x13db, "CYAN_SKILLFISH"),
    (0x13f9, "CYAN_SKILLFISH"),
    (0x13fa, "CYAN_SKILLFISH"),
    (0x13fb, "CYAN_SKILLFISH"),
    (0x13fc, "CYAN_SKILLFISH"),
    (0x13fe, "CYAN_SKILLFISH"),
    (0x143f, "CYAN_SKILLFISH"),
    (0x15d8, "RAVEN"),
    (0x15dd, "RAVEN"),
    (0x15e7, "RENOIR"),
    (0x1636, "RENOIR"),
    (0x1638, "RENOIR"),
    (0x164c, "RENOIR"),
    (0x164d, "YELLOW_CARP"),
    (0x1681, "YELLOW_CARP"),
    (0x6600, "OLAND"),
    (0x6601, "OLAND"),
    (0x6602, "OLAND"),
    (0x6603, "OLAND"),
    (0x6604, "OLAND"),
    (0x6605, "OLAND"),
    (0x6606, "OLAND"),
    (0x6607, "OLAND"),
    (0x6608, "OLAND"),
    (0x6610, "OLAND"),
    (0x6611, "OLAND"),
    (0x6613, "OLAND"),
    (0x6617, "OLAND"),
    (0x6620, "OLAND"),
    (0x6621, "OLAND"),
    (0x6623, "OLAND"),
    (0x6631, "OLAND"),
    (0x6640, "BONAIRE"),
    (0x6641, "BONAIRE"),
    (0x6646, "BONAIRE"),
    (0x6647, "BONAIRE"),
    (0x6649, "BONAIRE"),
    (0x6650, "BONAIRE"),
    (0x6651, "BONAIRE"),
    (0x6658, "BONAIRE"),
    (0x665c, "BONAIRE"),
    (0x665d, "BONAIRE"),
    (0x665f, "BONAIRE"),
    (0x6660, "HAINAN"),
    (0x6663, "HAINAN"),
    (0x6664, "HAINAN"),
    (0x6665, "HAINAN"),
    (0x6667, "HAINAN"),
    (0x666f, "HAINAN"),
    (0x66a0, "VEGA20"),
    (0x66a1, "VEGA20"),
    (0x66a2, "VEGA20"),
    (0x66a3, "VEGA20"),
    (0x66a4, "VEGA20"),
    (0x66a7, "VEGA20"),
    (0x66af, "VEGA20"),
    (0x6780, "TAHITI"),
    (0x6784, "TAHITI"),
    (0x6788, "TAHITI"),
    (0x678a, "TAHITI"),
    (0x6790, "TAHITI"),
    (0x6791, "TAHITI"),
    (0x6792, "TAHITI"),
    (0x6798, "TAHITI"),
    (0x6799, "TAHITI"),
    (0x679a, "TAHITI"),
    (0x679b, "TAHITI"),
    (0x679e, "TAHITI"),
    (0x679f, "TAHITI"),
    (0x67a0, "HAWAII"),
    (0x67a1, "HAWAII"),
    (0x67a2, "HAWAII"),
    (0x67a8, "HAWAII"),
    (0x67a9, "HAWAII"),
    (0x67aa, "HAWAII"),
    (0x67b0, "HAWAII"),
    (0x67b1, "HAWAII"),
    (0x67b8, "HAWAII"),
    (0x67b9, "HAWAII"),
    (0x67ba, "HAWAII"),
    (0x67be, "HAWAII"),
    (0x67c0, "POLARIS10"),
    (0x67c1, "POLARIS10"),
    (0x67c2, "POLARIS10"),
    (0x67c4, "POLARIS10"),
    (0x67c7, "POLARIS10"),
    (0x67c8, "POLARIS10"),
    (0x67c9, "POLARIS10"),
    (0x67ca, "POLARIS10"),
    (0x67cc, "POLARIS10"),
    (0x67cf, "POLARIS10"),
    (0x67d0, "POLARIS10"),
    (0x67df, "POLARIS10"),
    (0x67e0, "POLARIS11"),
    (0x67e1, "POLARIS11"),
    (0x67e3, "POLARIS11"),
    (0x67e7, "POLARIS11"),
    (0x67e8, "POLARIS11"),
    (0x67e9, "POLARIS11"),
    (0x67eb, "POLARIS11"),
    (0x67ef, "POLARIS11"),
    (0x67ff, "POLARIS11"),
    (0x6800, "PITCAIRN"),
    (0x6801, "PITCAIRN"),
    (0x6802, "PITCAIRN"),
    (0x6806, "PITCAIRN"),
    (0x6808, "PITCAIRN"),
    (0x6809, "PITCAIRN"),
    (0x6810, "PITCAIRN"),
    (0x6811, "PITCAIRN"),
    (0x6816, "PITCAIRN"),
    (0x6817, "PITCAIRN"),
    (0x6818, "PITCAIRN"),
    (0x6819, "PITCAIRN"),
    (0x6820, "VERDE"),
    (0x6821, "VERDE"),
    (0x6822, "VERDE"),
    (0x6823, "VERDE"),
    (0x6824, "VERDE"),
    (0x6825, "VERDE"),
    (0x6826, "VERDE"),
    (0x6827, "VERDE"),
    (0x6828, "VERDE"),
    (0x6829, "VERDE"),
    (0x682a, "VERDE"),
    (0x682b, "VERDE"),
    (0x682c, "VERDE"),
    (0x682d, "VERDE"),
    (0x682f, "VERDE"),
    (0x6830, "VERDE"),
    (0x6831, "VERDE"),
    (0x6835, "VERDE"),
    (0x6837, "VERDE"),
    (0x6838, "VERDE"),
    (0x6839, "VERDE"),
    (0x683b, "VERDE"),
    (0x683d, "VERDE"),
    (0x683f, "VERDE"),
    (0x6860, "VEGA10"),
    (0x6861, "VEGA10"),
    (0x6862, "VEGA10"),
    (0x6863, "VEGA10"),
    (0x6864, "VEGA10"),
    (0x6867, "VEGA10"),
    (0x6868, "VEGA10"),
    (0x6869, "VEGA10"),
    (0x686a, "VEGA10"),
    (0x686b, "VEGA10"),
    (0x686c, "VEGA10"),
    (0x686d, "VEGA10"),
    (0x686e, "VEGA10"),
    (0x686f, "VEGA10"),
    (0x687f, "VEGA10"),
    (0x6900, "TOPAZ"),
    (0x6901, "TOPAZ"),
    (0x6902, "TOPAZ"),
    (0x6903, "TOPAZ"),
    (0x6907, "TOPAZ"),
    (0x6920, "TONGA"),
    (0x6921, "TONGA"),
    (0x6928, "TONGA"),
    (0x6929, "TONGA"),
    (0x692b, "TONGA"),
    (0x692f, "TONGA"),
    (0x6930, "TONGA"),
    (0x6938, "TONGA"),
    (0x6939, "TONGA"),
    (0x694c, "VEGAM"),
    (0x694e, "VEGAM"),
    (0x694f, "VEGAM"),
    (0x6980, "POLARIS12"),
    (0x6981, "POLARIS12"),
    (0x6985, "POLARIS12"),
    (0x6986, "POLARIS12"),
    (0x6987, "POLARIS12"),
    (0x6995, "POLARIS12"),
    (0x6997, "POLARIS12"),
    (0x699f, "POLARIS12"),
    (0x69a0, "VEGA12"),
    (0x69a1, "VEGA12"),
    (0x69a2, "VEGA12"),
    (0x69a3, "VEGA12"),
    (0x69af, "VEGA12"),
    (0x6fdf, "POLARIS10"),
    (0x7300, "FIJI"),
    (0x730f, "FIJI"),
    (0x7310, "NAVI10"),
    (0x7312, "NAVI10"),
    (0x7318, "NAVI10"),
    (0x7319, "NAVI10"),
    (0x731a, "NAVI10"),
    (0x731b, "NAVI10"),
    (0x731e, "NAVI10"),
    (0x731f, "NAVI10"),
    (0x7340, "NAVI14"),
    (0x7341, "NAVI14"),
    (0x7347, "NAVI14"),
    (0x734f, "NAVI14"),
    (0x7360, "NAVI12"),
    (0x7362, "NAVI12"),
    (0x7388, "ARCTURUS"),
    (0x738c, "ARCTURUS"),
    (0x738e, "ARCTURUS"),
    (0x7390, "ARCTURUS"),
    (0x73a0, "SIENNA_CICHLID"),
    (0x73a1, "SIENNA_CICHLID"),
    (0x73a2, "SIENNA_CICHLID"),
    (0x73a3, "SIENNA_CICHLID"),
    (0x73a5, "SIENNA_CICHLID"),
    (0x73a8, "SIENNA_CICHLID"),
    (0x73a9, "SIENNA_CICHLID"),
    (0x73ab, "SIENNA_CICHLID"),
    (0x73ac, "SIENNA_CICHLID"),
    (0x73ad, "SIENNA_CICHLID"),
    (0x73ae, "SIENNA_CICHLID"),
    (0x73af, "SIENNA_CICHLID"),
    (0x73bf, "SIENNA_CICHLID"),
    (0x73c0, "NAVY_FLOUNDER"),
    (0x73c1, "NAVY_FLOUNDER"),
    (0x73c3, "NAVY_FLOUNDER"),
    (0x73da, "NAVY_FLOUNDER"),
    (0x73db, "NAVY_FLOUNDER"),
    (0x73dc, "NAVY_FLOUNDER"),
    (0x73dd, "NAVY_FLOUNDER"),
    (0x73de, "NAVY_FLOUNDER"),
    (0x73df, "NAVY_FLOUNDER"),
    (0x73e0, "DIMGREY_CAVEFISH"),
    (0x73e1, "DIMGREY_CAVEFISH"),
    (0x73e2, "DIMGREY_CAVEFISH"),
    (0x73e3, "DIMGREY_CAVEFISH"),
    (0x73e8, "DIMGREY_CAVEFISH"),
    (0x73e9, "DIMGREY_CAVEFISH"),
    (0x73ea, "DIMGREY_CAVEFISH"),
    (0x73eb, "DIMGREY_CAVEFISH"),
    (0x73ec, "DIMGREY_CAVEFISH"),
    (0x73ed, "DIMGREY_CAVEFISH"),
    (0x73ef, "DIMGREY_CAVEFISH"),
    (0x73ff, "DIMGREY_CAVEFISH"),
    (0x7408, "ALDEBARAN"),
    (0x740c, "ALDEBARAN"),
    (0x740f, "ALDEBARAN"),
    (0x7410, "ALDEBARAN"),
    (0x7420, "BEIGE_GOBY"),
    (0x7421, "BEIGE_GOBY"),
    (0x7422, "BEIGE_GOBY"),
    (0x7423, "BEIGE_GOBY"),
    (0x7424, "BEIGE_GOBY"),
    (0x743f, "BEIGE_GOBY"),
    (0x9830, "KABINI"),
    (0x9831, "KABINI"),
    (0x9832, "KABINI"),
    (0x9833, "KABINI"),
    (0x9834, "KABINI"),
    (0x9835, "KABINI"),
    (0x9836, "KABINI"),
    (0x9837, "KABINI"),
    (0x9838, "KABINI"),
    (0x9839, "KABINI"),
    (0x983a, "KABINI"),
    (0x983b, "KABINI"),
    (0x983c, "KABINI"),
    (0x983d, "KABINI"),
    (0x983e, "KABINI"),
    (0x983f, "KABINI"),
    (0x9850, "MULLINS"),
    (0x9851, "MULLINS"),
    (0x9852, "MULLINS"),
    (0x9853, "MULLINS"),
    (0x9854, "MULLINS"),
    (0x9855, "MULLINS"),
    (0x9856, "MULLINS"),
    (0x9857, "MULLINS"),
    (0x9858, "MULLINS"),
    (0x9859, "MULLINS"),
    (0x985a, "MULLINS"),
    (0x985b, "MULLINS"),
    (0x985c, "MULLINS"),
    (0x985d, "MULLINS"),
    (0x985e, "MULLINS"),
    (0x985f, "MULLINS"),
    (0x9870, "CARRIZO"),
    (0x9874, "CARRIZO"),
    (0x9875, "CARRIZO"),
    (0x9876, "CARRIZO"),
    (0x9877, "CARRIZO"),
    (0x98e4, "STONEY"),
];

/// `amdgpu_unsupported_pciidlist`: devices amdgpu leaves to radeon, r128 or
/// mach64, which load none of its files.
pub(super) const LEFT_TO_RADEON: &[u16] = &[
    0x3150, 0x3151, 0x3152, 0x3154, 0x3155, 0x3171, 0x3e50, 0x3e54, 0x3e70, 0x4136, 0x4137, 0x4144,
    0x4145, 0x4146, 0x4147, 0x4148, 0x4149, 0x414a, 0x414b, 0x4150, 0x4151, 0x4152, 0x4153, 0x4154,
    0x4155, 0x4156, 0x4164, 0x4165, 0x4166, 0x4168, 0x4170, 0x4171, 0x4172, 0x4173, 0x4237, 0x4242,
    0x4336, 0x4337, 0x4354, 0x4358, 0x4437, 0x4554, 0x4742, 0x4744, 0x4749, 0x474c, 0x474d, 0x474e,
    0x474f, 0x4750, 0x4751, 0x4752, 0x4753, 0x4754, 0x4755, 0x4756, 0x4757, 0x4758, 0x4759, 0x475a,
    0x4966, 0x4967, 0x496e, 0x4a48, 0x4a49, 0x4a4a, 0x4a4b, 0x4a4c, 0x4a4d, 0x4a4e, 0x4a4f, 0x4a50,
    0x4a54, 0x4a69, 0x4a6a, 0x4a6b, 0x4a70, 0x4a74, 0x4b48, 0x4b49, 0x4b4a, 0x4b4b, 0x4b4c, 0x4b69,
    0x4b6b, 0x4b6c, 0x4c42, 0x4c44, 0x4c45, 0x4c46, 0x4c47, 0x4c49, 0x4c4d, 0x4c4e, 0x4c50, 0x4c51,
    0x4c52, 0x4c53, 0x4c57, 0x4c58, 0x4c59, 0x4c5a, 0x4c64, 0x4c66, 0x4c67, 0x4c6e, 0x4d46, 0x4d4c,
    0x4e44, 0x4e45, 0x4e46, 0x4e47, 0x4e48, 0x4e49, 0x4e4a, 0x4e4b, 0x4e50, 0x4e51, 0x4e52, 0x4e53,
    0x4e54, 0x4e56, 0x4e64, 0x4e65, 0x4e66, 0x4e67, 0x4e68, 0x4e69, 0x4e6a, 0x4e71, 0x4f73, 0x5041,
    0x5042, 0x5043, 0x5044, 0x5045, 0x5046, 0x5047, 0x5048, 0x5049, 0x504a, 0x504b, 0x504c, 0x504d,
    0x504e, 0x504f, 0x5050, 0x5051, 0x5052, 0x5053, 0x5054, 0x5055, 0x5056, 0x5057, 0x5058, 0x5144,
    0x5145, 0x5146, 0x5147, 0x5148, 0x514c, 0x514d, 0x5157, 0x5158, 0x5159, 0x515a, 0x515e, 0x5245,
    0x5246, 0x5247, 0x524b, 0x524c, 0x534d, 0x5446, 0x544c, 0x5452, 0x5460, 0x5462, 0x5464, 0x5548,
    0x5549, 0x554a, 0x554b, 0x554c, 0x554d, 0x554e, 0x554f, 0x5550, 0x5551, 0x5552, 0x5554, 0x5569,
    0x556b, 0x556d, 0x556f, 0x5571, 0x564a, 0x564b, 0x564f, 0x5652, 0x5653, 0x5654, 0x5655, 0x5656,
    0x5657, 0x5834, 0x5835, 0x5854, 0x5874, 0x5940, 0x5941, 0x5954, 0x5955, 0x5960, 0x5961, 0x5962,
    0x5964, 0x5965, 0x5969, 0x5974, 0x5975, 0x5a41, 0x5a42, 0x5a61, 0x5a62, 0x5b60, 0x5b62, 0x5b63,
    0x5b64, 0x5b65, 0x5b70, 0x5b72, 0x5b73, 0x5b74, 0x5b75, 0x5c61, 0x5c63, 0x5d44, 0x5d45, 0x5d48,
    0x5d49, 0x5d4a, 0x5d4c, 0x5d4d, 0x5d4e, 0x5d4f, 0x5d50, 0x5d52, 0x5d57, 0x5d6d, 0x5d6f, 0x5d72,
    0x5d77, 0x5e48, 0x5e4a, 0x5e4b, 0x5e4c, 0x5e4d, 0x5e4f, 0x5e6b, 0x5e6d, 0x6700, 0x6701, 0x6702,
    0x6703, 0x6704, 0x6705, 0x6706, 0x6707, 0x6708, 0x6709, 0x6718, 0x6719, 0x671c, 0x671d, 0x671f,
    0x6720, 0x6721, 0x6722, 0x6723, 0x6724, 0x6725, 0x6726, 0x6727, 0x6728, 0x6729, 0x6738, 0x6739,
    0x673e, 0x6740, 0x6741, 0x6742, 0x6743, 0x6744, 0x6745, 0x6746, 0x6747, 0x6748, 0x6749, 0x674a,
    0x6750, 0x6751, 0x6758, 0x6759, 0x675b, 0x675d, 0x675f, 0x6760, 0x6761, 0x6762, 0x6763, 0x6764,
    0x6765, 0x6766, 0x6767, 0x6768, 0x6770, 0x6771, 0x6772, 0x6778, 0x6779, 0x677b, 0x6840, 0x6841,
    0x6842, 0x6843, 0x6849, 0x684c, 0x6850, 0x6858, 0x6859, 0x6880, 0x6888, 0x6889, 0x688a, 0x688c,
    0x688d, 0x6898, 0x6899, 0x689b, 0x689c, 0x689d, 0x689e, 0x68a0, 0x68a1, 0x68a8, 0x68a9, 0x68b0,
    0x68b8, 0x68b9, 0x68ba, 0x68be, 0x68bf, 0x68c0, 0x68c1, 0x68c7, 0x68c8, 0x68c9, 0x68d8, 0x68d9,
    0x68da, 0x68de, 0x68e0, 0x68e1, 0x68e4, 0x68e5, 0x68e8, 0x68e9, 0x68f1, 0x68f2, 0x68f8, 0x68f9,
    0x68fa, 0x68fe, 0x7100, 0x7101, 0x7102, 0x7103, 0x7104, 0x7105, 0x7106, 0x7108, 0x7109, 0x710a,
    0x710b, 0x710c, 0x710e, 0x710f, 0x7120, 0x7124, 0x7129, 0x712e, 0x712f, 0x7140, 0x7141, 0x7142,
    0x7143, 0x7144, 0x7145, 0x7146, 0x7147, 0x7149, 0x714a, 0x714b, 0x714c, 0x714d, 0x714e, 0x714f,
    0x7151, 0x7152, 0x7153, 0x715e, 0x715f, 0x7162, 0x7163, 0x7166, 0x7167, 0x7172, 0x7173, 0x7180,
    0x7181, 0x7183, 0x7186, 0x7187, 0x7188, 0x718a, 0x718b, 0x718c, 0x718d, 0x718f, 0x7193, 0x7196,
    0x719b, 0x719f, 0x71a0, 0x71a1, 0x71a3, 0x71a7, 0x71bb, 0x71c0, 0x71c1, 0x71c2, 0x71c3, 0x71c4,
    0x71c5, 0x71c6, 0x71c7, 0x71cd, 0x71ce, 0x71d2, 0x71d4, 0x71d5, 0x71d6, 0x71da, 0x71de, 0x71e0,
    0x71e1, 0x71e2, 0x71e6, 0x71e7, 0x71f2, 0x7200, 0x7210, 0x7211, 0x7240, 0x7243, 0x7244, 0x7245,
    0x7246, 0x7247, 0x7248, 0x7249, 0x724a, 0x724b, 0x724c, 0x724d, 0x724e, 0x724f, 0x7269, 0x726b,
    0x726e, 0x7280, 0x7281, 0x7283, 0x7284, 0x7287, 0x7288, 0x7289, 0x728b, 0x728c, 0x7290, 0x7291,
    0x7293, 0x7297, 0x72a0, 0x72a8, 0x72b1, 0x72b3, 0x7834, 0x7835, 0x791e, 0x791f, 0x793f, 0x7941,
    0x7942, 0x796c, 0x796d, 0x796e, 0x796f, 0x9400, 0x9401, 0x9402, 0x9403, 0x9405, 0x940a, 0x940b,
    0x940f, 0x9440, 0x9441, 0x9442, 0x9443, 0x9444, 0x9446, 0x944a, 0x944b, 0x944c, 0x944e, 0x9450,
    0x9452, 0x9456, 0x945a, 0x945b, 0x945e, 0x9460, 0x9462, 0x946a, 0x946b, 0x947a, 0x947b, 0x9480,
    0x9487, 0x9488, 0x9489, 0x948a, 0x948f, 0x9490, 0x9491, 0x9495, 0x9498, 0x949c, 0x949e, 0x949f,
    0x94a0, 0x94a1, 0x94a3, 0x94b1, 0x94b3, 0x94b4, 0x94b5, 0x94b9, 0x94c0, 0x94c1, 0x94c3, 0x94c4,
    0x94c5, 0x94c6, 0x94c7, 0x94c8, 0x94c9, 0x94cb, 0x94cc, 0x94cd, 0x9500, 0x9501, 0x9504, 0x9505,
    0x9506, 0x9507, 0x9508, 0x9509, 0x950f, 0x9511, 0x9515, 0x9517, 0x9519, 0x9540, 0x9541, 0x9542,
    0x954e, 0x954f, 0x9552, 0x9553, 0x9555, 0x9557, 0x955f, 0x9580, 0x9581, 0x9583, 0x9586, 0x9587,
    0x9588, 0x9589, 0x958a, 0x958b, 0x958c, 0x958d, 0x958e, 0x958f, 0x9590, 0x9591, 0x9593, 0x9595,
    0x9596, 0x9597, 0x9598, 0x9599, 0x959b, 0x95c0, 0x95c2, 0x95c4, 0x95c5, 0x95c6, 0x95c7, 0x95c9,
    0x95cc, 0x95cd, 0x95ce, 0x95cf, 0x9610, 0x9611, 0x9612, 0x9613, 0x9614, 0x9615, 0x9616, 0x9640,
    0x9641, 0x9642, 0x9643, 0x9644, 0x9645, 0x9647, 0x9648, 0x9649, 0x964a, 0x964b, 0x964c, 0x964e,
    0x964f, 0x9710, 0x9711, 0x9712, 0x9713, 0x9714, 0x9715, 0x9802, 0x9803, 0x9804, 0x9805, 0x9806,
    0x9807, 0x9808, 0x9809, 0x980a, 0x9900, 0x9901, 0x9903, 0x9904, 0x9905, 0x9906, 0x9907, 0x9908,
    0x9909, 0x990a, 0x990b, 0x990c, 0x990d, 0x990e, 0x990f, 0x9910, 0x9913, 0x9917, 0x9918, 0x9919,
    0x9990, 0x9991, 0x9992, 0x9993, 0x9994, 0x9995, 0x9996, 0x9997, 0x9998, 0x9999, 0x999a, 0x999b,
    0x999c, 0x999d, 0x99a0, 0x99a2, 0x99a4,
];
