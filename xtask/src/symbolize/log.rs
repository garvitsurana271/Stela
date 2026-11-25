//! The `PES PC` lines of a boot log (`step_timing::PcSamples::lines`): each
//! beat's group and, every 16 beats, a `PES PC TOTAL` group over every beat
//! since the first. A group is a header line, then a line per level with
//! samples that lists its blocks with the most cycles.
//!
//! A `--profile` window mark (`PROFILE MARK`) adds, for the window it ends,
//! a `PES PC WINDOW` group of the window's blocks and a `PES PC PAGES`
//! group of every page its samples fell in, each page as a block at offset
//! 0 (`stela_boot_core::profile_window::PcPages`), a level's pages over as
//! many level lines as it takes; a header's `w=` names the window. The
//! open window's pages are said every 32 beats too, so the last pages
//! group of each window is its whole. `PES PC SHARE` lines are no group's.
use anyhow::{Context, Result, bail};
use std::collections::{BTreeMap, HashSet};
use stela_engine::machine::engine::step_timing::{Level, pc_samples::level_name};

/// Which group a `PES PC` line belongs to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Kind {
    #[default]
    Beat,
    Total,
    Window,
    Pages,
}

/// What [`Profile::parse_with`] reads.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Select {
    /// Every beat group, not the last total.
    pub beats: bool,
    /// The pages groups: the last of each window, or of `window` alone.
    pub pages: bool,
    /// One window: its last `PES PC WINDOW` group, or with `pages` its
    /// pages.
    pub window: Option<u32>,
}

/// The levels in the order the lines print them.
pub(super) const LEVELS: [Level; 3] = [Level::User, Level::Kernel, Level::Guarded];

/// One block a level line lists: its PC as printed (`0x...`, `v+0x...`,
/// `v-0x...`), its page key's first 8 bytes and its offset there, and its
/// samples and host cycles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Entry {
    pub level: Level,
    pub pc: String,
    pub page: Option<u64>,
    pub offset: u16,
    pub samples: u64,
    pub cycles: u64,
}

/// One group's header and level lines.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Group {
    kind: Kind,
    /// A window's or pages group's `w=`.
    window: Option<u32>,
    samples: u64,
    cycles: u64,
    gone: [u64; 2],
    other: [u64; 2],
    /// Samples and cycles by level.
    levels: [[u64; 2]; 3],
    entries: Vec<Entry>,
}

/// Which lines a [`Profile`] sums.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Source {
    /// The `taken`th of `of` total groups, the last.
    Total { taken: usize, of: usize },
    /// Every beat group, `of` of them.
    Beats { of: usize },
    /// Window `window`'s last `PES PC WINDOW` group.
    Window { window: u32 },
    /// The last `PES PC PAGES` group of each of `windows` windows: every
    /// page's samples, not blocks.
    Pages { windows: usize },
}

/// The samples a log's lines hold: the counts and the listed blocks, each
/// once, most cycles first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Profile {
    pub source: Source,
    pub samples: u64,
    pub cycles: u64,
    pub gone: [u64; 2],
    pub other: [u64; 2],
    pub levels: [[u64; 2]; 3],
    pub entries: Vec<Entry>,
    /// The `PES PC` lines skipped, each with its line number and why.
    pub warnings: Vec<String>,
}

/// An entry's key while groups are summed: level, page, offset, PC.
type Key = (usize, Option<u64>, u16, String);

impl Profile {
    /// The last total group of `text`, or with `beats` (or without a
    /// total) the sum of its beat groups.
    #[cfg(test)]
    pub(super) fn parse(text: &str, beats: bool) -> Result<Self> {
        Self::parse_with(
            text,
            Select {
                beats,
                ..Select::default()
            },
        )
    }

    /// The groups `select` names: the last total, the beats, one window's
    /// blocks, or the pages groups.
    pub(super) fn parse_with(text: &str, select: Select) -> Result<Self> {
        let (groups, warnings) = groups(text);
        let of = |kind: Kind| -> Vec<&Group> {
            groups.iter().filter(|group| group.kind == kind).collect()
        };
        let totals = of(Kind::Total);
        let beat_groups = of(Kind::Beat);
        let (source, chosen) = match (select.pages, select.window) {
            (true, window) => {
                // The last pages group of each window: each is its window
                // so far.
                let mut last: BTreeMap<u32, &Group> = BTreeMap::new();
                for group in of(Kind::Pages) {
                    let at = group.window.unwrap_or(0);
                    if window.is_none_or(|window| window == at) {
                        last.insert(at, group);
                    }
                }
                if last.is_empty() {
                    bail!(
                        "the log has no `PES PC PAGES` lines{}: boot a build whose \
                         `--profile` says them",
                        window.map_or(String::new(), |window| format!(" of window {window}"))
                    );
                }
                (
                    Source::Pages {
                        windows: last.len(),
                    },
                    last.into_values().collect(),
                )
            }
            (false, Some(window)) => {
                let Some(group) = of(Kind::Window)
                    .into_iter()
                    .rfind(|group| group.window == Some(window))
                else {
                    bail!("the log has no `PES PC WINDOW` lines of window {window}");
                };
                (Source::Window { window }, vec![group])
            }
            (false, None) => match totals.last() {
                Some(&last) if !select.beats => (
                    Source::Total {
                        taken: totals.len(),
                        of: totals.len(),
                    },
                    vec![last],
                ),
                _ if !beat_groups.is_empty() => (
                    Source::Beats {
                        of: beat_groups.len(),
                    },
                    beat_groups,
                ),
                _ => bail!(
                    "the log has no `PES PC` lines: boot with `--profile` on a host whose \
                     counters sample (its log has `PES WHERE` lines)"
                ),
            },
        };
        let mut profile = Profile {
            source,
            samples: 0,
            cycles: 0,
            gone: [0; 2],
            other: [0; 2],
            levels: [[0; 2]; 3],
            entries: Vec::new(),
            warnings,
        };
        let mut entries: BTreeMap<Key, (u64, u64)> = BTreeMap::new();
        for group in chosen {
            profile.samples += group.samples;
            profile.cycles += group.cycles;
            for index in 0..2 {
                profile.gone[index] += group.gone[index];
                profile.other[index] += group.other[index];
            }
            for (level, counts) in profile.levels.iter_mut().enumerate() {
                counts[0] += group.levels[level][0];
                counts[1] += group.levels[level][1];
            }
            for entry in &group.entries {
                let key = (
                    level_index(entry.level),
                    entry.page,
                    entry.offset,
                    entry.pc.clone(),
                );
                let counts = entries.entry(key).or_default();
                counts.0 += entry.samples;
                counts.1 += entry.cycles;
            }
        }
        profile.entries = entries
            .into_iter()
            .map(|((level, page, offset, pc), (samples, cycles))| Entry {
                level: LEVELS[level],
                pc,
                page,
                offset,
                samples,
                cycles,
            })
            .collect();
        // Stable: equal entries keep the key's order, level first.
        profile
            .entries
            .sort_by(|a, b| b.cycles.cmp(&a.cycles).then(b.samples.cmp(&a.samples)));
        Ok(profile)
    }

    /// The page keys of the listed blocks.
    pub(super) fn pages(&self) -> HashSet<u64> {
        self.entries.iter().filter_map(|entry| entry.page).collect()
    }
}

/// A level's index in [`LEVELS`].
pub(super) fn level_index(level: Level) -> usize {
    LEVELS.iter().position(|&l| l == level).unwrap_or(0)
}

/// The `PES PC` groups of `text`, in order, and a warning for each line
/// skipped: a level line without its group's header, as a log that starts
/// or is cut mid-group has, or a broken line.
fn groups(text: &str) -> (Vec<Group>, Vec<String>) {
    const PREFIX: &str = "PES PC ";
    let mut groups: Vec<Group> = Vec::new();
    let mut warnings = Vec::new();
    for (number, line) in text.lines().enumerate() {
        let Some(at) = line.find(PREFIX) else {
            continue;
        };
        let rest = &line[at + PREFIX.len()..];
        if rest.starts_with("SHARE ") {
            continue;
        }
        let (kind, rest) = [
            ("TOTAL ", Kind::Total),
            ("WINDOW ", Kind::Window),
            ("PAGES ", Kind::Pages),
        ]
        .into_iter()
        .find_map(|(prefix, kind)| rest.strip_prefix(prefix).map(|rest| (kind, rest)))
        .unwrap_or((Kind::Beat, rest));
        if let Err(error) = group_line(&mut groups, kind, rest) {
            warnings.push(format!("line {}: skipped, {error:#}", number + 1));
        }
    }
    (groups, warnings)
}

/// Add one line's words, `rest` after its prefix, to `groups`: a header
/// starts a group, a level line fills the last.
fn group_line(groups: &mut Vec<Group>, kind: Kind, rest: &str) -> Result<()> {
    let mut words = rest.split_ascii_whitespace();
    let first = words.next().context("an empty line")?;
    if first.starts_with("samples=") {
        let mut group = Group {
            kind,
            ..Group::default()
        };
        for word in std::iter::once(first).chain(words) {
            let (name, value) = word.split_once('=').context("a `name=value` word")?;
            match name {
                "samples" => group.samples = number(value)?,
                "cycles" => group.cycles = number(value)?,
                "gone" => group.gone = pair(value)?,
                "other" => group.other = pair(value)?,
                "w" => {
                    group.window = Some(
                        value
                            .parse()
                            .with_context(|| format!("a window `{value}`"))?,
                    )
                }
                _ => {}
            }
        }
        groups.push(group);
        return Ok(());
    }
    let level = LEVELS
        .into_iter()
        .find(|&level| level_name(level) == first)
        .with_context(|| format!("an unknown level `{first}`"))?;
    let group = groups
        .last_mut()
        .filter(|group| group.kind == kind)
        .context("a level line before its group's header")?;
    // The whole line first: a broken one leaves its group as it was.
    let mut counts = [0; 2];
    let mut entries = Vec::new();
    for word in words {
        if word.contains('@') {
            entries.push(entry(level, word)?);
            continue;
        }
        let (name, value) = word.split_once('=').context("a `name=value` word")?;
        match name {
            "samples" => counts[0] = number(value)?,
            "cycles" => counts[1] = number(value)?,
            _ => {}
        }
    }
    group.levels[level_index(level)] = counts;
    group.entries.extend(entries);
    Ok(())
}

/// A block word, `{pc}:{samples}:{cycles}@{page}+{offset}`.
fn entry(level: Level, word: &str) -> Result<Entry> {
    let (counts, place) = word.split_once('@').context("a block's `@`")?;
    let (page, offset) = place.split_once('+').context("a block's `+offset`")?;
    let mut fields = counts.rsplitn(3, ':');
    let (Some(cycles), Some(samples), Some(pc)) = (fields.next(), fields.next(), fields.next())
    else {
        bail!("a block's `pc:samples:cycles` in `{word}`");
    };
    let page = match page {
        "?" => None,
        page => Some(u64::from_str_radix(page, 16).with_context(|| format!("a page `{page}`"))?),
    };
    Ok(Entry {
        level,
        pc: pc.into(),
        page,
        offset: u16::from_str_radix(offset, 16).with_context(|| format!("an offset `{offset}`"))?,
        samples: number(samples)?,
        cycles: number(cycles)?,
    })
}

fn number(text: &str) -> Result<u64> {
    text.parse().with_context(|| format!("a number `{text}`"))
}

/// `N/C`: samples and their cycles.
fn pair(text: &str) -> Result<[u64; 2]> {
    let (samples, cycles) = text.split_once('/').context("a `samples/cycles` pair")?;
    Ok([number(samples)?, number(cycles)?])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cycles in the lines' units, whole cycles: a million.
    const M: u64 = 1_000_000;

    const LOG: &str = "\
[   10.000] PES WHERE all: ins=1M cyc=2M
[   20.000] PES PC samples=3 cycles=50000000 gone=1/16000000 other=0/0
[   20.000] PES PC user samples=2 cycles=33000000 blocks=2 0x18a2b3c40:1:17000000@1f2e3d4c5b6a7980+c40 0x100:1:16000000@?+100
[   20.000] PES PC kernel samples=1 cycles=16000000 blocks=1 v+0x2c4d0:1:16000000@00000000000000ab+4d0
[   30.000] PES PC samples=2 cycles=34000000 gone=0/0 other=1/1000000
[   30.000] PES PC user samples=2 cycles=34000000 blocks=1 0x18a2b3c40:2:34000000@1f2e3d4c5b6a7980+c40
[   30.000] PES PC TOTAL samples=5 cycles=84000000 gone=1/16000000 other=1/1000000
[   30.000] PES PC TOTAL user samples=4 cycles=67000000 blocks=2 0x18a2b3c40:3:51000000@1f2e3d4c5b6a7980+c40 0x100:1:16000000@?+100
[   30.000] PES PC TOTAL kernel samples=1 cycles=16000000 blocks=1 v+0x2c4d0:1:16000000@00000000000000ab+4d0
[   40.000] PES PC samples=1 cycles=16000000 gone=0/0 other=0/0
[   40.000] PES PC guarded samples=1 cycles=16000000 blocks=1 v-0x40:1:16000000@00000000000000cd+fc0
";

    fn entry(
        level: Level,
        pc: &str,
        page: Option<u64>,
        offset: u16,
        samples: u64,
        cycles: u64,
    ) -> Entry {
        Entry {
            level,
            pc: pc.into(),
            page,
            offset,
            samples,
            cycles,
        }
    }

    #[test]
    fn the_last_total_is_read_by_default() {
        let profile = Profile::parse(LOG, false).unwrap();
        assert_eq!(profile.source, Source::Total { taken: 1, of: 1 });
        assert_eq!((profile.samples, profile.cycles), (5, 84 * M));
        assert_eq!(profile.gone, [1, 16 * M]);
        assert_eq!(profile.other, [1, M]);
        assert_eq!(profile.levels, [[4, 67 * M], [1, 16 * M], [0, 0]]);
        assert_eq!(
            profile.entries,
            [
                entry(
                    Level::User,
                    "0x18a2b3c40",
                    Some(0x1f2e_3d4c_5b6a_7980),
                    0xc40,
                    3,
                    51 * M
                ),
                entry(Level::User, "0x100", None, 0x100, 1, 16 * M),
                entry(Level::Kernel, "v+0x2c4d0", Some(0xab), 0x4d0, 1, 16 * M),
            ]
        );
        assert_eq!(
            profile.pages(),
            HashSet::from([0x1f2e_3d4c_5b6a_7980, 0xab])
        );
    }

    #[test]
    fn beats_sum_every_beat_group() {
        let profile = Profile::parse(LOG, true).unwrap();
        assert_eq!(profile.source, Source::Beats { of: 3 });
        assert_eq!((profile.samples, profile.cycles), (6, 100 * M));
        assert_eq!(profile.gone, [1, 16 * M]);
        assert_eq!(profile.other, [1, M]);
        assert_eq!(profile.levels, [[4, 67 * M], [1, 16 * M], [1, 16 * M]]);
        assert_eq!(
            profile.entries,
            [
                entry(
                    Level::User,
                    "0x18a2b3c40",
                    Some(0x1f2e_3d4c_5b6a_7980),
                    0xc40,
                    3,
                    51 * M
                ),
                entry(Level::User, "0x100", None, 0x100, 1, 16 * M),
                entry(Level::Kernel, "v+0x2c4d0", Some(0xab), 0x4d0, 1, 16 * M),
                entry(Level::Guarded, "v-0x40", Some(0xcd), 0xfc0, 1, 16 * M),
            ]
        );
    }

    #[test]
    fn a_log_without_a_total_sums_its_beats() {
        let beats: String = LOG
            .lines()
            .filter(|line| !line.contains("TOTAL"))
            .map(|line| format!("{line}\n"))
            .collect();
        let profile = Profile::parse(&beats, false).unwrap();
        assert_eq!(profile.source, Source::Beats { of: 3 });
        assert_eq!(profile.cycles, 100 * M);
    }

    #[test]
    fn a_log_without_lines_is_refused() {
        let none = Profile::parse("PES WHERE all: ins=1M\nPES HOT user samples=3\n", false);
        assert!(none.unwrap_err().to_string().contains("no `PES PC` lines"));
        // Nor does a log of level lines alone hold a group.
        let orphan = "PES PC user samples=1 cycles=16000000 blocks=1 0x100:1:16000000@?+100\n";
        assert!(Profile::parse(orphan, false).is_err());
    }

    /// A level line without its group's header (a log that starts
    /// mid-group: a wrapped ring, a dropped line, a cut) and a broken line
    /// are skipped with a warning naming them; the rest reads as before.
    #[test]
    fn an_orphan_or_broken_line_is_skipped_with_a_warning() {
        let log = format!(
            "PES PC user samples=1 cycles=7 blocks=1 0x200:1:7@?+200\n{LOG}\
             PES PC TOTAL user samples=1 cycles=7 blocks=1 0x300:1:7@?+300\n\
             PES PC samples=0 cycles=0 gone=0/0 other=0/0\n\
             PES PC user samples=1 cycles=16000000 blocks=1 0x100:1@?+100\n"
        );
        let beats = Profile::parse(&log, true).unwrap();
        assert_eq!(beats.source, Source::Beats { of: 4 });
        let clean = Profile::parse(LOG, true).unwrap();
        assert_eq!(beats.entries, clean.entries);
        assert_eq!(beats.cycles, clean.cycles);
        assert_eq!(beats.warnings.len(), 3, "{:?}", beats.warnings);
        assert!(
            beats.warnings[0].starts_with("line 1: "),
            "{:?}",
            beats.warnings
        );
        assert!(beats.warnings[0].contains("header"), "{:?}", beats.warnings);
        // A total's level line after a beat's group is no part of it.
        assert!(
            beats.warnings[1].starts_with("line 13: "),
            "{:?}",
            beats.warnings
        );
        assert!(
            beats.warnings[2].starts_with("line 15: "),
            "{:?}",
            beats.warnings
        );
        assert!(clean.warnings.is_empty());
        // The total reads as before too.
        let total = Profile::parse(&log, false).unwrap();
        assert_eq!(total.entries, Profile::parse(LOG, false).unwrap().entries);
    }

    /// A marked boot: window 0's pages twice (the periodic lines, then the
    /// mark's), its blocks, window 1's pages; share lines among them.
    const MARKED: &str = "\
[   10.000] PES PC samples=1 cycles=16000000 gone=0/0 other=0/0
[   10.000] PES PC user samples=1 cycles=16000000 blocks=1 0x100:1:16000000@?+100
[   20.000] PES PC PAGES samples=1 cycles=7 gone=0/0 other=0/0 w=0 pages=1
[   20.000] PES PC PAGES user samples=1 cycles=7 pages=1 part=1/1 0x180000000:1:7@00000000000000aa+0
[   25.000] PES PC SHARE user w=0 samples=3 cycles=9 blocks=2 exact=yes evicted=0 floor=0 top24=1000
[   25.000] PES PC PAGES samples=3 cycles=9 gone=1/1 other=0/0 w=0 pages=2
[   25.000] PES PC PAGES user samples=3 cycles=9 pages=2 part=1/2 0x180000000:2:8@00000000000000aa+0
[   25.000] PES PC PAGES user samples=3 cycles=9 pages=2 part=2/2 0x180004000:1:1@00000000000000bb+0
[   25.000] PES PC WINDOW samples=3 cycles=9 gone=1/1 other=0/0 w=0
[   25.000] PES PC WINDOW user samples=3 cycles=9 blocks=2 0x180000040:2:8@00000000000000aa+40 0x180004000:1:1@00000000000000bb+0
[   40.000] PES PC PAGES samples=2 cycles=20 gone=0/0 other=0/0 w=1 pages=1
[   40.000] PES PC PAGES kernel samples=2 cycles=20 pages=1 part=1/1 v+0x4000:2:20@00000000000000cc+0
";

    #[test]
    fn pages_read_the_last_group_of_each_window() {
        let select = |pages: bool, window: Option<u32>| Select {
            beats: false,
            pages,
            window,
        };
        let all = Profile::parse_with(MARKED, select(true, None)).unwrap();
        assert_eq!(all.source, Source::Pages { windows: 2 });
        assert_eq!((all.samples, all.cycles), (5, 29));
        assert_eq!(all.gone, [1, 1]);
        assert_eq!(all.levels, [[3, 9], [2, 20], [0, 0]]);
        assert_eq!(all.entries.len(), 3);
        assert_eq!(
            all.entries[0],
            entry(Level::Kernel, "v+0x4000", Some(0xcc), 0, 2, 20)
        );
        assert!(all.warnings.is_empty(), "{:?}", all.warnings);
        let first = Profile::parse_with(MARKED, select(true, Some(0))).unwrap();
        assert_eq!(first.source, Source::Pages { windows: 1 });
        assert_eq!(first.cycles, 9);
        assert!(Profile::parse_with(MARKED, select(true, Some(5))).is_err());
        // One window's blocks.
        let blocks = Profile::parse_with(MARKED, select(false, Some(0))).unwrap();
        assert_eq!(blocks.source, Source::Window { window: 0 });
        assert_eq!(blocks.entries[0].offset, 0x40);
        assert!(Profile::parse_with(MARKED, select(false, Some(1))).is_err());
        // The default still reads the beats, the other groups apart.
        let beats = Profile::parse(MARKED, false).unwrap();
        assert_eq!(beats.source, Source::Beats { of: 1 });
        assert_eq!(beats.cycles, 16_000_000);
    }

    /// The lines the engine prints parse back into the blocks it held.
    #[test]
    fn the_engines_lines_parse_back() {
        use stela_engine::machine::engine::step_timing::{PcHit, PcSamples};
        let mut samples = PcSamples::default();
        let hits = [
            (
                0x1_8a2b_3c40u64,
                Level::User,
                Some(0x1f2e_3d4c_5b6a_7980u64),
                0xc40u16,
                33_554_432u64,
            ),
            (
                0x1_8a2b_3c40,
                Level::User,
                Some(0x1f2e_3d4c_5b6a_7980),
                0xc40,
                16_777_216,
            ),
            (0x2c4d0, Level::Kernel, None, 0x4d0, 20_000_000),
            (
                0u64.wrapping_sub(0x40),
                Level::Guarded,
                Some(7),
                0xfc0,
                16_777_216,
            ),
        ];
        for (pc, level, page, offset, cycles) in hits {
            samples.add(PcHit {
                pc,
                level,
                page,
                offset,
                cycles,
            });
        }
        samples.add_gone(1, 16_777_216);
        let text: String = samples
            .lines("PES PC TOTAL", 8)
            .iter()
            .map(|line| format!("[   1.000] {line}\n"))
            .collect();
        let profile = Profile::parse(&text, false).unwrap();
        assert_eq!(profile.source, Source::Total { taken: 1, of: 1 });
        assert_eq!(profile.samples, 4);
        // Exact cycles: nothing is lost to whole millions.
        assert_eq!(profile.cycles, 87_108_864);
        assert_eq!(profile.gone, [1, 16_777_216]);
        assert_eq!(
            profile.entries,
            [
                entry(
                    Level::User,
                    "0x18a2b3c40",
                    Some(0x1f2e_3d4c_5b6a_7980),
                    0xc40,
                    2,
                    50_331_648
                ),
                entry(Level::Kernel, "v+0x2c4d0", None, 0x4d0, 1, 20_000_000),
                entry(Level::Guarded, "v-0x40", Some(7), 0xfc0, 1, 16_777_216),
            ]
        );
    }
}
