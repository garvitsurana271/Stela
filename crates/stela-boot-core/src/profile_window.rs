//! `--profile`'s whole tables and marked windows, which the heartbeat's
//! per-beat lines cannot give: every guest code page's samples (`PES PC
//! PAGES`), exact top-N shares of the cycle and transfer samplers (`PES PC
//! SHARE`, `PES HOT SHARE`), the tester's window marks (`PROFILE MARK`) and
//! the guest images' identity (`PROFILE DYLD CACHE`, `PROFILE KERNEL
//! COLLECTION`).
//!
//! - [`PcPages`] sums the cycle sampler's blocks (`step_timing::PcSamples`)
//!   by their 4 KiB fetch page, the unit whose SHA-256 names the page: a
//!   boot's few thousand executed pages hold every named sample, so `cargo
//!   xtask symbolize --pages` attributes all of them to images, where the
//!   `PES PC TOTAL` lines list 256 blocks a level.
//! - [`HeavyHitters`] is the space-saving summary (Metwally, Agrawal and El
//!   Abbadi, "Efficient Computation of Frequent and Top-k Elements in Data
//!   Streams", ICDT 2005): a key that is not held never had more weight than
//!   the least held one, and a held key's weight overstates its own by at
//!   most its `error`. While nothing was evicted every weight is exact.
//!   [`HotCounts`] takes a beat's transfer samples whole, so the beat's
//!   table filling (`step_timing::HOT_BLOCKS`) no longer folds them into
//!   `other` unseen; the boot side drains the board's table often enough
//!   that it does not fill.
//! - [`MarkKeys`] finds Control+Option(Alt)+Shift+M on the host keyboard,
//!   which the guest does not get: the heartbeat ends a beat there, so every
//!   per-beat line splits at the mark and the window's tables are said.
//! - [`dyld_header`] and [`mach_header`] decode what the boot reads of the
//!   guest's dyld shared cache and kernel collection: their UUIDs and
//!   slides, so a log maps to its images without searching for bytes.
use alloc::{
    collections::{BTreeMap, BTreeSet},
    format,
    string::String,
    vec::Vec,
};
use stela_engine::machine::engine::step_timing::{
    HotBlock, HotBlocks, Level, PcSamples,
    pc_samples::{level_name, pc_name},
};

/// The levels in the order the lines print them.
pub const LEVELS: [Level; 3] = [Level::User, Level::Kernel, Level::Guarded];

/// A held key's counts ([`HeavyHitters`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Slot {
    /// The weight counted for the key, its `error` included.
    pub weight: u64,
    /// The weight the key may have been given for keys it replaced: its
    /// own is at least `weight - error`.
    pub error: u64,
    /// A second count of the key's own (samples where the weight is
    /// cycles), counted from when it was last taken in.
    pub aux: u64,
}

/// The space-saving summary of a weighted stream: at most `capacity` keys,
/// the heaviest kept ([`Self::add`]).
#[derive(Debug, Clone)]
pub struct HeavyHitters<K: Ord + Copy> {
    capacity: usize,
    slots: BTreeMap<K, Slot>,
    /// The held keys by weight, lightest first.
    order: BTreeSet<(u64, K)>,
    /// Every weight added.
    pub total: u64,
    /// Keys replaced to take in others.
    pub evicted: u64,
}

impl<K: Ord + Copy> HeavyHitters<K> {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            slots: BTreeMap::new(),
            order: BTreeSet::new(),
            total: 0,
            evicted: 0,
        }
    }

    /// Count `weight` (and `aux`) for `key`. A new key that finds the
    /// summary full replaces the lightest, taking its weight as its error.
    pub fn add(&mut self, key: K, weight: u64, aux: u64) {
        self.total += weight;
        if let Some(slot) = self.slots.get_mut(&key) {
            self.order.remove(&(slot.weight, key));
            slot.weight += weight;
            slot.aux += aux;
            self.order.insert((slot.weight, key));
            return;
        }
        let mut slot = Slot {
            weight,
            error: 0,
            aux,
        };
        if self.slots.len() >= self.capacity
            && let Some((floor, victim)) = self.order.pop_first()
        {
            self.slots.remove(&victim);
            self.evicted += 1;
            slot.weight += floor;
            slot.error = floor;
        }
        self.order.insert((slot.weight, key));
        self.slots.insert(key, slot);
    }

    /// Whether every held weight is exact: nothing was replaced.
    pub fn exact(&self) -> bool {
        self.evicted == 0
    }

    /// The most weight a key that is not held can have had: the lightest
    /// held one's, once the summary replaced keys; else 0.
    pub fn floor(&self) -> u64 {
        match self.exact() {
            true => 0,
            false => self.order.first().map_or(0, |(weight, _)| *weight),
        }
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// The `shown` heaviest keys, heaviest first.
    pub fn top(&self, shown: usize) -> Vec<(K, Slot)> {
        self.order
            .iter()
            .rev()
            .take(shown)
            .map(|&(_, key)| (key, self.slots[&key]))
            .collect()
    }

    /// The weight the `shown` heaviest keys certainly had (each one's
    /// weight less its error): exact while [`Self::exact`].
    pub fn top_weight(&self, shown: usize) -> u64 {
        self.top(shown)
            .iter()
            .map(|(_, slot)| slot.weight - slot.error)
            .sum()
    }
}

/// `{weight} * 1000 / {of}`, 0 of nothing.
fn permille(weight: u64, of: u64) -> u64 {
    (u128::from(weight) * 1000)
        .checked_div(u128::from(of))
        .unwrap_or(0) as u64
}

/// The shares the share lines give: the 24, 100, 1000 and 10000 heaviest.
pub const SHARES: [usize; 4] = [24, 100, 1000, 10_000];

/// ` exact=yes|no evicted=E floor=F top24=.. top100=.. top1000=..
/// top10000=..` of `summary`, per mille of `of`: lower bounds while not
/// exact.
fn share_words<K: Ord + Copy>(summary: &HeavyHitters<K>, of: u64) -> String {
    let mut words = format!(
        " exact={} evicted={} floor={}",
        if summary.exact() { "yes" } else { "no" },
        summary.evicted,
        summary.floor()
    );
    for shown in SHARES {
        words.push_str(&format!(
            " top{shown}={}",
            permille(summary.top_weight(shown), of)
        ));
    }
    words
}

/// A level's index in the per-level arrays, as `Level as usize`.
fn index(level: Level) -> usize {
    level as usize
}

/// The PCs of a page's blocks less their offsets: the page's own PC, as the
/// lines print it ([`pc_name`]).
fn page_pc(pc: u64, offset: u16) -> u64 {
    pc.wrapping_sub(u64::from(offset))
}

/// Every named cycle sample since the window began, by 4 KiB fetch page:
/// `(level, page key, page PC)` to `(samples, cycles)`, at most
/// [`PcPages::MOST`] pages; samples of further pages count as `other`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PcPages {
    pages: BTreeMap<(u8, Option<u64>, u64), (u64, u64)>,
    pub samples: [u64; 3],
    pub cycles: [u64; 3],
    /// The beats' own `other` (blocks their tables had no room for) and
    /// the pages past [`Self::MOST`].
    pub other: [u64; 2],
    pub gone: [u64; 2],
}

impl PcPages {
    /// The pages held apart at most: far above a boot's executed pages.
    pub const MOST: usize = 1 << 18;

    /// Add a beat's samples, every block of its table by page.
    pub fn add(&mut self, beat: &PcSamples) {
        for level in LEVELS {
            let at = index(level);
            self.samples[at] += beat.samples[at];
            self.cycles[at] += beat.cycles[at];
            for block in beat.top(level, usize::MAX) {
                let key = (level as u8, block.page, page_pc(block.pc, block.offset));
                let room = self.pages.len() < Self::MOST;
                match self.pages.get_mut(&key) {
                    Some(counts) => {
                        counts.0 += block.samples;
                        counts.1 += block.cycles;
                    }
                    None if room => {
                        self.pages.insert(key, (block.samples, block.cycles));
                    }
                    None => {
                        self.other[0] += block.samples;
                        self.other[1] += block.cycles;
                    }
                }
            }
        }
        for slot in 0..2 {
            self.other[slot] += beat.other[slot];
            self.gone[slot] += beat.gone[slot];
        }
    }

    pub fn is_empty(&self) -> bool {
        self.samples.iter().all(|&samples| samples == 0) && self.gone[0] == 0
    }

    pub fn len(&self) -> usize {
        self.pages.len()
    }

    /// The lines of window `window` under `prefix` (`PES PC PAGES`), in
    /// the `PES PC` lines' form (`step_timing::PcSamples::lines`), each
    /// page as a block at offset 0 of its page:
    ///
    /// - `{prefix} samples=N cycles=C gone=G/GC other=O/OC w=W pages=P`;
    /// - per level with samples, its pages with the most cycles first, at
    ///   most `per_line` a line: `{prefix} {level} samples=N cycles=C
    ///   pages=P part=I/J {pc}:{samples}:{cycles}@{page}+0 ...`.
    pub fn lines(&self, prefix: &str, window: u32, per_line: usize) -> Vec<String> {
        if self.is_empty() {
            return Vec::new();
        }
        let mut lines = alloc::vec![format!(
            "{prefix} samples={} cycles={} gone={}/{} other={}/{} w={window} pages={}",
            self.samples.iter().sum::<u64>(),
            self.cycles.iter().sum::<u64>(),
            self.gone[0],
            self.gone[1],
            self.other[0],
            self.other[1],
            self.pages.len(),
        )];
        let per_line = per_line.max(1);
        for level in LEVELS {
            let at = index(level);
            if self.samples[at] == 0 {
                continue;
            }
            let mut pages: Vec<(Option<u64>, u64, u64, u64)> = self
                .pages
                .iter()
                .filter(|(key, _)| key.0 == level as u8)
                .map(|(&(_, page, pc), &(samples, cycles))| (page, pc, samples, cycles))
                .collect();
            pages.sort_unstable_by(|a, b| b.3.cmp(&a.3).then(b.2.cmp(&a.2)).then(a.cmp(b)));
            let parts = pages.len().div_ceil(per_line).max(1);
            for (part, chunk) in pages.chunks(per_line).enumerate() {
                let mut line = format!(
                    "{prefix} {} samples={} cycles={} pages={} part={}/{parts}",
                    level_name(level),
                    self.samples[at],
                    self.cycles[at],
                    pages.len(),
                    part + 1,
                );
                for &(page, pc, samples, cycles) in chunk {
                    let page = match page {
                        Some(page) => format!("{page:016x}"),
                        None => String::from("?"),
                    };
                    line.push_str(&format!(
                        " {}:{samples}:{cycles}@{page}+0",
                        pc_name(level, pc)
                    ));
                }
                lines.push(line);
            }
        }
        lines
    }
}

/// A cycle sample's block: page key, offset, PC.
pub type PcKey = (Option<u64>, u16, u64);

/// The cycle sampler's blocks by level, weighed by cycles, for exact top-N
/// shares ([`HeavyHitters`]).
#[derive(Debug, Clone)]
pub struct PcShares {
    pub levels: [HeavyHitters<PcKey>; 3],
    pub samples: [u64; 3],
    pub cycles: [u64; 3],
}

impl PcShares {
    pub fn new(capacity: usize) -> Self {
        Self {
            levels: core::array::from_fn(|_| HeavyHitters::new(capacity)),
            samples: [0; 3],
            cycles: [0; 3],
        }
    }

    /// Add a beat's samples. Its `other` blocks count in the levels'
    /// cycles and in no block.
    pub fn add(&mut self, beat: &PcSamples) {
        for level in LEVELS {
            let at = index(level);
            self.samples[at] += beat.samples[at];
            self.cycles[at] += beat.cycles[at];
            for block in beat.top(level, usize::MAX) {
                self.levels[at].add(
                    (block.page, block.offset, block.pc),
                    block.cycles,
                    block.samples,
                );
            }
        }
    }

    /// `{prefix} {level} w=W samples=N cycles=C blocks=B exact= evicted=
    /// floor= top24= top100= top1000= top10000=`, per level with samples:
    /// the heaviest blocks' share of the level's cycles, per mille.
    pub fn lines(&self, prefix: &str, window: Option<u32>) -> Vec<String> {
        let mut lines = Vec::new();
        for level in LEVELS {
            let at = index(level);
            if self.samples[at] == 0 {
                continue;
            }
            let window = window.map_or(String::new(), |window| format!(" w={window}"));
            lines.push(format!(
                "{prefix} {}{window} samples={} cycles={} blocks={}{}",
                level_name(level),
                self.samples[at],
                self.cycles[at],
                self.levels[at].len(),
                share_words(&self.levels[at], self.cycles[at]),
            ));
        }
        lines
    }
}

/// The transfer sampler's samples of some beats, whole
/// (`step_timing::HotBlocks` holds [`step_timing::HOT_BLOCKS`] blocks).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HotCounts {
    blocks: BTreeMap<(u8, u64), u64>,
    pub samples: [u64; 3],
    /// Samples the drained tables had no room for.
    pub other: u64,
}

impl HotCounts {
    /// Take in a drained table's blocks and counts (not its address spaces
    /// or dynamic transfers).
    pub fn absorb(&mut self, hot: &HotBlocks) {
        for entry in hot.counted() {
            *self
                .blocks
                .entry((entry.level.min(2), entry.pc))
                .or_default() += entry.count;
        }
        for (mine, theirs) in self.samples.iter_mut().zip(hot.samples) {
            *mine += theirs;
        }
        self.other += hot.other;
    }

    pub fn is_empty(&self) -> bool {
        self.samples.iter().all(|&samples| samples == 0)
    }

    /// The `shown` blocks of `level` with the most samples, most first.
    pub fn top(&self, level: Level, shown: usize) -> Vec<HotBlock> {
        let mut blocks: Vec<HotBlock> = self
            .blocks
            .range((level as u8, 0)..=(level as u8, u64::MAX))
            .map(|(&(level, pc), &count)| HotBlock { pc, level, count })
            .collect();
        blocks.sort_unstable_by(|a, b| b.count.cmp(&a.count).then(a.pc.cmp(&b.pc)));
        blocks.truncate(shown);
        blocks
    }

    /// Every block, by level.
    pub fn blocks(&self) -> impl Iterator<Item = (Level, u64, u64)> + '_ {
        self.blocks
            .iter()
            .map(|(&(level, pc), &count)| (LEVELS_BY_INDEX[usize::from(level)], pc, count))
    }
}

/// [`Level`]s by their `as usize`.
const LEVELS_BY_INDEX: [Level; 3] = [Level::User, Level::Kernel, Level::Guarded];

/// The transfer sampler's blocks by level since some beat, for exact top-N
/// shares.
#[derive(Debug, Clone)]
pub struct HotShares {
    pub levels: [HeavyHitters<u64>; 3],
    pub samples: [u64; 3],
    pub other: u64,
}

impl HotShares {
    pub fn new(capacity: usize) -> Self {
        Self {
            levels: core::array::from_fn(|_| HeavyHitters::new(capacity)),
            samples: [0; 3],
            other: 0,
        }
    }

    pub fn add(&mut self, beat: &HotCounts) {
        for (level, pc, count) in beat.blocks() {
            self.levels[index(level)].add(pc, count, 0);
        }
        for (mine, theirs) in self.samples.iter_mut().zip(beat.samples) {
            *mine += theirs;
        }
        self.other += beat.other;
    }

    /// The `shown` most sampled blocks of `level`, most first, as weights
    /// (exact while the level's summary is).
    pub fn top(&self, level: Level, shown: usize) -> Vec<HotBlock> {
        self.levels[index(level)]
            .top(shown)
            .into_iter()
            .map(|(pc, slot)| HotBlock {
                pc,
                level: level as u8,
                count: slot.weight,
            })
            .collect()
    }

    /// `{prefix} {level}{ w=W} samples=N other=O blocks=B exact= evicted=
    /// floor= top24= ... top10000=` per mille of the level's samples.
    pub fn line(&self, prefix: &str, level: Level, window: Option<u32>) -> Option<String> {
        let at = index(level);
        let samples = self.samples[at];
        if samples == 0 {
            return None;
        }
        let window = window.map_or(String::new(), |window| format!(" w={window}"));
        Some(format!(
            "{prefix} {}{window} samples={samples} other={} blocks={}{}",
            level_name(level),
            self.other,
            self.levels[at].len(),
            share_words(&self.levels[at], samples),
        ))
    }
}

/// Keyboard/Keypad usages of the mark ([`MarkKeys`]).
pub const USAGE_M: u8 = 0x10;
const LEFT_CONTROL: u8 = 0xe0;
const RIGHT_GUI: u8 = 0xe7;

/// What to do with one host key change ([`MarkKeys::key`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    /// Give it to the guest.
    Deliver,
    /// A window mark: the M of Control+Option+Shift+M, kept from the guest.
    Mark,
    /// The release of a mark's M, kept from the guest too.
    Swallow,
}

/// Control+Option(Alt)+Shift+M, either side's modifiers, no Command:
/// `--profile`'s window mark. The modifiers reach the guest as they are
/// (it sees them pressed and released with nothing between); the M does
/// not.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MarkKeys {
    /// Modifiers held, bit `usage - 0xE0`.
    held: u8,
    /// The M pressed for a mark, whose release is kept from the guest too.
    marking: bool,
}

impl MarkKeys {
    pub const fn new() -> Self {
        Self {
            held: 0,
            marking: false,
        }
    }

    pub fn key(&mut self, usage: u8, pressed: bool) -> KeyAction {
        if (LEFT_CONTROL..=RIGHT_GUI).contains(&usage) {
            let bit = 1 << (usage - LEFT_CONTROL);
            match pressed {
                true => self.held |= bit,
                false => self.held &= !bit,
            }
            return KeyAction::Deliver;
        }
        if usage != USAGE_M {
            return KeyAction::Deliver;
        }
        // Left and right: Control 0/4, Shift 1/5, Alt 2/6, GUI 3/7.
        let either = |bit: u8| self.held & (1 << bit | 1 << (bit + 4)) != 0;
        match pressed {
            true if either(0) && either(1) && either(2) && !either(3) => {
                self.marking = true;
                KeyAction::Mark
            }
            false if core::mem::take(&mut self.marking) => KeyAction::Swallow,
            _ => KeyAction::Deliver,
        }
    }
}

/// The dyld shared cache's magic starts so: `dyld_v1   arm64e`.
pub const DYLD_MAGIC: &[u8] = b"dyld_v1 ";

/// What a dyld shared cache's header says of it (`dyld_cache_header`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DyldHeader {
    pub magic: [u8; 16],
    pub uuid: [u8; 16],
    /// Its first mapping's unslid address, where the header lies: the
    /// slide is where it was found less this.
    pub unslid: u64,
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

fn u64_at(bytes: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(bytes.get(at..at + 8)?.try_into().ok()?))
}

/// A dyld shared cache's header from its first bytes (`header`, at least
/// through its first mapping): the magic at 0, `mappingOffset` at 16, the
/// UUID at 88, and the first `dyld_cache_mapping_info`'s address.
pub fn dyld_header(header: &[u8]) -> Option<DyldHeader> {
    if !header.starts_with(DYLD_MAGIC) {
        return None;
    }
    let mapping = u32_at(header, 16)? as usize;
    if mapping < 104 {
        return None;
    }
    Some(DyldHeader {
        magic: header.get(..16)?.try_into().ok()?,
        uuid: header.get(88..104)?.try_into().ok()?,
        unslid: u64_at(header, mapping)?,
    })
}

/// `MH_MAGIC_64` and arm64's CPU type.
const MH_MAGIC_64: u32 = 0xfeed_facf;
const CPU_TYPE_ARM64: u32 = 0x0100_000c;
/// `MH_EXECUTE`, the kernel's; `MH_FILESET`, a kernel collection's.
pub const MH_EXECUTE: u32 = 2;
pub const MH_FILESET: u32 = 0xc;
/// The bytes of `mach_header_64`.
pub const MACH_HEADER_BYTES: usize = 32;

/// An arm64 `mach_header_64`'s file type and load commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MachHeader {
    pub filetype: u32,
    pub ncmds: u32,
    pub sizeofcmds: u32,
}

pub fn mach_header(bytes: &[u8]) -> Option<MachHeader> {
    if u32_at(bytes, 0)? != MH_MAGIC_64 || u32_at(bytes, 4)? != CPU_TYPE_ARM64 {
        return None;
    }
    Some(MachHeader {
        filetype: u32_at(bytes, 12)?,
        ncmds: u32_at(bytes, 16)?,
        sizeofcmds: u32_at(bytes, 20)?,
    })
}

/// What a Mach-O's load commands say of it ([`load_info`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LoadInfo {
    /// The `__TEXT` segment's unslid address.
    pub text: Option<u64>,
    pub uuid: Option<[u8; 16]>,
    /// A kernel collection's `com.apple.kernel` entry's unslid address.
    pub kernel: Option<u64>,
}

const LC_SEGMENT_64: u32 = 0x19;
const LC_UUID: u32 = 0x1b;
const LC_FILESET_ENTRY: u32 = 0x8000_0035;

/// Read `ncmds` load commands from `commands` (the bytes after the
/// header): `LC_SEGMENT_64` `__TEXT`'s `vmaddr`, `LC_UUID`, and the
/// `LC_FILESET_ENTRY` named `com.apple.kernel`.
pub fn load_info(commands: &[u8], ncmds: u32) -> LoadInfo {
    let mut info = LoadInfo::default();
    let mut at = 0usize;
    for _ in 0..ncmds {
        let (Some(command), Some(size)) = (u32_at(commands, at), u32_at(commands, at + 4)) else {
            break;
        };
        let size = size as usize;
        let Some(body) = commands.get(at..at + size).filter(|_| size >= 8) else {
            break;
        };
        match command {
            LC_SEGMENT_64 if body.get(8..14) == Some(b"__TEXT") && body.get(14) == Some(&0) => {
                info.text = info.text.or(u64_at(body, 24));
            }
            LC_UUID => info.uuid = body.get(8..24).and_then(|uuid| uuid.try_into().ok()),
            LC_FILESET_ENTRY => {
                let name = u32_at(body, 24)
                    .and_then(|offset| body.get(offset as usize..))
                    .map(|name| name.split(|&byte| byte == 0).next().unwrap_or(&[]));
                if name == Some(b"com.apple.kernel") {
                    info.kernel = u64_at(body, 8);
                }
            }
            _ => {}
        }
        at += size;
    }
    info
}

/// A UUID as `XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX`, as `dyld_info` and
/// `dwarfdump --uuid` print it.
pub fn uuid_text(uuid: &[u8; 16]) -> String {
    let mut text = String::new();
    for (at, byte) in uuid.iter().enumerate() {
        if matches!(at, 4 | 6 | 8 | 10) {
            text.push('-');
        }
        text.push_str(&format!("{byte:02X}"));
    }
    text
}

/// The printable part of a magic, up to its first NUL.
pub fn magic_text(magic: &[u8]) -> String {
    magic
        .iter()
        .take_while(|&&byte| byte != 0)
        .map(|&byte| match byte {
            0x20..=0x7e => byte as char,
            _ => '.',
        })
        .collect()
}

#[cfg(test)]
mod tests;
