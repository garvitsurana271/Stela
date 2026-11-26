//! What the ahead-of-time translator could take from a set of images, as
//! counts: functions and their sizes, which fit one translation unit (one
//! page, at most `MAX_BLOCK_INSTRUCTIONS` words, the contract a region and
//! every stored block keeps), calls and returns, the registers functions
//! use outside the pinned set, and dead flag setters. With PC samples
//! (`--profile`'s hot blocks, `pc weight ...` lines), each count is also
//! weighted by where the samples fall.
use crate::a64::Flow;
use crate::cfg::{Boundary, Function, PINNED};
use crate::image::Image;
use std::fmt::Write as _;

/// The most words one unit takes (`block::MAX_BLOCK_INSTRUCTIONS`).
pub const UNIT_WORDS: usize = 256;
const PAGE: u64 = 4096;
/// Functions longer than this are counted but not analyzed.
const LONGEST: usize = 1 << 16;
/// The guest registers a unit can keep in host registers besides the
/// pinned ones: R8 to R11 (`x86_64::pinned::LOCAL_HOSTS`).
pub const LOCALS: usize = 4;

/// One function's place.
#[derive(Debug, Clone, Copy)]
pub struct Place {
    pub image: usize,
    pub start: u64,
    pub end: u64,
}

impl Place {
    pub const fn words(&self) -> usize {
        ((self.end - self.start) / 4) as usize
    }

    /// The words of the unit entered at the function's start: to its end,
    /// the page's end, or [`UNIT_WORDS`].
    pub fn unit_words(&self) -> usize {
        let page_end = (self.start / PAGE + 1) * PAGE;
        (((self.end.min(page_end)) - self.start) / 4).min(UNIT_WORDS as u64) as usize
    }

    pub fn fits(&self) -> bool {
        self.unit_words() == self.words()
    }
}

/// Every function of `images`: from each start to the next or the end of
/// its code range, ascending.
pub fn places(images: &[Image<'_>]) -> Vec<Place> {
    let mut places = Vec::new();
    for (index, image) in images.iter().enumerate() {
        for code in &image.code {
            let end = code.address + code.bytes.len() as u64;
            let inside: Vec<u64> = image
                .starts
                .iter()
                .copied()
                .filter(|start| (code.address..end).contains(start))
                .collect();
            for (at, &start) in inside.iter().enumerate() {
                let next = inside.get(at + 1).copied().unwrap_or(end);
                places.push(Place {
                    image: index,
                    start,
                    end: next,
                });
            }
        }
    }
    places.sort_by_key(|place| place.start);
    places
}

/// `pc weight` pairs from sample lines, `#` comments and blank lines
/// skipped.
pub fn read_samples(text: &str) -> Vec<(u64, u64)> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pc = u64::from_str_radix(fields.next()?.trim_start_matches("0x"), 16).ok()?;
            let weight = fields.next()?.parse().ok()?;
            Some((pc, weight))
        })
        .collect()
}

/// The function holding `pc`.
pub fn find(places: &[Place], pc: u64) -> Option<usize> {
    let at = places.partition_point(|place| place.start <= pc).checked_sub(1)?;
    (pc < places[at].end).then_some(at)
}

/// The counts, each also as a sample weight.
#[derive(Debug, Default, Clone, Copy)]
struct Count {
    count: u64,
    weight: u64,
}

impl Count {
    fn add(&mut self, count: u64, weight: u64) {
        self.count += count;
        self.weight += weight;
    }
}

#[derive(Debug, Default)]
pub struct Census {
    functions: Count,
    words: Count,
    analyzed: u64,
    fits: Count,
    fits_words: u64,
    unit_words: u64,
    sizes: [Count; 6],
    leaf: Count,
    unknown_words: u64,
    calls_direct: u64,
    calls_intra: u64,
    calls_indirect: u64,
    returns: u64,
    returns_pac: u64,
    tails: u64,
    unpinned: [Count; 6],
    /// Weighted register uses outside the pinned set, and those the four
    /// most used of each function take.
    unpinned_uses: [f64; 2],
    flags: [u64; 2],
    sampled_weight: u64,
    found_weight: u64,
    in_unit_weight: u64,
    hot: Vec<(u64, usize)>,
}

fn size_bucket(words: usize) -> usize {
    match words {
        0..=16 => 0,
        17..=64 => 1,
        65..=256 => 2,
        257..=1024 => 3,
        1025..=4096 => 4,
        _ => 5,
    }
}

fn unpinned_bucket(registers: u32) -> usize {
    match registers {
        0 => 0,
        1..=2 => 1,
        3..=4 => 2,
        5..=6 => 3,
        7..=8 => 4,
        _ => 5,
    }
}

impl Census {
    /// Count the functions of `images` at `places`, weighted by `samples`.
    pub fn of(images: &[Image<'_>], places: &[Place], samples: &[(u64, u64)]) -> Self {
        let mut census = Self::default();
        // Each function's sample weight, and how much of it falls in the
        // unit entered at its start.
        let mut weight = vec![0u64; places.len()];
        let mut in_unit = vec![0u64; places.len()];
        for &(pc, sample) in samples {
            census.sampled_weight += sample;
            if let Some(at) = find(places, pc) {
                weight[at] += sample;
                census.found_weight += sample;
                if pc - places[at].start < 4 * places[at].unit_words() as u64 {
                    in_unit[at] += sample;
                }
            }
        }
        census.in_unit_weight = in_unit.iter().sum();
        let starts: std::collections::HashSet<u64> =
            places.iter().map(|place| place.start).collect();
        for (at, place) in places.iter().enumerate() {
            let w = weight[at];
            let words = place.words();
            census.functions.add(1, w);
            census.words.add(words as u64, w);
            census.sizes[size_bucket(words)].add(1, w);
            census.unit_words += place.unit_words() as u64;
            if place.fits() {
                census.fits.add(1, w);
                census.fits_words += words as u64;
            }
            if w != 0 {
                census.hot.push((w, at));
            }
            if words > LONGEST {
                continue;
            }
            let Some(bytes) = images[place.image].bytes(place.start, 4 * words) else {
                continue;
            };
            census.analyzed += 1;
            let function = Function::analyze(place.start, bytes, Boundary::Abi);
            if function.leaf() {
                census.leaf.add(1, w);
            }
            census.unknown_words += function
                .insns
                .iter()
                .filter(|insn| !insn.effects.known)
                .count() as u64;
            for call in &function.calls {
                match call.target {
                    Some(target) => {
                        census.calls_direct += 1;
                        if starts.contains(&target) {
                            census.calls_intra += 1;
                        }
                    }
                    None => census.calls_indirect += 1,
                }
            }
            for insn in &function.insns {
                if insn.flow == Flow::Return {
                    census.returns += 1;
                    if insn.effects.pac {
                        census.returns_pac += 1;
                    }
                }
            }
            census.tails += function.tails.len() as u64;
            let uses = function.register_uses();
            let mut unpinned: Vec<u64> = (0..31)
                .filter(|&index| PINNED & (1 << index) == 0 && index != 30)
                .map(|index| uses[index])
                .filter(|&count| count != 0)
                .collect();
            census.unpinned[unpinned_bucket(unpinned.len() as u32)].add(1, w);
            unpinned.sort_unstable_by(|a, b| b.cmp(a));
            let total: u64 = unpinned.iter().sum();
            let top: u64 = unpinned.iter().take(LOCALS).sum();
            // Weighted by the function's samples, or by one where there are
            // none.
            let scale = w.max(1) as f64;
            census.unpinned_uses[0] += scale * total as f64;
            census.unpinned_uses[1] += scale * top as f64;
            let (dead, setters) = function.dead_flag_setters();
            census.flags[0] += dead as u64;
            census.flags[1] += setters as u64;
        }
        census.hot.sort_unstable_by(|a, b| b.cmp(a));
        census
    }

    /// The hottest functions' indexes in the places, hottest first.
    pub fn hottest(&self, count: usize) -> impl Iterator<Item = (u64, usize)> + '_ {
        self.hot.iter().copied().take(count)
    }

    /// The census as report lines.
    pub fn report(&self, images: &[Image<'_>], places: &[Place], top: usize) -> String {
        let mut out = String::new();
        let percent = |part: u64, whole: u64| 100.0 * part as f64 / whole.max(1) as f64;
        let w = |count: &Count| percent(count.weight, self.found_weight);
        let _ = writeln!(
            out,
            "functions {} words {} (analyzed {}, words of unknown effect {})",
            self.functions.count, self.words.count, self.analyzed, self.unknown_words
        );
        let _ = writeln!(
            out,
            "unit (one page, <= {UNIT_WORDS} words): whole functions {:.1}% ({:.1}% of samples), \
             words covered from each start {:.1}%",
            percent(self.fits.count, self.functions.count),
            w(&self.fits),
            percent(self.unit_words, self.words.count)
        );
        let names = ["<=16", "17-64", "65-256", "257-1024", "1025-4096", ">4096"];
        let sizes: Vec<String> = names
            .iter()
            .zip(&self.sizes)
            .map(|(name, count)| {
                format!(
                    "{name}:{:.1}%/{:.1}%",
                    percent(count.count, self.functions.count),
                    w(count)
                )
            })
            .collect();
        let _ = writeln!(out, "sizes in words (functions/samples): {}", sizes.join(" "));
        let _ = writeln!(
            out,
            "leaf functions {:.1}% ({:.1}% of samples)",
            percent(self.leaf.count, self.functions.count),
            w(&self.leaf)
        );
        let _ = writeln!(
            out,
            "calls: direct {} ({:.1}% to a function start of these images), indirect {}; \
             returns {} ({:.1}% RETA*); tail branches {}",
            self.calls_direct,
            percent(self.calls_intra, self.calls_direct),
            self.calls_indirect,
            self.returns,
            percent(self.returns_pac, self.returns),
            self.tails
        );
        let names = ["0", "1-2", "3-4", "5-6", "7-8", "9+"];
        let unpinned: Vec<String> = names
            .iter()
            .zip(&self.unpinned)
            .map(|(name, count)| {
                format!(
                    "{name}:{:.1}%/{:.1}%",
                    percent(count.count, self.functions.count),
                    w(count)
                )
            })
            .collect();
        let _ = writeln!(
            out,
            "registers used outside the pinned six and X30 (functions/samples): {}",
            unpinned.join(" ")
        );
        let _ = writeln!(
            out,
            "the {LOCALS} most used of each function take {:.1}% of those uses (loop-weighted, \
             sample-weighted)",
            100.0 * self.unpinned_uses[1] / self.unpinned_uses[0].max(1.0)
        );
        let _ = writeln!(
            out,
            "flag setters whose NZCV no path reads (AAPCS64 at calls and returns): {:.1}% of {}",
            percent(self.flags[0], self.flags[1]),
            self.flags[1]
        );
        if self.sampled_weight != 0 {
            let _ = writeln!(
                out,
                "samples: weight {} found in a function {:.1}%, in the unit entered at its \
                 function's start {:.1}%",
                self.sampled_weight,
                percent(self.found_weight, self.sampled_weight),
                percent(self.in_unit_weight, self.sampled_weight)
            );
            let _ = writeln!(
                out,
                "hottest: weight% words unit fits unpinned-registers image function"
            );
            for (weight, at) in self.hottest(top) {
                let place = &places[at];
                let image = &images[place.image];
                let name = image
                    .symbol(place.start)
                    .map_or_else(|| format!("sub_{:x}", place.start), str::to_owned);
                let unpinned = image
                    .bytes(place.start, 4 * place.words())
                    .map(|bytes| {
                        let uses =
                            Function::analyze(place.start, bytes, Boundary::Abi).register_uses();
                        (0..31)
                            .filter(|&index| {
                                PINNED & (1 << index) == 0 && index != 30 && uses[index] != 0
                            })
                            .count()
                    })
                    .unwrap_or(0);
                let short = image.name.rsplit('/').next().unwrap_or(&image.name);
                let _ = writeln!(
                    out,
                    "  {:5.2}% {:5} {:3} {} {:2} {short} {name}",
                    percent(weight, self.sampled_weight),
                    place.words(),
                    place.unit_words(),
                    if place.fits() { "yes" } else { " no" },
                    unpinned
                );
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn units_end_at_pages_and_their_bound() {
        let place = |start, end| Place {
            image: 0,
            start,
            end,
        };
        assert_eq!(place(0x1000, 0x1100).unit_words(), 64);
        assert!(place(0x1000, 0x1100).fits());
        // Past the page.
        assert_eq!(place(0x1f00, 0x2100).unit_words(), 64);
        // Past the bound.
        assert_eq!(place(0x2000, 0x2800).unit_words(), UNIT_WORDS);
        assert!(!place(0x2000, 0x2800).fits());
    }

    #[test]
    fn samples_find_their_functions() {
        let places = [
            Place {
                image: 0,
                start: 0x1000,
                end: 0x1040,
            },
            Place {
                image: 0,
                start: 0x1040,
                end: 0x1100,
            },
        ];
        assert_eq!(find(&places, 0x1000), Some(0));
        assert_eq!(find(&places, 0x10fc), Some(1));
        assert_eq!(find(&places, 0x1100), None);
        assert_eq!(find(&places, 0xfff), None);
        assert_eq!(
            read_samples("fffffe000b9c8384 249 ab\n# no\n\n1000 3"),
            vec![(0xffff_fe00_0b9c_8384, 249), (0x1000, 3)]
        );
    }
}
