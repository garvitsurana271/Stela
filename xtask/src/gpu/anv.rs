//! ANV on xe and on i915 without a GPU, on the build host: Mesa built for
//! this machine with the image's ANV options, its objects linked into a test
//! program as Mesa links its driver, with Stela's libdrm stand-in
//! (`compat/mesa/drm.c`) in libdrm's place and Stela's NIR options hook,
//! run over a render node that answers the kernel driver's queries and
//! accepts its binds and submissions without running them: xe's
//! (`intel/hosted/xe_noop_drm_shim.c`) or Mesa's own i915 shim
//! (`intel/hosted/i915_noop_drm_shim.c`), on Mesa's DRM shim. For each
//! device each driver binds, the program creates ANV's device, builds a
//! compute pipeline through BRW from SPIR-V and one from NIR, as the Metal
//! device passes the AIR compiler's NIR, and submits one dispatch
//! (`intel/tests/anv_device.c`); every ioctl ANV made must be one Stela's
//! driver serves on a render node. It also counts the devices where ANV
//! meets the Metal device's Vulkan requirements and reads and writes Metal's
//! tier 2 read-write texture formats.
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

/// i915's own tables, read as its build reads them.
#[allow(dead_code)]
#[path = "../../../crates/stela-driver-gpu/build/i915_tables.rs"]
mod i915_tables;

/// A kernel driver ANV runs over in the test.
struct Kernel {
    name: &'static str,
    /// Its no-op render node, under `crates/stela-driver-gpu/src`.
    shim: &'static str,
    /// Where it registers its ioctls, under Linux's tree, and their prefix.
    ioctls: (&'static str, &'static str),
}

const XE: Kernel = Kernel {
    name: "xe",
    shim: "intel/hosted/xe_noop_drm_shim.c",
    ioctls: ("drivers/gpu/drm/xe/xe_device.c", "XE_"),
};
const I915: Kernel = Kernel {
    name: "i915",
    shim: "intel/hosted/i915_noop_drm_shim.c",
    ioctls: ("drivers/gpu/drm/i915/i915_driver.c", "I915_"),
};

/// The graphics versions ANV drives (`anv_physical_device_try_create`
/// refuses the rest). i915 drives the earlier ones with displays only: the
/// image's ANV has no Gen8 compiler (`-Dintel-elk=false`), and hasvk, Mesa's
/// Vulkan driver for Gen7 and Gen8, lacks the Metal device's descriptor
/// indexing, update-after-bind and non-uniform image indexing, its
/// formatless storage image reads, unused-attachment dynamic rendering and
/// unrestricted dynamic topology (`vulkan_hasvk/anv_device.c`); Gen7 also
/// lacks buffer device addresses and half floats, and Ivy Bridge
/// conditional rendering.
const ANV_VERSIONS: std::ops::RangeInclusive<u8> = 9..=11;

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

/// The devices of i915's PCI ID table that i915 binds without force probe
/// and xe's table does not hold (those are xe's), with Mesa's answer for
/// each, from Gen9 on; and how many of i915's earlier devices there are, by
/// graphics version, which show displays only by rule ([`ANV_VERSIONS`]).
fn i915_devices() -> Result<(Vec<Device>, std::collections::BTreeMap<u8, usize>)> {
    let linux = third_party("linux");
    let read = |path: PathBuf| {
        fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))
    };
    let ids = read(linux.join("include/drm/intel/pciids.h"))?;
    let xe = xe_tables::devices(&read(linux.join("drivers/gpu/drm/xe/xe_pci.c"))?, &ids)
        .map_err(|error| anyhow::anyhow!("xe's PCI ID table: {error}"))?;
    let table = i915_tables::devices(&read(linux.join("drivers/gpu/drm/i915/i915_pci.c"))?, &ids)
        .map_err(|error| anyhow::anyhow!("i915's PCI ID table: {error}"))?;
    let mesa = mesa_devices(&read(
        third_party("mesa").join("include/pci_ids/iris_pci_ids.h"),
    )?);
    let mut devices = Vec::new();
    let mut earlier = std::collections::BTreeMap::new();
    for (id, described) in table {
        if described.forced || xe.contains_key(&id) {
            continue;
        }
        if described.version < *ANV_VERSIONS.start() {
            *earlier.entry(described.version).or_default() += 1;
            continue;
        }
        ensure!(
            ANV_VERSIONS.contains(&described.version),
            "i915 binds {id:04x}, of graphics version {}, which xe's table does not hold",
            described.version
        );
        let listed = mesa.get(&id);
        devices.push(Device {
            id,
            platform: described.platform,
            family: listed.map(|(family, _)| family.clone()),
            discrete: described.discrete,
            absent: match listed {
                None => Some("Mesa's Intel device table does not list it"),
                Some((_, true)) => Some("Mesa lists it only for its force probe"),
                Some((_, false)) => None,
            },
        });
    }
    ensure!(
        !devices.is_empty(),
        "i915's table holds no Gen9 to Gen11 device"
    );
    Ok((devices, earlier))
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
    let (shims, program) = link_test(&build, &output, &link, &tools.glslang.0)?;
    let mut failures = Vec::new();
    run(
        &XE,
        &devices()?,
        &program,
        &shims[0],
        &output,
        &mut failures,
    )?;
    let (devices, earlier) = i915_devices()?;
    run(&I915, &devices, &program, &shims[1], &output, &mut failures)?;
    for (version, count) in &earlier {
        println!(
            "i915 graphics version {version}: {count} devices show displays only by rule: \
             ANV drives graphics versions {} to {} of i915's, and hasvk lacks the Metal \
             device's requirements",
            ANV_VERSIONS.start(),
            ANV_VERSIONS.end()
        );
    }
    ensure!(
        failures.is_empty(),
        "ANV failed on the host (logs in {}):\n{}",
        output.display(),
        failures.join("\n")
    );
    Ok(())
}

/// Runs the test program over `kernel`'s shim for each of `devices`, logging
/// each run's output beside the program, and adds each failure to
/// `failures`.
fn run(
    kernel: &Kernel,
    devices: &[Device],
    program: &Path,
    shim: &Path,
    output: &Path,
    failures: &mut Vec<String>,
) -> Result<()> {
    let (registry, prefix) = kernel.ioctls;
    let served = hosted::render_ioctls(
        &fs::read_to_string(third_party("linux").join(registry))?,
        prefix,
    );
    ensure!(
        !served.is_empty(),
        "{registry} serves no ioctl on a render node"
    );
    let marker = format!("{} shim: ioctls", kernel.name);
    let (mut run, mut metal, mut read_write) = (0, 0, 0);
    // Each Mesa family's devices, and how many met the Metal device's
    // requirements and the tier 2 formats.
    let mut families = std::collections::BTreeMap::<String, (usize, usize, usize)>::new();
    for device in devices {
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
        let result = Command::new(program)
            .args([
                id.as_str(),
                if device.discrete {
                    "discrete"
                } else {
                    "integrated"
                },
                kernel.name,
            ])
            .env("LD_PRELOAD", shim)
            .env("INTEL_STUB_GPU_DEVICE_ID", &id)
            .stdin(Stdio::null())
            .output()?;
        let stdout = String::from_utf8_lossy(&result.stdout);
        let stderr = String::from_utf8_lossy(&result.stderr);
        fs::write(
            output.join(format!("{}-{id}.log", kernel.name)),
            format!("{stdout}{stderr}"),
        )?;
        let used = hosted::used_ioctls(&stderr, &marker);
        let unserved: Vec<_> = used
            .keys()
            .filter(|ioctl| !served.contains(ioctl))
            .cloned()
            .collect();
        match (
            result.status.success(),
            used.is_empty(),
            unserved.is_empty(),
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
                    "  {} ioctls: {}",
                    kernel.name,
                    used.iter()
                        .map(|(ioctl, count)| format!("{ioctl} {count}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            (true, _, false) => failures.push(format!(
                "{name}: ANV used ioctls Stela's {} does not serve on a render node: {}",
                kernel.name,
                unserved.join(", ")
            )),
            _ => failures.push(format!(
                "{name} on {} failed with {}: {}",
                kernel.name,
                result.status,
                stderr.trim()
            )),
        }
    }
    for (family, (count, met, formats)) in &families {
        println!(
            "Mesa family {family} on {}: {count} devices, Metal requirements met on {met}, \
             tier 2 read-write formats on {formats}",
            kernel.name
        );
    }
    let absent = devices.len() - run;
    println!(
        "ANV ran on {0}'s ioctls for {run} devices of {0}'s table, with no GPU; the Metal \
         device's Vulkan requirements are met on {metal} of them, and Metal's tier 2 \
         read-write texture formats on {read_write}; {absent} devices get no Vulkan device \
         from ANV and show displays only",
        kernel.name
    );
    Ok(())
}

/// What the test program prints for a device the Metal device admits.
const METAL_MET: &str = "Metal device: every requirement met";
/// What it prints for a device that reads and writes every format of
/// Metal's read-write texture tier 2.
const READ_WRITE_MET: &str = "Metal read-write textures: every tier 2 format read and written";

/// Builds the xe and i915 shims and the test program; their paths.
fn link_test(
    build: &Path,
    output: &Path,
    link: &[String],
    glslang: &Path,
) -> Result<(Vec<PathBuf>, PathBuf)> {
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
    // The DRM shim's stat and stat64 come first (see the xe shim's source).
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
    let mut shims = Vec::new();
    for kernel in [&XE, &I915] {
        let stem = Path::new(kernel.shim).file_stem().context("shim name")?;
        let shim_object = output.join(stem).with_extension("o");
        crate::tools::exec(
            hosted::compile_command(build, SHIM_MODEL, &sources.join(kernel.shim), &shim_object)?
                .args(["-fPIC", "-Isrc/intel", "-Isrc/intel/dev"])
                .arg(format!(
                    "-I{}",
                    third_party("mesa").join("src/intel").display()
                )),
        )?;
        let shim = output.join(stem).with_extension("so");
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
        shims.push(shim);
    }
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
    Ok((shims, program))
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
