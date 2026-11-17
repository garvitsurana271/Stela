//! Every AMD GPU RADV creates a device for, as Mesa's amdgpu no-op DRM shim
//! shows it to RADV, taken from the drivers' own tables.
//!
//! The chips are those Mesa identifies (`ac_identify_chip` in
//! `ac_gpu_info.c`) by an ASIC revision range of the graphics address
//! library (`amdgpu_asic_addr.h`): compute-only chips have no range there,
//! and RADV refuses them. Each is in RADV's generations, GFX6 through
//! GFX12, by `ac_get_gfx_level` (`amd_family.c`). The shim records 20 real
//! devices (`amdgpu_devices.c`); a chip without a record is derived from a
//! recorded sibling (`amd/tests/radv_shim_devices.c`): the sibling of its
//! own family and generation, else of its generation, else of the nearest
//! generation below with the chip's graphics IP version. Its family and
//! revision put it in its chip's range, and amdgpu's own ID table
//! (`amdgpu_drv.c`) gives its PCI ID and whether it is an APU; amdgpu claims
//! later chips by IP discovery, and those keep the sibling's.
use anyhow::{Context, Result, bail, ensure};
use std::collections::BTreeMap;

/// The sources the table comes from.
pub(super) struct Sources {
    /// `src/amd/common/amd_family.h`.
    pub families: String,
    /// `src/amd/common/amd_family.c`.
    pub levels: String,
    /// `src/amd/common/ac_gpu_info.c`.
    pub identify: String,
    /// `src/amd/addrlib/src/amdgpu_asic_addr.h`.
    pub ranges: String,
    /// `src/amd/common/amdgpu_devices.c`.
    pub records: String,
    /// Linux's `drivers/gpu/drm/amd/amdgpu/amdgpu_drv.c`.
    pub ids: String,
}

/// A device the test runs.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct ShimDevice {
    /// The shim's name for it (`AMDGPU_GPU_ID`): the chip's, in lower case.
    pub name: String,
    /// Mesa's chip (`CHIP_*`, without the prefix).
    pub chip: String,
    /// Its graphics generation (`GFX10_3`).
    pub level: String,
    /// What `amd_family.h` says it is, if anything.
    pub note: String,
    pub discrete: bool,
    /// How a chip without a record is made, or `None` for a recorded one.
    pub derived: Option<Derived>,
}

/// A device made from a recorded sibling's record.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Derived {
    pub base: String,
    pub family: u32,
    pub external_rev: u32,
    pub device_id: u32,
    /// The graphics IP version (major, minor) where the sibling's differs.
    pub version: Option<(u32, u32)>,
}

impl ShimDevice {
    /// What the test prints for it.
    pub fn description(&self) -> String {
        let level = self.level.replacen('_', ".", 1);
        let mut text = match self.note.is_empty() {
            true => format!("{}, {level}", self.chip),
            false => format!("{}, {level}", self.note),
        };
        if let Some(derived) = &self.derived {
            text.push_str(&format!(", from {}'s record", derived.base));
        }
        text
    }
}

/// Mesa's chips in enum order, each with its comment.
fn chips(families: &str) -> Result<Vec<(String, String)>> {
    let body = families
        .split("enum radeon_family")
        .nth(1)
        .and_then(|rest| rest.split("};").next())
        .context("amd_family.h has no enum radeon_family")?;
    Ok(body
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            let name = line.strip_prefix("CHIP_")?;
            let end = name.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))?;
            let note = line
                .split_once("/*")
                .and_then(|(_, comment)| comment.split_once("*/"))
                .map_or("", |(comment, _)| comment.trim());
            Some((name[..end].to_owned(), note.to_owned()))
        })
        .collect())
}

/// `ac_get_gfx_level`'s thresholds: from each chip on, its generation.
fn levels(source: &str) -> Result<(Vec<(String, String)>, String)> {
    let body = source
        .split("ac_get_gfx_level(enum radeon_family family)")
        .nth(1)
        .and_then(|rest| rest.split("\n}").next())
        .context("amd_family.c has no ac_get_gfx_level")?;
    let mut thresholds = Vec::new();
    let mut chip = None;
    let mut last = None;
    for line in body.lines().map(str::trim) {
        if let Some(rest) = line.strip_prefix("if (family >= CHIP_") {
            chip = rest.strip_suffix(')').map(str::to_owned);
        } else if let Some(level) = line
            .strip_prefix("return ")
            .and_then(|l| l.strip_suffix(';'))
        {
            match chip.take() {
                Some(chip) => thresholds.push((chip, level.to_owned())),
                None => last = Some(level.to_owned()),
            }
        }
    }
    ensure!(
        !thresholds.is_empty(),
        "ac_get_gfx_level names no generation"
    );
    Ok((
        thresholds,
        last.context("ac_get_gfx_level has no last generation")?,
    ))
}

/// `ac_identify_chip`'s chips: (family, ASIC range, chip) in its order.
fn identified(source: &str) -> Result<Vec<(String, String, String)>> {
    let body = source
        .split("ac_identify_chip(struct radeon_info *info")
        .nth(1)
        .and_then(|rest| rest.split("info->family == CHIP_UNKNOWN").next())
        .context("ac_gpu_info.c has no ac_identify_chip")?;
    let mut family = None;
    let mut found = Vec::new();
    for line in body.lines().map(str::trim) {
        if let Some(name) = line
            .strip_prefix("case FAMILY_")
            .and_then(|l| l.strip_suffix(':'))
        {
            family = Some(name.to_owned());
        } else if let Some(arguments) = line
            .strip_prefix("identify_chip")
            .and_then(|rest| rest.strip_suffix(");"))
        {
            let family = family.clone().context("identify_chip outside a family")?;
            let (asic, chip) = match arguments.strip_prefix("2(") {
                Some(pair) => pair
                    .split_once(", ")
                    .context("identify_chip2's arguments")?,
                None => {
                    let chip = arguments
                        .strip_prefix('(')
                        .context("identify_chip's argument")?;
                    (chip, chip)
                }
            };
            found.push((family, asic.to_owned(), chip.to_owned()));
        }
    }
    ensure!(!found.is_empty(), "ac_identify_chip identifies no chip");
    Ok(found)
}

/// The address library's family numbers and ASIC revision ranges.
fn ranges(source: &str) -> (BTreeMap<String, u32>, BTreeMap<String, (u32, u32)>) {
    let number = |word: &str| {
        let word = word.trim().trim_end_matches(',');
        u32::from_str_radix(word.trim_start_matches("0x").trim_start_matches("0X"), 16).ok()
    };
    let mut families = BTreeMap::new();
    let mut ranges = BTreeMap::new();
    for line in source.lines() {
        let Some(rest) = line.strip_prefix("#define ") else {
            continue;
        };
        let mut words = rest.split_whitespace();
        let Some(name) = words.next() else {
            continue;
        };
        if let Some(family) = name.strip_prefix("FAMILY_") {
            if let Some(value) = words.next().and_then(number) {
                families.insert(family.to_owned(), value);
            }
        } else if let Some(asic) = name
            .strip_prefix("AMDGPU_")
            .and_then(|name| name.strip_suffix("_RANGE"))
            && let (Some(low), Some(high)) =
                (words.next().and_then(number), words.next().and_then(number))
        {
            ranges.insert(asic.to_owned(), (low, high));
        }
    }
    (families, ranges)
}

/// The shim's records: name, chip and whether it is an APU.
fn records(source: &str) -> Result<Vec<(String, String, bool)>> {
    let mut found = Vec::new();
    for record in source.split(".name = \"").skip(1) {
        let name = record.split('"').next().context("a record's name")?;
        let chip = record
            .split(".radeon_family = CHIP_")
            .nth(1)
            .and_then(|rest| rest.split(',').next())
            .context("a record's chip")?;
        let flags = record
            .split(".ids_flags = ")
            .nth(1)
            .and_then(|rest| rest.split(',').next())
            .context("a record's flags")?;
        let flags = flags
            .trim()
            .trim_start_matches("UINT64_C(")
            .trim_end_matches(')')
            .trim_start_matches("0x");
        let flags = u64::from_str_radix(flags, 16).context("a record's flags")?;
        found.push((name.to_owned(), chip.trim().to_owned(), flags & 1 != 0));
    }
    ensure!(!found.is_empty(), "amdgpu_devices.c records no device");
    Ok(found)
}

/// Linux's chip names Mesa calls otherwise.
const LINUX_NAMES: [(&str, &str); 8] = [
    ("SIENNA_CICHLID", "NAVI21"),
    ("NAVY_FLOUNDER", "NAVI22"),
    ("DIMGREY_CAVEFISH", "NAVI23"),
    ("BEIGE_GOBY", "NAVI24"),
    ("CYAN_SKILLFISH", "GFX1013"),
    ("YELLOW_CARP", "REMBRANDT"),
    ("TOPAZ", "ICELAND"),
    ("MULLINS", "KABINI"),
];

/// amdgpu's ID table: each chip's first PCI device ID and whether it is an
/// APU, by Mesa's name.
fn linux_ids(source: &str) -> BTreeMap<String, (u32, bool)> {
    let mut ids = BTreeMap::new();
    let table = source
        .split("pci_device_id pciidlist[] = {")
        .nth(1)
        .unwrap_or_default();
    for line in table.lines().take_while(|line| !line.starts_with("};")) {
        let Some(entry) = line.trim().strip_prefix("{0x1002, 0x") else {
            continue;
        };
        let Some(id) = entry
            .get(..4)
            .and_then(|id| u32::from_str_radix(id, 16).ok())
        else {
            continue;
        };
        let Some(chip) = entry.split("CHIP_").nth(1) else {
            continue;
        };
        let name: String = chip
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        let name = LINUX_NAMES
            .iter()
            .find(|(linux, _)| *linux == name)
            .map_or(name.clone(), |(_, mesa)| (*mesa).to_owned());
        ids.entry(name)
            .or_insert((id, entry.contains("AMD_IS_APU")));
    }
    ids
}

/// The generations RADV runs.
const LEVELS: [&str; 10] = [
    "GFX6", "GFX7", "GFX8", "GFX9", "GFX10", "GFX10_3", "GFX11", "GFX11_5", "GFX11_7", "GFX12",
];

/// The graphics IP version `ac_identify_chip` reads as `level`.
fn version(level: &str) -> Option<(u32, u32)> {
    Some(match level {
        "GFX6" => (6, 0),
        "GFX7" => (7, 0),
        "GFX8" => (8, 0),
        "GFX9" => (9, 0),
        "GFX10" => (10, 1),
        "GFX10_3" => (10, 3),
        "GFX11" => (11, 0),
        "GFX11_5" => (11, 5),
        "GFX11_7" => (11, 7),
        "GFX12" => (12, 0),
        _ => return None,
    })
}

/// Every device RADV creates, the owner's PC's graphics core first.
pub(super) fn devices(sources: &Sources, first: &str) -> Result<Vec<ShimDevice>> {
    let chips = chips(&sources.families)?;
    let order = |chip: &str| chips.iter().position(|(name, _)| name == chip);
    let (thresholds, lowest) = levels(&sources.levels)?;
    let level_of = |chip: &str| -> Result<String> {
        let index = order(chip).with_context(|| format!("amd_family.h has no CHIP_{chip}"))?;
        for (threshold, level) in &thresholds {
            let start = order(threshold).with_context(|| format!("no CHIP_{threshold}"))?;
            if index >= start {
                return Ok(level.clone());
            }
        }
        Ok(lowest.clone())
    };
    let (families, ranges) = ranges(&sources.ranges);
    let records = records(&sources.records)?;
    let ids = linux_ids(&sources.ids);
    let identified = identified(&sources.identify)?;
    // Each record with its generation and family.
    let recorded: Vec<(&(String, String, bool), String, String)> = records
        .iter()
        .map(|record| {
            let family = identified
                .iter()
                .find(|(_, _, chip)| *chip == record.1)
                .map(|(family, _, _)| family.clone())
                .unwrap_or_default();
            Ok((record, level_of(&record.1)?, family))
        })
        .collect::<Result<_>>()?;
    let mut devices: Vec<ShimDevice> = Vec::new();
    for (family, asic, chip) in identified.iter().cloned() {
        if devices.iter().any(|device| device.chip == chip) {
            continue;
        }
        // Compute-only chips have no graphics address range.
        let Some(&(low, _)) = ranges.get(&asic) else {
            continue;
        };
        let level = level_of(&chip)?;
        if !LEVELS.contains(&level.as_str()) {
            continue;
        }
        let note = chips
            .iter()
            .find(|(name, _)| *name == chip)
            .map(|(_, note)| note.clone())
            .unwrap_or_default();
        if let Some((name, _, apu)) = records.iter().find(|(_, recorded, _)| *recorded == chip) {
            devices.push(ShimDevice {
                name: name.clone(),
                chip,
                level,
                note,
                discrete: !*apu,
                derived: None,
            });
            continue;
        }
        let family_number = *families
            .get(&family)
            .with_context(|| format!("amdgpu_asic_addr.h has no FAMILY_{family}"))?;
        let rank = LEVELS.iter().position(|known| *known == level);
        let base = recorded
            .iter()
            .find(|(_, base_level, base_family)| *base_level == level && *base_family == family)
            .or_else(|| {
                recorded
                    .iter()
                    .find(|(_, base_level, _)| *base_level == level)
            })
            .or_else(|| {
                recorded
                    .iter()
                    .filter(|(_, base_level, _)| {
                        LEVELS.iter().position(|known| known == base_level) < rank
                    })
                    .max_by_key(|(_, base_level, _)| {
                        LEVELS.iter().position(|known| known == base_level)
                    })
            })
            .with_context(|| format!("no recorded device to derive {chip} from"))?;
        let ((base_name, _, base_apu), base_level, _) = base;
        let (device_id, apu) = ids.get(&chip).copied().unwrap_or_else(|| {
            (
                base_device_id(&sources.records, base_name).unwrap_or(0),
                *base_apu,
            )
        });
        ensure!(device_id != 0, "no PCI ID for {chip}");
        devices.push(ShimDevice {
            name: chip.to_lowercase(),
            chip: chip.clone(),
            level: level.clone(),
            note,
            discrete: !apu,
            derived: Some(Derived {
                base: base_name.clone(),
                family: family_number,
                external_rev: low,
                device_id,
                version: (*base_level != level).then(|| version(&level)).flatten(),
            }),
        });
    }
    let position = devices
        .iter()
        .position(|device| device.name == first)
        .with_context(|| format!("RADV creates no {first}"))?;
    let owner = devices.remove(position);
    devices.insert(0, owner);
    Ok(devices)
}

/// A record's PCI device ID.
fn base_device_id(records: &str, name: &str) -> Option<u32> {
    let record = records.split(&format!(".name = \"{name}\"")).nth(1)?;
    let id = record.split(".device_id = 0x").nth(1)?;
    u32::from_str_radix(id.split(',').next()?.trim(), 16).ok()
}

/// `radv_shim_derived.h`: the derived devices, as
/// `amd/tests/radv_shim_devices.c` reads them.
pub(super) fn derived_header(devices: &[ShimDevice]) -> Result<String> {
    let mut text = String::from(
        "/* Written by `cargo xtask gpu radv-test` (radv/devices.rs). */\n\
         static const struct stela_derived_device stela_derived[] = {\n",
    );
    let mut count = 0;
    for device in devices {
        let Some(derived) = &device.derived else {
            continue;
        };
        let (major, minor) = derived.version.unwrap_or((0, 0));
        text.push_str(&format!(
            "   {{\"{}\", \"{}\", CHIP_{}, {:#x}, {:#x}, {:#06x}, {major}, {minor}, {}}},\n",
            device.name,
            derived.base,
            device.chip,
            derived.family,
            derived.external_rev,
            derived.device_id,
            u32::from(!device.discrete),
        ));
        count += 1;
    }
    if count == 0 {
        bail!("no device to derive");
    }
    text.push_str("};\n");
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gpu::support::third_party;
    use std::fs;

    fn sources() -> Sources {
        let mesa = third_party("mesa").join("src/amd");
        let read = |path: std::path::PathBuf| {
            fs::read_to_string(&path).unwrap_or_else(|_| panic!("{} (fetch Mesa)", path.display()))
        };
        Sources {
            families: read(mesa.join("common/amd_family.h")),
            levels: read(mesa.join("common/amd_family.c")),
            identify: read(mesa.join("common/ac_gpu_info.c")),
            ranges: read(mesa.join("addrlib/src/amdgpu_asic_addr.h")),
            records: read(mesa.join("common/amdgpu_devices.c")),
            ids: read(third_party("linux").join("drivers/gpu/drm/amd/amdgpu/amdgpu_drv.c")),
        }
    }

    /// The test runs every chip RADV creates a device for at the Mesa pin,
    /// from GCN 1.0 to RDNA 4, and no compute-only chip: a chip Mesa adds
    /// joins it, or the table fails to derive it.
    #[test]
    fn every_chip_radv_supports_is_a_device() {
        let devices = devices(&sources(), "raphael_mendocino").unwrap();
        assert_eq!(devices[0].name, "raphael_mendocino");
        let chips: Vec<&str> = devices.iter().map(|device| device.chip.as_str()).collect();
        for chip in [
            "TAHITI",
            "PITCAIRN",
            "VERDE",
            "OLAND",
            "HAINAN",
            "BONAIRE",
            "KAVERI",
            "KABINI",
            "HAWAII",
            "TONGA",
            "ICELAND",
            "CARRIZO",
            "FIJI",
            "STONEY",
            "POLARIS10",
            "POLARIS11",
            "POLARIS12",
            "VEGAM",
            "VEGA10",
            "VEGA12",
            "VEGA20",
            "RAVEN",
            "RAVEN2",
            "RENOIR",
            "NAVI10",
            "NAVI12",
            "NAVI14",
            "GFX1013",
            "NAVI21",
            "NAVI22",
            "VANGOGH",
            "NAVI23",
            "NAVI24",
            "REMBRANDT",
            "RAPHAEL_MENDOCINO",
            "NAVI31",
            "NAVI32",
            "NAVI33",
            "PHOENIX",
            "PHOENIX2",
            "STRIX1",
            "STRIX_HALO",
            "KRACKAN1",
            "GFX1153",
            "GFX1156",
            "GFX1170",
            "GFX1171",
            "GFX1200",
            "GFX1201",
        ] {
            assert!(chips.contains(&chip), "{chip} is no device");
        }
        for compute in ["MI100", "MI200", "GFX940", "GFX1210"] {
            assert!(
                !chips.contains(&compute),
                "{compute} has no graphics RADV runs"
            );
        }
        // The families amdgpu's table names that RADV runs no device for
        // are those the port leaves out as having no Vulkan device.
        let unrun: Vec<String> = linux_ids(&sources().ids)
            .into_keys()
            .filter(|chip| !chips.contains(&chip.as_str()))
            .collect();
        let mut left_out = stela_driver_core::amd_metal::NO_VULKAN_FAMILIES.to_vec();
        left_out.sort_unstable();
        assert_eq!(unrun, left_out);
        assert_eq!(
            devices
                .iter()
                .filter(|device| device.derived.is_none())
                .count(),
            20
        );
        let names: std::collections::BTreeSet<_> = devices.iter().map(|d| &d.name).collect();
        assert_eq!(names.len(), devices.len(), "each name once");
    }

    /// A derived device's family and revision identify its own chip, as
    /// `ac_identify_chip` reads them: the last range of its family holding
    /// the revision wins.
    #[test]
    fn derived_devices_identify_as_their_chip() {
        let sources = sources();
        let (families, ranges) = ranges(&sources.ranges);
        let identified = identified(&sources.identify).unwrap();
        for device in devices(&sources, "raphael_mendocino").unwrap() {
            let Some(derived) = &device.derived else {
                continue;
            };
            let chip = identified
                .iter()
                .rfind(|(family, asic, _)| {
                    families.get(family) == Some(&derived.family)
                        && ranges
                            .get(asic)
                            .is_some_and(|&(low, high)| (low..high).contains(&derived.external_rev))
                })
                .map(|(_, _, chip)| chip.as_str());
            assert_eq!(chip, Some(device.chip.as_str()), "{}", device.name);
        }
    }

    #[test]
    fn derived_devices_take_their_generation_and_amdgpus_ids() {
        let devices = devices(&sources(), "raphael_mendocino").unwrap();
        let find = |name: &str| devices.iter().find(|device| device.name == name).unwrap();
        // Kaveri: a GFX7 APU, from the GFX7 discrete record.
        let kaveri = find("kaveri");
        assert_eq!(kaveri.level, "GFX7");
        assert!(!kaveri.discrete);
        let derived = kaveri.derived.as_ref().unwrap();
        assert_eq!(
            (derived.base.as_str(), derived.device_id),
            ("bonaire", 0x1304)
        );
        assert_eq!(derived.version, None);
        // GFX11.7 has no record: the nearest generation's, with its IP.
        let gfx1170 = find("gfx1170");
        assert_eq!(gfx1170.derived.as_ref().unwrap().version, Some((11, 7)));
        // Navi 22 is in amdgpu's table by Linux's name for it.
        let navi22 = find("navi22");
        let derived = navi22.derived.as_ref().unwrap();
        assert_eq!((derived.base.as_str(), navi22.discrete), ("navi21", true));
        assert_eq!(derived.device_id, 0x73c0);
        let header = derived_header(&devices).unwrap();
        assert!(
            header.contains("{\"kaveri\", \"bonaire\", CHIP_KAVERI, 0x7d, 0x1, 0x1304, 0, 0, 1},")
        );
    }

    #[test]
    fn excerpts_parse() {
        let (thresholds, lowest) = levels(
            "enum amd_gfx_level ac_get_gfx_level(enum radeon_family family)\n{\n   if (family >= CHIP_B)\n      return GFX7;\n\n   return GFX6;\n}\n",
        )
        .unwrap();
        assert_eq!(thresholds, [("B".to_owned(), "GFX7".to_owned())]);
        assert_eq!(lowest, "GFX6");
        let (families, ranges) = ranges(
            "#define FAMILY_SI      0x6E //# 110\n#define AMDGPU_TAHITI_RANGE     0x05, 0x14 //#\n",
        );
        assert_eq!(families["SI"], 0x6e);
        assert_eq!(ranges["TAHITI"], (5, 0x14));
        let ids = linux_ids(
            "u16 amdgpu_unsupported_pciidlist[] = {\n\t0x4354,\n};\n\
             pci_device_id pciidlist[] = {\n\t{0x1002, 0x73C0, PCI_ANY_ID, PCI_ANY_ID, 0, 0, CHIP_NAVY_FLOUNDER},\n};\n",
        );
        assert_eq!(ids["NAVI22"], (0x73c0, false));
    }
}
