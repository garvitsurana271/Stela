//! ANV on xe without a GPU, on the build host: Mesa built for this machine
//! with the image's ANV options, its objects linked into a test program as
//! Mesa links its driver, with Stela's libdrm stand-in
//! (`compat/mesa/drm.c`) in libdrm's place and Stela's NIR options hook,
//! run over an xe render node that answers xe's queries and accepts its
//! binds and submissions without running them
//! (`intel/hosted/xe_noop_drm_shim.c`, on Mesa's DRM shim). For each
//! platform xe binds, the program creates ANV's device, builds a compute
//! pipeline through BRW from SPIR-V and one from NIR, as the Metal device
//! passes the AIR compiler's NIR, and submits one dispatch
//! (`intel/tests/anv_device.c`); every xe ioctl ANV made must be one Stela's
//! xe registers. It also counts the platforms where ANV meets the Metal
//! device's Vulkan requirements and reads and writes Metal's tier 2
//! read-write texture formats.
//!
//! Needs what ANV's image build needs (`clc.rs`: mesa_clc's LLVM, Clang,
//! SPIRV-LLVM-Translator and SPIRV-Tools, and glslang) and the host's
//! `libdrm.so.2`, whose mode calls the display WSI units take; the image
//! leaves those units out.
use super::hosted;
use super::support::{build_directory, gpu, third_party, tool};
use super::toolchain::GENERATORS;
use anyhow::{Context, Result, bail, ensure};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

/// xe's own tables, read as its build reads them.
#[allow(dead_code)]
#[path = "../../../crates/stela-driver-gpu/build/xe_tables.rs"]
mod xe_tables;

/// A device of xe's table that Stela's xe stack admits, as the test treats
/// it: run on ANV, or listed as one ANV does not drive.
struct Device {
    id: u16,
    /// xe's platform and Mesa's device family, for the log.
    platform: String,
    family: Option<String>,
    /// xe's `is_dgfx`: Vulkan must report a discrete GPU.
    discrete: bool,
    /// Why ANV has no Vulkan device for it, by Mesa's own table: the GPU
    /// shows displays only and the Metal device goes elsewhere.
    absent: Option<&'static str>,
}

/// Every device of xe's PCI ID table whose platform xe's firmware table
/// gives a GuC image (Stela's xe refuses the others before any claim), with
/// Mesa's answer for it from ANV's device table (`iris_pci_ids.h`): a
/// device it does not list, or lists only for its force probe, gets no
/// Vulkan device from ANV.
fn devices() -> Result<Vec<Device>> {
    let linux = third_party("linux");
    let read = |path: PathBuf| {
        fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))
    };
    let table = xe_tables::devices(
        &read(linux.join("drivers/gpu/drm/xe/xe_pci.c"))?,
        &read(linux.join("include/drm/intel/pciids.h"))?,
    )
    .map_err(|error| anyhow::anyhow!("xe's PCI ID table: {error}"))?;
    let firmware = xe_tables::uc_firmware(&read(linux.join("drivers/gpu/drm/xe/xe_uc_fw.c"))?)
        .map_err(|error| anyhow::anyhow!("xe's firmware table: {error}"))?;
    let mesa = mesa_devices(&read(
        third_party("mesa").join("include/pci_ids/iris_pci_ids.h"),
    )?);
    let mut devices = Vec::new();
    for (id, (_, _, platform, _, discrete)) in table {
        if firmware
            .get(&platform)
            .is_none_or(|files| files[0].is_none())
        {
            continue;
        }
        let listed = mesa.get(&id);
        devices.push(Device {
            id,
            platform,
            family: listed.map(|(family, _)| family.clone()),
            discrete,
            absent: match listed {
                None => Some("Mesa's Intel device table does not list it"),
                Some((_, true)) => Some("Mesa lists it only for its force probe"),
                Some((_, false)) => None,
            },
        });
    }
    ensure!(!devices.is_empty(), "xe's table admits no device");
    Ok(devices)
}

/// Mesa's Intel device table: each PCI device ID with its family and
/// whether Mesa drives it only by force probe.
fn mesa_devices(table: &str) -> std::collections::BTreeMap<u16, (String, bool)> {
    let mut devices = std::collections::BTreeMap::new();
    for line in table.lines() {
        let Some(entry) = line.trim().strip_prefix("CHIPSET(") else {
            continue;
        };
        let mut fields = entry.split(',').map(str::trim);
        let (Some(id), Some(family)) = (fields.next(), fields.next()) else {
            continue;
        };
        let Some(id) = id
            .strip_prefix("0x")
            .and_then(|digits| u16::from_str_radix(digits, 16).ok())
        else {
            continue;
        };
        devices.insert(id, (family.to_owned(), entry.contains("FORCE_PROBE")));
    }
    devices
}
/// The driver's link target, whose inputs the test program links.
const DRIVER: &str = "src/intel/vulkan/libvulkan_intel.so";
/// One of ANV's own units and one of the DRM shim's, whose flags the test's
/// sources compile with.
const ANV_MODEL: &str = "src/intel/vulkan/anv_util.c";
const SHIM_MODEL: &str = "src/drm-shim/device.c";
/// The shim's objects as Meson builds them, and the archives it links.
const SHIM_OBJECTS: [&str; 2] = [
    "src/drm-shim/libdrm_shim.a.p/drm_shim.c.o",
    "src/drm-shim/libdrm_shim.a.p/device.c.o",
];
const SHIM_ARCHIVES: [&str; 5] = [
    "src/intel/dev/libintel_dev.a",
    "src/util/libmesa_util.a",
    "src/c11/impl/libmesa_util_c11.a",
    "src/util/blake3/libblake3.a",
    "src/util/libmesa_util_simd.a",
];

pub fn test(jobs: u16) -> Result<()> {
    let directory = build_directory("anv-host");
    let build = directory.join("build");
    let output = directory.join("test");
    let bin = directory.join("bin");
    let packages = directory.join("packages");
    let local = crate::tools::root().join(".tools/gpu");
    let host = std::env::join_paths(
        [local.join(GENERATORS), local.join("rust/bin")]
            .into_iter()
            .chain(std::env::split_paths(
                &std::env::var_os("PATH").unwrap_or_default(),
            )),
    )?;
    let tools = match super::clc::tools(Path::new(&host), jobs)? {
        Ok(tools) => tools,
        Err(reason) => bail!("ANV cannot be built on this host: {reason}"),
    };
    for path in [&output, &bin, &packages] {
        fs::create_dir_all(path)?;
    }
    for program in &tools.programs {
        fs::copy(program, bin.join(program.file_name().context("host tool")?))?;
    }
    let path =
        std::env::join_paths(std::iter::once(bin.clone()).chain(std::env::split_paths(&host)))?;
    // libdrm's headers as the image has them; its calls are Stela's.
    fs::write(
        packages.join("libdrm.pc"),
        format!(
            "Name: libdrm\nDescription: vendored headers\nVersion: {}\n\
             Cflags: -I{1} -I{1}/include/drm\n",
            super::nvk::LIBDRM_VERSION,
            super::support::slashed(&third_party("libdrm"))
        ),
    )?;
    ensure!(
        third_party("libdrm").join("xf86drm.h").is_file(),
        "third_party/libdrm is missing; run `cargo xtask gpu fetch`"
    );
    let arguments = hosted::arguments("intel", &super::nvk::ANV_OPTIONS);
    hosted::configure(&directory, &build, &packages, &path, &arguments)?;
    let link = hosted::link_command(&build, DRIVER)?;
    let mut targets = hosted::built_inputs(&link);
    targets.extend(SHIM_OBJECTS.map(str::to_owned));
    targets.extend(SHIM_ARCHIVES.map(str::to_owned));
    crate::tools::exec(
        Command::new(tool("ninja", GENERATORS)?)
            .arg("-C")
            .arg(&build)
            .arg(format!("-j{jobs}"))
            .args(&targets)
            .env("PATH", &path),
    )?;
    let (shim, program) = link_test(&build, &output, &link, &tools.glslang.0)?;
    let registered = hosted::registered_ioctls(
        &fs::read_to_string(third_party("linux").join("drivers/gpu/drm/xe/xe_device.c"))?,
        "XE_",
    );
    ensure!(!registered.is_empty(), "xe_device.c registers no ioctl");
    let devices = devices()?;
    let mut failures = Vec::new();
    let (mut run, mut metal, mut read_write) = (0, 0, 0);
    // Each Mesa family's devices, and how many met the Metal device's
    // requirements and the tier 2 formats.
    let mut families = std::collections::BTreeMap::<String, (usize, usize, usize)>::new();
    for device in &devices {
        let id = format!("{:04x}", device.id);
        let name = format!(
            "{id} {} ({})",
            device.platform,
            device.family.as_deref().unwrap_or("not in Mesa")
        );
        if let Some(reason) = device.absent {
            println!("{name}: no Vulkan device from ANV ({reason}): shows displays only");
            continue;
        }
        let result = Command::new(&program)
            .args([
                id.as_str(),
                if device.discrete {
                    "discrete"
                } else {
                    "integrated"
                },
            ])
            .env("LD_PRELOAD", &shim)
            .env("INTEL_STUB_GPU_DEVICE_ID", &id)
            .stdin(Stdio::null())
            .output()?;
        let stdout = String::from_utf8_lossy(&result.stdout);
        let stderr = String::from_utf8_lossy(&result.stderr);
        fs::write(
            output.join(format!("{id}.log")),
            format!("{stdout}{stderr}"),
        )?;
        let used = hosted::used_ioctls(&stderr, "xe shim: ioctls");
        let unregistered: Vec<_> = used
            .keys()
            .filter(|ioctl| !registered.contains(ioctl))
            .cloned()
            .collect();
        match (
            result.status.success(),
            used.is_empty(),
            unregistered.is_empty(),
        ) {
            (true, false, true) => {
                run += 1;
                let met = stdout.contains(METAL_MET);
                let formats = stdout.contains(READ_WRITE_MET);
                metal += usize::from(met);
                read_write += usize::from(formats);
                let family = families
                    .entry(device.family.clone().unwrap_or_default())
                    .or_default();
                family.0 += 1;
                family.1 += usize::from(met);
                family.2 += usize::from(formats);
                println!("{name}: {}", stdout.trim().replace('\n', "; "));
                println!(
                    "  xe ioctls: {}",
                    used.iter()
                        .map(|(ioctl, count)| format!("{ioctl} {count}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            (true, _, false) => failures.push(format!(
                "{name}: ANV used ioctls Stela's xe does not register: {}",
                unregistered.join(", ")
            )),
            _ => failures.push(format!(
                "{name} failed with {}: {}",
                result.status,
                stderr.trim()
            )),
        }
    }
    ensure!(
        failures.is_empty(),
        "ANV on xe failed on the host (logs in {}):\n{}",
        output.display(),
        failures.join("\n")
    );
    for (family, (count, met, formats)) in &families {
        println!(
            "Mesa family {family}: {count} devices, Metal requirements met on {met}, \
             tier 2 read-write formats on {formats}"
        );
    }
    let absent = devices.len() - run;
    println!(
        "ANV ran on xe's ioctls for {run} devices of xe's table, with no GPU; the Metal \
         device's Vulkan requirements are met on {metal} of them, and Metal's tier 2 \
         read-write texture formats on {read_write}; {absent} devices get no Vulkan device \
         from ANV and show displays only"
    );
    Ok(())
}

/// What the test program prints for a device the Metal device admits.
const METAL_MET: &str = "Metal device: every requirement met";
/// What it prints for a device that reads and writes every format of
/// Metal's read-write texture tier 2.
const READ_WRITE_MET: &str = "Metal read-write textures: every tier 2 format read and written";

/// Builds the shim and the test program; their paths.
fn link_test(
    build: &Path,
    output: &Path,
    link: &[String],
    glslang: &Path,
) -> Result<(PathBuf, PathBuf)> {
    let sources = gpu();
    crate::tools::exec(
        Command::new(glslang)
            .args([
                "-V",
                "--target-env",
                "vulkan1.1",
                "--vn",
                "stela_anv_device_spirv",
            ])
            .arg("-o")
            .arg(output.join("anv_device_shader.h"))
            .arg(sources.join("intel/tests/anv_device.comp"))
            .stdout(Stdio::null()),
    )?;
    let shim_object = output.join("xe_noop_drm_shim.o");
    crate::tools::exec(
        hosted::compile_command(
            build,
            SHIM_MODEL,
            &sources.join("intel/hosted/xe_noop_drm_shim.c"),
            &shim_object,
        )?
        .args(["-fPIC", "-Isrc/intel", "-Isrc/intel/dev"])
        .arg(format!(
            "-I{}",
            third_party("mesa").join("src/intel").display()
        )),
    )?;
    // The shim's stat and stat64 come first (see the shim's source).
    let renamed = output.join("drm_shim.o");
    crate::tools::exec(
        Command::new("objcopy")
            .args([
                "--redefine-sym",
                "stat=stela_drm_shim_stat",
                "--redefine-sym",
                "stat64=stela_drm_shim_stat64",
            ])
            .arg(build.join(SHIM_OBJECTS[0]))
            .arg(&renamed),
    )?;
    let shim = output.join("xe_noop_drm_shim.so");
    crate::tools::exec(
        Command::new("cc")
            .current_dir(build)
            .arg("-shared")
            .arg("-o")
            .arg(&shim)
            .arg(&shim_object)
            .arg(&renamed)
            .arg(SHIM_OBJECTS[1])
            .args(SHIM_ARCHIVES)
            .args(["-ldl", "-lpthread", "-lm"]),
    )?;
    let libc = format!("-idirafter{}", sources.join("compat/libc").display());
    let units = [
        (
            "compat/mesa/anv_nir_options.c",
            vec!["-Wno-missing-prototypes".to_owned()],
        ),
        // Exported, so the shim's own libdrm calls find them.
        (
            "compat/mesa/drm.c",
            vec![libc.clone(), "-fvisibility=default".to_owned()],
        ),
        (
            "intel/tests/anv_nir_shader.c",
            vec!["-Wno-missing-prototypes".to_owned()],
        ),
        (
            "intel/tests/anv_device.c",
            vec![libc, format!("-I{}", output.display())],
        ),
    ];
    let mut objects = Vec::new();
    for (source, flags) in units {
        let object = output.join(
            Path::new(source)
                .with_extension("o")
                .file_name()
                .context("unit name")?,
        );
        crate::tools::exec(
            hosted::compile_command(build, ANV_MODEL, &sources.join(source), &object)?.args(flags),
        )?;
        objects.push(object);
    }
    let libdrm = [hosted::host_libdrm()?.display().to_string()];
    let program = output.join("anv_device");
    let words = hosted::program_link(link, &program, &objects, &libdrm);
    crate::tools::exec(Command::new(&words[0]).current_dir(build).args(&words[1..]))?;
    Ok((shim, program))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_test_build_keeps_the_images_options_but_its_own() {
        let arguments = hosted::arguments("intel", &super::super::nvk::ANV_OPTIONS);
        assert!(arguments.contains(&"-Dvulkan-drivers=intel".to_owned()));
        assert!(arguments.contains(&"-Dintel-rt=disabled".to_owned()));
        assert!(arguments.contains(&"-Dtools=drm-shim".to_owned()));
    }

    #[test]
    fn mesas_table_says_which_devices_anv_drives() {
        let table = "CHIPSET(0x9a49, tgl_gt2, \"TGL GT2\", \"Intel(R) Iris(R) Xe Graphics\")\n\
                     /* a comment */\n\
                     CHIPSET(0xd750, nvl_p, \"NVL\", \"Intel(R) Graphics\", FORCE_PROBE)\n";
        let devices = mesa_devices(table);
        assert_eq!(devices.get(&0x9a49), Some(&("tgl_gt2".to_owned(), false)));
        assert_eq!(devices.get(&0xd750), Some(&("nvl_p".to_owned(), true)));
        assert_eq!(devices.len(), 2);
    }
}
