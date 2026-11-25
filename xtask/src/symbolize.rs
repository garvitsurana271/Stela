//! `cargo xtask symbolize`: name the functions `--profile`'s `PES PC`
//! lines sample. Each line lists guest blocks by where native code spent
//! its host cycles, each with its code page's identity: the first 8 bytes
//! of the SHA-256 of its 4 KiB page and its offset there
//! (`step_timing::pc_samples`). Hashing every executable page of the
//! guest's own dyld shared cache (or of a kernel or any Mach-O image) the
//! same way finds each block's unslid address without knowing the slide,
//! and the images' symbols and function starts name its function. The
//! table ranks functions by their share of the sampled cycles.
use anyhow::{Context, Result};
use clap::Args;
use std::path::PathBuf;

pub(crate) mod code;
#[cfg(test)]
mod fixtures;
mod log;

#[derive(Args)]
pub struct Symbolize {
    /// The boot log, as `stela log --output log.txt` saves it.
    log: PathBuf,
    /// The guest's dyld shared cache (its main file, with its subcaches
    /// and `.symbols` beside it, or the directory holding them), a Mach-O
    /// image such as a KDK kernel, or a directory of them. Repeat for
    /// more; without one, blocks are listed by page.
    #[arg(long)]
    cache: Vec<PathBuf>,
    /// Sum every beat's `PES PC` lines, for a log cut to one workload,
    /// instead of the last `PES PC TOTAL` (every beat since the boot's
    /// first).
    #[arg(long)]
    beats: bool,
    /// Every page the samples fell in (the `PES PC PAGES` lines a
    /// `--profile` boot says per window and every 32 beats), not the
    /// listed blocks: all of the named samples by image and level. With
    /// `--window`, that window's.
    #[arg(long)]
    pages: bool,
    /// One marked window (`PROFILE MARK`): its `PES PC WINDOW` blocks, or
    /// with `--pages` its pages. Window 0 is the boot up to the first mark.
    #[arg(long)]
    window: Option<u32>,
    /// The functions the table shows.
    #[arg(long, default_value_t = 50)]
    top: usize,
}

impl Symbolize {
    pub fn run(&self) -> Result<()> {
        let bytes =
            std::fs::read(&self.log).with_context(|| format!("reading {}", self.log.display()))?;
        let profile = log::Profile::parse_with(
            &String::from_utf8_lossy(&bytes),
            log::Select {
                beats: self.beats,
                pages: self.pages,
                window: self.window,
            },
        )?;
        let code = code::Code::load(&self.cache, &profile.pages())?;
        print!("{}", report(&profile, &code, self.top));
        Ok(())
    }
}

/// The share of the cycles in `other`, in percent, that the report warns
/// of.
const OTHER_WARNING: f64 = 10.0;

/// One row of the table: a function's samples, or a block's no image
/// names.
#[derive(Default)]
struct Row {
    samples: u64,
    cycles: u64,
    blocks: u64,
}

/// The ranked table of `profile`'s functions named in `code`, the `top`
/// first, then the images. Shares are of the sampled cycles of native
/// code, the blocks the lines did not list included.
fn report(profile: &log::Profile, code: &code::Code, top: usize) -> String {
    use code::Place;
    use std::collections::BTreeMap;
    use std::fmt::Write;
    use stela_engine::machine::engine::step_timing::pc_samples::level_name;
    let mut out = String::new();
    let share = |cycles: u64| cycles as f64 * 100.0 / profile.cycles.max(1) as f64;
    let source = match profile.source {
        log::Source::Total { taken, of } => format!("the last PES PC TOTAL ({taken} of {of})"),
        log::Source::Beats { of } => format!("{of} beats of PES PC"),
        log::Source::Window { window } => format!("window {window}'s PES PC WINDOW"),
        log::Source::Pages { windows } => {
            format!("the PES PC PAGES of {windows} window(s), every page")
        }
    };
    let listed: u64 = profile.entries.iter().map(|entry| entry.cycles).sum();
    let _ = writeln!(
        out,
        "{source}: {} samples, {} cycles of native code; gone {} ({}), other {} ({}); \
         listed blocks hold {:.1}%",
        profile.samples,
        millions(profile.cycles),
        profile.gone[0],
        millions(profile.gone[1]),
        profile.other[0],
        millions(profile.other[1]),
        share(listed),
    );
    let levels: Vec<String> = log::LEVELS
        .iter()
        .map(|&level| {
            let cycles = profile.levels[log::level_index(level)][1];
            format!("{} {:.1}%", level_name(level), share(cycles))
        })
        .collect();
    let _ = writeln!(out, "levels: {}", levels.join(", "));
    for warning in &profile.warnings {
        let _ = writeln!(out, "warning: {warning}");
    }
    // A total trims the blocks with the fewest cycles into `other`.
    let other = share(profile.other[1]);
    if other >= OTHER_WARNING {
        let advice = match profile.source {
            log::Source::Total { .. } => {
                "for one workload, cut the log to its beats and read them with --beats"
            }
            log::Source::Beats { .. } => "the beats' tables were full",
            log::Source::Window { .. } | log::Source::Pages { .. } => {
                "read the window's pages with --pages"
            }
        };
        let _ = writeln!(
            out,
            "warning: {other:.1}% of the cycles are of blocks the lines no longer list \
             (other): {advice}"
        );
    }
    for loaded in &code.loaded {
        let _ = writeln!(out, "read: {loaded}");
    }
    if code.loaded.is_empty() {
        let _ = writeln!(out, "read: no dyld shared cache or Mach-O image");
    }
    for warning in &code.warnings {
        let _ = writeln!(out, "warning: {warning}");
    }
    let pages = profile.pages();
    let placed = |wanted: Place| {
        pages
            .iter()
            .filter(|&&page| code.place(page, 0) == wanted)
            .count()
    };
    let ambiguous = placed(Place::Ambiguous);
    let found = pages.len() - placed(Place::Missing);
    let _ = writeln!(
        out,
        "pages: {found} of {} found, {ambiguous} ambiguous",
        pages.len()
    );
    // Rows by level, image and function; images by path.
    let mut rows: BTreeMap<(usize, String, String), Row> = BTreeMap::new();
    let mut images: BTreeMap<String, Row> = BTreeMap::new();
    for entry in &profile.entries {
        let level = log::level_index(entry.level);
        let page = entry.page.unwrap_or(0);
        let (image, function) = match entry.page.map(|page| code.place(page, entry.offset)) {
            None => ("?".into(), format!("{} (page not read)", entry.pc)),
            Some(Place::Missing) => (
                "?".into(),
                format!("{} ({page:016x}+{:x})", entry.pc, entry.offset),
            ),
            Some(Place::Ambiguous) => (
                "?".into(),
                format!("{} (ambiguous {page:016x}+{:x})", entry.pc, entry.offset),
            ),
            Some(Place::Found { file, address }) => {
                let named = code.name(file, address);
                (named.image.unwrap_or("?").to_owned(), named.function)
            }
        };
        for row in [
            rows.entry((level, image.clone(), function)).or_default(),
            images.entry(image).or_default(),
        ] {
            row.samples += entry.samples;
            row.cycles += entry.cycles;
            row.blocks += 1;
        }
    }
    // Pages are listed at offset 0: their functions are not the samples'.
    let pages_only = matches!(profile.source, log::Source::Pages { .. });
    let rows = if pages_only { BTreeMap::new() } else { rows };
    let mut ranked: Vec<_> = rows.into_iter().collect();
    // Stable: equal rows keep the key's order.
    ranked.sort_by(|a, b| {
        b.1.cycles
            .cmp(&a.1.cycles)
            .then(b.1.samples.cmp(&a.1.samples))
    });
    let _ = writeln!(
        out,
        "\n{:>7}  {:>9}  {:>7}  {:>6}  {:<7}  {:<40}  image",
        "share", "cycles", "samples", "blocks", "level", "function"
    );
    for ((level, image, function), row) in ranked.iter().take(top) {
        let short = image.rsplit('/').next().unwrap_or(image);
        let _ = writeln!(
            out,
            "{:>6.1}%  {:>9}  {:>7}  {:>6}  {:<7}  {function:<40}  {short}",
            share(row.cycles),
            millions(row.cycles),
            row.samples,
            row.blocks,
            level_name(log::LEVELS[*level]),
        );
    }
    if ranked.len() > top {
        let rest: u64 = ranked.iter().skip(top).map(|(_, row)| row.cycles).sum();
        let _ = writeln!(
            out,
            "{:>6.1}%  {:>9}  and {} more",
            share(rest),
            millions(rest),
            ranked.len() - top
        );
    }
    let mut images: Vec<_> = images.into_iter().collect();
    images.sort_by_key(|image| std::cmp::Reverse(image.1.cycles));
    let _ = writeln!(
        out,
        "\nby image:\n{:>7}  {:>9}  {:>6}  image",
        "share", "cycles", "blocks"
    );
    for (image, row) in images {
        let _ = writeln!(
            out,
            "{:>6.1}%  {:>9}  {:>6}  {image}",
            share(row.cycles),
            millions(row.cycles),
            row.blocks
        );
    }
    out
}

/// Host cycles in millions with one decimal, as the table prints them.
fn millions(cycles: u64) -> String {
    format!("{:.1}M", cycles as f64 / 1e6)
}

#[cfg(test)]
mod tests {
    use super::code::Code;
    use super::fixtures::{self, ALPHA, BASE_FOR_REPORT, PAGE, UNNAMED};
    use super::*;

    /// A total whose user blocks lie in the fixture image's two code
    /// pages, one block in a page no image holds, and one kernel block
    /// whose page was not read.
    fn log(file: &[u8]) -> String {
        let first = fixtures::key(&file[PAGE..2 * PAGE]);
        let second = fixtures::key(&file[2 * PAGE..3 * PAGE]);
        let alpha = BASE_FOR_REPORT + ALPHA;
        let unnamed = BASE_FOR_REPORT + UNNAMED;
        format!(
            "PES PC TOTAL samples=9 cycles=200000000 gone=1/16000000 other=1/10000000\n\
             PES PC TOTAL user samples=8 cycles=180000000 blocks=5 \
             {alpha:#x}:3:60000000@{first:016x}+0 {:#x}:2:40000000@{first:016x}+40 \
             {unnamed:#x}:1:30000000@{first:016x}+400 \
             {:#x}:1:20000000@{second:016x}+20 \
             0x100:1:10000000@00000000000000ab+100\n\
             PES PC TOTAL kernel samples=1 cycles=20000000 blocks=1 \
             v+0x2c4d0:1:20000000@?+4d0\n",
            alpha + 0x40,
            BASE_FOR_REPORT + 0x2020,
        )
    }

    #[test]
    fn functions_rank_by_their_share_of_the_cycles() {
        let file = fixtures::macho(BASE_FOR_REPORT);
        let profile = log::Profile::parse(&log(&file), false).unwrap();
        let mut code = Code::default();
        code.add_macho(&file, "/usr/lib/libfixture.dylib", &profile.pages())
            .unwrap();
        let table = report(&profile, &code, 10);
        let rows: Vec<&str> = table
            .lines()
            .filter(|line| line.contains('%') && !line.contains("share"))
            .collect();
        // The header counts every sample; the rows rank the functions.
        assert!(
            table.contains("the last PES PC TOTAL (1 of 1): 9 samples, 200.0M cycles"),
            "{table}"
        );
        assert!(table.contains("gone 1 (16.0M)"), "{table}");
        // Cycles in millions with a decimal: _alpha's 100M.
        assert!(table.contains(" 100.0M "), "{table}");
        assert!(table.contains("listed blocks hold 90.0%"), "{table}");
        assert!(
            table.contains("pages: 2 of 3 found, 0 ambiguous"),
            "{table}"
        );
        let first = |name: &str| rows.iter().position(|row| row.contains(name));
        let alpha = first("_alpha").expect("_alpha");
        let unnamed = first(&format!("sub_{:x}", BASE_FOR_REPORT + UNNAMED)).expect("sub_");
        let beta = first("_beta").expect("_beta");
        assert!(alpha < unnamed && unnamed < beta, "{table}");
        // _alpha's two blocks are one function: 100M of 200M.
        assert!(rows[alpha].contains("50.0%"), "{}", rows[alpha]);
        assert!(rows[alpha].contains("libfixture.dylib"), "{}", rows[alpha]);
        assert!(rows[alpha].contains(" 2 "), "two blocks: {}", rows[alpha]);
        // A page no image holds and a page not read are listed by block.
        assert!(table.contains("0x100"), "{table}");
        assert!(table.contains("v+0x2c4d0"), "{table}");
        assert!(table.contains("page not read"), "{table}");
        // By image.
        assert!(table.contains("by image"), "{table}");
        // `top` bounds the function rows.
        let short = report(&profile, &code, 1);
        assert!(
            short.contains("_alpha") && !short.contains("_beta"),
            "{short}"
        );
    }

    /// A total whose blocks are mostly folded into `other` says so, and
    /// how to read one workload instead; a small `other` is not warned of.
    #[test]
    fn a_large_other_is_warned_of() {
        let text = |other: u64| {
            format!(
                "PES PC TOTAL samples=4 cycles=100 gone=0/0 other=3/{other}\n\
                 PES PC TOTAL user samples=4 cycles=100 blocks=1 0x200:1:{}@?+200\n",
                100 - other
            )
        };
        let large = log::Profile::parse(&text(60), false).unwrap();
        let table = report(&large, &Code::default(), 10);
        assert!(table.contains("warning: 60.0% of the cycles"), "{table}");
        assert!(table.contains("--beats"), "{table}");
        let small = log::Profile::parse(&text(5), false).unwrap();
        let table = report(&small, &Code::default(), 10);
        assert!(!table.contains("warning:"), "{table}");
    }

    /// The lines the log's reading skipped are named.
    #[test]
    fn skipped_lines_are_reported() {
        let text = "PES PC user samples=1 cycles=7 blocks=1 0x200:1:7@?+200\n\
                    PES PC samples=1 cycles=7 gone=0/0 other=0/0\n\
                    PES PC user samples=1 cycles=7 blocks=1 0x200:1:7@?+200\n";
        let profile = log::Profile::parse(text, false).unwrap();
        let table = report(&profile, &Code::default(), 10);
        assert!(table.contains("warning: line 1: "), "{table}");
        assert!(table.contains("0x200"), "{table}");
    }

    /// What the loading skipped is reported.
    #[test]
    fn skipped_files_are_reported() {
        let text = "PES PC samples=1 cycles=7 gone=0/0 other=0/0\n\
                    PES PC user samples=1 cycles=7 blocks=1 0x200:1:7@?+200\n";
        let profile = log::Profile::parse(text, false).unwrap();
        let mut code = Code::default();
        code.warnings.push("a.dylib: skipped, broken".into());
        let table = report(&profile, &code, 10);
        assert!(
            table.contains("warning: a.dylib: skipped, broken"),
            "{table}"
        );
    }

    /// Without a cache, blocks are listed by page.
    #[test]
    fn without_a_cache_blocks_are_listed_by_page() {
        let file = fixtures::macho(BASE_FOR_REPORT);
        let profile = log::Profile::parse(&log(&file), false).unwrap();
        let table = report(&profile, &Code::default(), 10);
        let first = fixtures::key(&file[PAGE..2 * PAGE]);
        assert!(table.contains(&format!("{first:016x}+0")), "{table}");
        assert!(table.contains("pages: 0 of 3 found"), "{table}");
        assert!(
            table.contains("read: no dyld shared cache or Mach-O image"),
            "{table}"
        );
    }
}
