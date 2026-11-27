//! What a lost device's GPU was running, for a fault such as nouveau's Xid
//! 13 ("Graphics Exception: Shader Program Header N Error"), which names no
//! shader: each pipeline that the command buffers the GPU had not finished
//! bound, by its functions, with each of its stages' shader program header
//! (SPH) as NAK wrote it and the size and SHA-256 of its machine code, as
//! NVK reports them (`VK_KHR_pipeline_executable_properties`, Mesa patch
//! 0043). The first fault of a boot also gives that code in hex, up to
//! [`HEX_BUDGET`] bytes, so the same pipeline on a GPU that runs it can be
//! compared byte for byte.
//!
//! Always on and bounded, as the other reports of a loss are: nothing is
//! asked of the driver until the device is lost, and only the first
//! [`REPORTED_FAULTS`] losses of a boot are dumped ([`Limiter`]). The lines
//! hold Metal function names, pipeline ids, driver statistics and shader
//! bytes; nothing of the user's. Each starts with [`PREFIX`], which the
//! platform logs as it is, apart from the refusals it logs once per text.
use alloc::{format, string::String, vec::Vec};
use core::{
    fmt::Write,
    sync::atomic::{AtomicU32, Ordering},
};

/// How each line of a fault dump starts, after which the platform's log
/// writes it whole (`GPU FAULT ...`).
pub const PREFIX: &str = "fault dump ";
/// The losses of a boot that are dumped: a WindowServer that faults at each
/// start loses the device every few seconds, and the first ones say it all.
pub const REPORTED_FAULTS: u32 = 3;
/// The pipelines a dump names, render pipelines first.
pub const MAX_PIPELINES: usize = 8;
/// The Vulkan pipelines of one Metal pipeline a dump names: its first, then
/// its bicubic and mesh variants or a compute pipeline's threadgroup shapes.
pub const MAX_VARIANTS: usize = 4;
/// The bytes of machine code the first fault's dump gives in hex: about
/// 19 KB of `GPU FAULT` text, so the boot log's 64 KiB burst for the family
/// (`stela_boot_core::log_budget::BURST_BYTES`) still holds the pipeline and
/// stage lines of all [`REPORTED_FAULTS`] faults.
pub const HEX_BUDGET: usize = 6 << 10;
/// The bytes of code on each hex line.
pub const HEX_LINE_BYTES: usize = 64;

/// The names of the statistics NVK reports for an executable that a
/// shader's line gives, and the word it gives each as.
const STATISTICS: [(&str, &str); 4] = [
    ("Instruction count", "instructions"),
    ("Number of GPRs", "gprs"),
    ("SLM size", "slm"),
    ("Program header address", "sph_va"),
];
/// NVK's statistic that the upload queue wrote the shader (1) rather than
/// the CPU through a mapping of the shader heap (0).
const UPLOADED_BY_QUEUE: &str = "Uploaded by queue";

/// One stage of a pipeline as the driver compiled it.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ShaderImage {
    /// The driver's name for the stage's executable (`vertex`, `fragment`).
    pub stage: String,
    /// The shader program header, in the 32-bit words the GPU reads before
    /// the code; empty for compute, which has none.
    pub header: Vec<u32>,
    /// A mesh shader's geometry program header, when it has one.
    pub geometry_header: Vec<u32>,
    /// The machine code.
    pub code: Vec<u8>,
    /// The driver's statistics of it, by name.
    pub statistics: Vec<(String, u64)>,
}

impl ShaderImage {
    /// The statistic the driver names `name`.
    pub fn statistic(&self, name: &str) -> Option<u64> {
        self.statistics
            .iter()
            .find(|(named, _)| named == name)
            .map(|&(_, value)| value)
    }
}

/// `bytes` as the little-endian 32-bit words a program header is read in;
/// a trailing partial word is dropped.
pub fn words(bytes: &[u8]) -> Vec<u32> {
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|&word| u32::from_le_bytes(word))
        .collect()
}

/// The faults of a boot left to dump.
pub struct Limiter {
    taken: AtomicU32,
}

impl Limiter {
    pub const fn new() -> Self {
        Self {
            taken: AtomicU32::new(0),
        }
    }

    /// The number, from 1, of the fault to dump now, or `None` once
    /// [`REPORTED_FAULTS`] were.
    pub fn take(&self) -> Option<u32> {
        let taken = self
            .taken
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |taken| {
                (taken < REPORTED_FAULTS).then_some(taken + 1)
            })
            .ok()?;
        Some(taken + 1)
    }

    /// The boot's limiter, which every service of the boot shares.
    #[cfg(not(test))]
    pub fn boot() -> &'static Self {
        static BOOT: Limiter = Limiter::new();
        &BOOT
    }

    /// A unit test's service has a limiter of its own, so tests running
    /// together each see their first fault.
    #[cfg(test)]
    pub fn boot() -> &'static Self {
        alloc::boxed::Box::leak(alloc::boxed::Box::new(Self::new()))
    }
}

impl Default for Limiter {
    fn default() -> Self {
        Self::new()
    }
}

/// The fault a dump's opening line numbers, for the platform to put the
/// kernel's lines of the fault after it.
pub fn opening_fault(line: &str) -> Option<u32> {
    let (number, _) = line.strip_prefix(PREFIX)?.split_once('/')?;
    number.parse().ok()
}

/// The first line of fault `fault`'s dump: the command buffers the GPU had
/// not finished, by serial, and the pipelines they bound, by label.
pub fn opening(fault: u32, serials: &[u64], pipelines: &[String]) -> String {
    let mut line = format!("{PREFIX}{fault}/{REPORTED_FAULTS}: the device was lost ");
    match serials {
        [] => line.push_str("with no submitted command buffer unfinished"),
        [only] => {
            let _ = write!(line, "running command buffer {only}");
        }
        [first, .., last] => {
            let _ = write!(
                line,
                "running command buffers {first} to {last} ({})",
                serials.len()
            );
        }
    }
    match pipelines {
        [] if !serials.is_empty() => line.push_str(", which bound no pipeline"),
        [] => {}
        _ => {
            let _ = write!(
                line,
                ", which bound {} pipeline{}: {}",
                pipelines.len(),
                if pipelines.len() == 1 { "" } else { "s" },
                pipelines.join(", ")
            );
            if pipelines.len() > MAX_PIPELINES {
                let _ = write!(line, " (the first {MAX_PIPELINES} follow)");
            }
        }
    }
    line
}

/// The unfinished command buffers whose progress a dump gives.
pub const MAX_PROGRESS: usize = 4;

/// How far an unfinished command buffer's timestamps got by the loss: its
/// start, then the end of each span between its marks (after a pass, before
/// the barrier after dispatches). A span whose start is written and whose
/// end is not is the work the GPU did not finish: on a reset for a hang,
/// the work that hung, or work behind it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Progress {
    /// No timestamp was written: the GPU had not started it.
    NotStarted,
    /// Span `index` of `count` started and never ended: what it did, and
    /// the pipelines it bound by label and entry points.
    Stopped {
        index: usize,
        count: usize,
        shape: String,
        pipelines: Vec<String>,
    },
    /// Every span ended: the command buffer's own work was done.
    Ended { count: usize },
    /// The driver reads no results on a lost device.
    Unreadable(String),
}

/// Fault `fault`'s line of command buffer `serial`'s [`Progress`].
pub fn progress(fault: u32, serial: u64, progress: &Progress) -> String {
    let opening = format!("{PREFIX}{fault} command buffer {serial}");
    match progress {
        Progress::NotStarted => {
            format!("{opening}: not started, no timestamp written (the GPU ran work before it)")
        }
        Progress::Stopped {
            index,
            count,
            shape,
            pipelines,
        } => {
            let mut line =
                format!("{opening}: span {index} of {count} started and never ended {shape}");
            if pipelines.is_empty() {
                line.push_str(", binding no pipeline");
            } else {
                let _ = write!(line, ", binding {}", pipelines.join(", "));
            }
            line
        }
        Progress::Ended { count } => {
            format!("{opening}: all {count} spans ended (its own work finished)")
        }
        Progress::Unreadable(why) => {
            format!("{opening}: its timestamps are unreadable on the lost device: {why}")
        }
    }
}

/// A pipeline whose object is gone, so its shaders cannot be named.
pub fn released(fault: u32, pipeline: &str) -> String {
    format!("{PREFIX}{fault} {pipeline}: released, its shaders unknown")
}

/// A Vulkan pipeline the driver reports no shaders of: a driver without
/// `VK_KHR_pipeline_executable_properties`, or NVK without patch 0043.
pub fn no_shaders(fault: u32, pipeline: &str, variant: usize, functions: &str) -> String {
    format!(
        "{PREFIX}{fault} {pipeline} variant {variant} ({functions}): the driver reports no shaders"
    )
}

/// The variants of a pipeline past [`MAX_VARIANTS`].
pub fn more_variants(fault: u32, pipeline: &str, more: usize) -> String {
    format!("{PREFIX}{fault} {pipeline}: {more} more variants not shown")
}

/// The words of a program header in hex, or `none`.
fn header_text(header: &[u32]) -> String {
    if header.is_empty() {
        return String::from("none");
    }
    let mut text = String::with_capacity(header.len() * 9);
    for (index, word) in header.iter().enumerate() {
        if index > 0 {
            text.push(' ');
        }
        let _ = write!(text, "{word:08x}");
    }
    text
}

/// The SHA-256 of `bytes` in hex.
fn sha256(bytes: &[u8]) -> String {
    use stela_crypto::{Sha256, digest::Digest};
    let mut text = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        let _ = write!(text, "{byte:02x}");
    }
    text
}

/// One stage's line: the pipeline, its functions, the stage, its code's
/// size and hash, the statistics a fault turns on and the program header.
pub fn shader(
    fault: u32,
    pipeline: &str,
    variant: usize,
    functions: &str,
    image: &ShaderImage,
) -> String {
    let mut line = format!(
        "{PREFIX}{fault} {pipeline} variant {variant} {} ({functions}): code={} sha256={}",
        image.stage,
        image.code.len(),
        sha256(&image.code)
    );
    for (name, word) in STATISTICS {
        if let Some(value) = image.statistic(name) {
            if name == "Program header address" {
                let _ = write!(line, " {word}={value:#x}");
            } else {
                let _ = write!(line, " {word}={value}");
            }
        }
    }
    if let Some(queued) = image.statistic(UPLOADED_BY_QUEUE) {
        line.push_str(if queued != 0 {
            " upload=queue"
        } else {
            " upload=mapped"
        });
    }
    let _ = write!(line, " sph={}", header_text(&image.header));
    if !image.geometry_header.is_empty() {
        let _ = write!(line, " gs_sph={}", header_text(&image.geometry_header));
    }
    line
}

/// The bytes of code a dump has left to give in hex, and those it had to
/// leave out.
#[derive(Debug)]
pub struct HexBudget {
    left: usize,
    unshown: usize,
}

impl HexBudget {
    /// A budget of `bytes`; none for a fault after the boot's first.
    pub fn new(bytes: usize) -> Self {
        Self {
            left: bytes,
            unshown: 0,
        }
    }

    /// `image`'s code in hex, [`HEX_LINE_BYTES`] to a line, as far as the
    /// budget goes; the rest is counted as unshown.
    pub fn code(
        &mut self,
        fault: u32,
        pipeline: &str,
        variant: usize,
        image: &ShaderImage,
    ) -> Vec<String> {
        let shown = image.code.len().min(self.left);
        self.left -= shown;
        self.unshown += image.code.len() - shown;
        image.code[..shown]
            .chunks(HEX_LINE_BYTES)
            .enumerate()
            .map(|(index, bytes)| {
                let mut line = format!(
                    "{PREFIX}{fault} {pipeline} variant {variant} {} code+{:#06x}: ",
                    image.stage,
                    index * HEX_LINE_BYTES
                );
                for byte in bytes {
                    let _ = write!(line, "{byte:02x}");
                }
                line
            })
            .collect()
    }

    /// The line that says how much code a dump with a budget left out.
    pub fn end(&self, fault: u32, budget: usize) -> Option<String> {
        (budget > 0 && self.unshown > 0).then(|| {
            format!(
                "{PREFIX}{fault} hex: {} bytes of code past the {budget}-byte budget not shown",
                self.unshown
            )
        })
    }
}

#[cfg(test)]
mod tests;
