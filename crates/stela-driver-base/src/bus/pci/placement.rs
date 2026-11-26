//! A place for a memory BAR the firmware left unassigned (its register at
//! zero): the lowest free span of the BAR's size, aligned to it, inside the
//! memory windows the function's root bridge declares in its ACPI `_CRS`
//! and every bridge above the function forwards, clear of every range
//! anything else claims.
//!
//! Linux's `pci_assign_unassigned_root_bus_resources` places such a BAR the
//! same way: first fit from the bottom of the host bridge's `_CRS` windows,
//! around the resources other functions were found with. This is narrower.
//! It never moves or resizes a BAR the firmware assigned
//! ([`unassigned`]), it never sizes another function's BAR (an assigned BAR
//! counts as large as its alignment allows, [`assigned_extent`]), and it
//! places nothing it cannot fit with room to spare: any doubt is a
//! [`Refusal`] the caller reports, never a guess.
//!
//! Everything here is arithmetic on bus addresses and parsing of firmware
//! tables; nothing reads or writes a device.
use super::Bar;
use alloc::vec::Vec;
use core::{fmt, ops::Range};

/// The smallest span a placed BAR takes: a page of its own, so it maps
/// alone, as Linux's `pci_realloc` and the hot-plug placement here give one.
pub const PAGE: u64 = 0x1000;
pub const FOUR_GIB: u64 = 1 << 32;
/// Nothing is placed below 1 MiB: legacy VGA memory, option ROMs and the
/// BIOS live there, and no host bridge window worth using starts lower.
pub const FLOOR: u64 = 1 << 20;
/// The largest BAR placed. The input controllers this serves decode a page
/// or a few; a larger request is no input controller's.
pub const MAX_BYTES: u64 = 16 << 20;
/// x86's fixed decode below 4 GiB: I/O APICs, HPET, VT-d units, the local
/// APIC and the boot flash. Firmware lists these in its memory map or its
/// motherboard resources only sometimes, so they are always claimed.
pub const X86_FIXED: Range<u64> = 0xfec0_0000..FOUR_GIB;

/// What a BAR needs placed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Request {
    /// The BAR's size: a power of two of 16 bytes or more.
    pub bytes: u64,
    /// A 64-bit BAR, which may go above 4 GiB.
    pub wide: bool,
    pub prefetchable: bool,
}

impl Request {
    /// The request for memory BAR `bar`, or none for an I/O BAR.
    pub fn of(bar: Bar) -> Option<Self> {
        match bar {
            Bar::Memory {
                size,
                wide,
                prefetchable,
                ..
            } => Some(Self {
                bytes: size,
                wide,
                prefetchable,
            }),
            Bar::Io { .. } => None,
        }
    }

    /// The span a placement takes and its alignment: the BAR's size, or a
    /// page for a smaller BAR.
    pub fn footprint(&self) -> u64 {
        self.bytes.max(PAGE)
    }

    fn check(&self) -> Result<(), Refusal> {
        if self.bytes < 16 || !self.bytes.is_power_of_two() || self.bytes > MAX_BYTES {
            return Err(Refusal::Size(self.bytes));
        }
        Ok(())
    }
}

/// Why no place was given.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Refusal {
    /// The BAR's size is no power of two of 16 bytes or more, or is past
    /// [`MAX_BYTES`].
    Size(u64),
    /// No window can hold a BAR of this type: none at all, or only windows
    /// above 4 GiB for a 32-bit BAR.
    NoWindow,
    /// Every window's free space is smaller than this many aligned bytes.
    NoRoom(u64),
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Size(bytes) => write!(
                f,
                "its size {bytes:#x} is no power of two from 16 bytes to {MAX_BYTES:#x}"
            ),
            Self::NoWindow => f.write_str("no memory window of its root bridge can hold it"),
            Self::NoRoom(bytes) => write!(
                f,
                "no free {bytes:#x}-byte span is left in its root bridge's memory windows"
            ),
        }
    }
}

/// Whether memory BAR `bar` is one the firmware left unassigned: its
/// register reads zero. A BAR at any other address is the firmware's, even
/// outside every window or with decoding off, and is never placed again.
pub fn unassigned(bar: Bar) -> bool {
    matches!(bar, Bar::Memory { address: 0, .. })
}

/// The lowest span of `request`'s footprint, aligned to it, inside
/// `windows` and clear of `claimed`. Windows below 4 GiB come first; a
/// 64-bit BAR goes above only when nothing below has room. Windows and
/// claims are half-open bus address ranges in any order; they may overlap.
pub fn place(
    windows: &[Range<u64>],
    claimed: &[Range<u64>],
    request: Request,
) -> Result<Range<u64>, Refusal> {
    request.check()?;
    let footprint = request.footprint();
    let claims = merged(claimed);
    let below = clip(windows, FLOOR..FOUR_GIB);
    let above = if request.wide {
        clip(windows, FOUR_GIB..u64::MAX)
    } else {
        Vec::new()
    };
    if below.is_empty() && above.is_empty() {
        return Err(Refusal::NoWindow);
    }
    below
        .iter()
        .chain(above.iter())
        .find_map(|window| first_fit(window, &claims, footprint))
        .ok_or(Refusal::NoRoom(footprint))
}

/// The first aligned span of `bytes` in `window` that no range of the
/// sorted, merged `claims` overlaps.
fn first_fit(window: &Range<u64>, claims: &[Range<u64>], bytes: u64) -> Option<Range<u64>> {
    let mut at = align_up(window.start, bytes)?;
    for claim in claims {
        let end = at.checked_add(bytes)?;
        if end > window.end {
            return None;
        }
        if claim.end <= at {
            continue;
        }
        if claim.start >= end {
            break;
        }
        at = align_up(claim.end, bytes)?;
    }
    let end = at.checked_add(bytes)?;
    (end <= window.end).then_some(at..end)
}

fn align_up(value: u64, alignment: u64) -> Option<u64> {
    value
        .checked_add(alignment - 1)
        .map(|value| value & !(alignment - 1))
}

/// `ranges` sorted by start, empty ones left out and overlapping or
/// touching ones joined.
fn merged(ranges: &[Range<u64>]) -> Vec<Range<u64>> {
    let mut sorted: Vec<Range<u64>> = ranges
        .iter()
        .filter(|range| range.start < range.end)
        .cloned()
        .collect();
    sorted.sort_unstable_by_key(|range| range.start);
    let mut joined: Vec<Range<u64>> = Vec::with_capacity(sorted.len());
    for range in sorted {
        match joined.last_mut() {
            Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
            _ => joined.push(range),
        }
    }
    joined
}

/// The parts of `windows` inside `bounds`, sorted and joined.
fn clip(windows: &[Range<u64>], bounds: Range<u64>) -> Vec<Range<u64>> {
    let clipped: Vec<Range<u64>> = windows
        .iter()
        .map(|window| window.start.max(bounds.start)..window.end.min(bounds.end))
        .collect();
    merged(&clipped)
}

/// The parts of `windows` that every list of `within` also holds: the
/// root bridge's windows narrowed by each bridge above the function.
pub fn intersect(windows: &[Range<u64>], within: &[Range<u64>]) -> Vec<Range<u64>> {
    let within = merged(within);
    let mut out = Vec::new();
    for window in merged(windows) {
        for bound in &within {
            let start = window.start.max(bound.start);
            let end = window.end.min(bound.end);
            if start < end {
                out.push(start..end);
            }
        }
    }
    out
}

/// The half-open range of inclusive `minimum..=maximum`, the end saturating
/// at the top of the address space; none when `maximum` is below `minimum`.
pub fn inclusive(minimum: u64, maximum: u64) -> Option<Range<u64>> {
    (minimum <= maximum).then(|| minimum..maximum.saturating_add(1))
}

/// The most an assigned memory BAR at `base` can decode: a BAR's size is a
/// power of two its address is aligned to, so it is at most the largest
/// power of two `base` is a multiple of. None for a BAR at zero, which
/// decodes nothing the firmware placed.
pub fn assigned_extent(base: u64) -> Option<Range<u64>> {
    if base == 0 {
        return None;
    }
    let most = 1u64.checked_shl(base.trailing_zeros())?;
    Some(base..base.saturating_add(most))
}

/// A bridge's memory windows as its Type 1 header gives them.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BridgeWindows {
    pub memory: Option<Range<u64>>,
    pub prefetchable: Option<Range<u64>>,
}

impl BridgeWindows {
    /// The windows the memory base/limit dword (0x20), the prefetchable
    /// base/limit dword (0x24) and its upper halves (0x28, 0x2c) open. A
    /// base above its limit closes a window.
    pub fn from_words(memory: u32, prefetchable: u32, upper: (u32, u32)) -> Self {
        let window = |word: u32, upper: (u64, u64)| {
            let base = upper.0 << 32 | u64::from(word & 0xfff0) << 16;
            let limit = upper.1 << 32 | u64::from(word & 0xfff0_0000) | 0xf_ffff;
            inclusive(base, limit)
        };
        let wide = prefetchable & 0xf == 1;
        let upper = if wide {
            (u64::from(upper.0), u64::from(upper.1))
        } else {
            (0, 0)
        };
        Self {
            memory: window(memory, (0, 0)),
            prefetchable: window(prefetchable, upper),
        }
    }

    /// Every range the bridge forwards downstream.
    pub fn ranges(&self) -> impl Iterator<Item = Range<u64>> + '_ {
        self.memory.iter().chain(self.prefetchable.iter()).cloned()
    }

    /// The windows a BAR below the bridge may sit in: the memory window,
    /// and the prefetchable one too for a prefetchable BAR.
    pub fn admitting(&self, request: Request) -> Vec<Range<u64>> {
        let mut out: Vec<Range<u64>> = self.memory.iter().cloned().collect();
        if request.prefetchable {
            out.extend(self.prefetchable.iter().cloned());
        }
        out
    }
}

/// Configuration dwords 0x10 through 0x3c of one function, as a census
/// read them: the BARs, a bridge's windows and the expansion ROM.
pub type Words = [u32; 12];

/// The register offset of `words[index]`.
const fn word_index(offset: u16) -> usize {
    ((offset - 0x10) / 4) as usize
}

/// What one function's `words` say it decodes: every assigned memory BAR
/// and its expansion ROM base as [`assigned_extent`] bounds them, and a
/// bridge's windows exactly. `bridge` is a Type 1 header; any other layout
/// is read as a Type 0 header.
pub fn decoded(words: &Words, bridge: bool) -> (Vec<Range<u64>>, Option<BridgeWindows>) {
    let bars = if bridge { 2 } else { 6 };
    let mut claims = Vec::new();
    let mut index = 0;
    while index < bars {
        let low = words[index];
        if low & 1 != 0 {
            index += 1;
            continue;
        }
        let wide = low & 6 == 4;
        let high = if wide && index + 1 < bars {
            u64::from(words[index + 1])
        } else {
            0
        };
        claims.extend(assigned_extent(high << 32 | u64::from(low & !0xf)));
        index += if wide { 2 } else { 1 };
    }
    let rom = words[word_index(if bridge { 0x38 } else { 0x30 })];
    claims.extend(assigned_extent(u64::from(rom & 0xffff_f800)));
    let windows = bridge.then(|| {
        BridgeWindows::from_words(
            words[word_index(0x20)],
            words[word_index(0x24)],
            (words[word_index(0x28)], words[word_index(0x2c)]),
        )
    });
    (claims, windows)
}

/// One memory descriptor of a `_CRS` buffer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AcpiMemory {
    /// Everything the descriptor can name, taken widely: a relocatable
    /// range's whole extent, and a translated one on both sides.
    pub claim: Vec<Range<u64>>,
    /// The window a producer address descriptor opens, exactly
    /// `minimum..=maximum` cut to its length; none for a consumer, a
    /// fixed or 24/32-bit memory descriptor, or a zero length.
    pub window: Option<Range<u64>>,
    /// The descriptor's bus-to-CPU translation.
    pub translation: u64,
}

/// Why a `_CRS` buffer could not be read for its memory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScanError {
    /// A descriptor runs past the buffer, or the buffer has no end tag.
    Truncated { offset: usize },
}

fn le(raw: &[u8], at: usize, width: usize) -> u64 {
    raw[at..at + width]
        .iter()
        .rev()
        .fold(0, |value, byte| value << 8 | u64::from(*byte))
}

/// Every memory descriptor in resource template `bytes`, those inside
/// dependent-function alternatives included (each alternative may be the
/// one chosen). Other descriptors are stepped over.
pub fn scan_memory(bytes: &[u8]) -> Result<Vec<AcpiMemory>, ScanError> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let tag = bytes[at];
        if tag & 0x80 == 0 {
            // A small descriptor; 0x79 is the end tag.
            if tag & 0x78 == 0x78 {
                return Ok(out);
            }
            at += 1 + usize::from(tag & 7);
            continue;
        }
        let body = bytes
            .get(at + 1..at + 3)
            .ok_or(ScanError::Truncated { offset: at })?;
        let length = 3 + usize::from(u16::from_le_bytes([body[0], body[1]]));
        let raw = bytes
            .get(at..at + length)
            .ok_or(ScanError::Truncated { offset: at })?;
        let short = ScanError::Truncated { offset: at };
        let fixed = |minimum: u64, maximum: u64, bytes: u64| AcpiMemory {
            claim: inclusive(minimum, maximum.saturating_add(bytes.saturating_sub(1)))
                .filter(|_| bytes != 0)
                .into_iter()
                .collect(),
            window: None,
            translation: 0,
        };
        match tag {
            // 24-bit memory range: minimum and maximum bases and the length
            // in 256-byte units.
            0x81 => {
                if raw.len() < 12 {
                    return Err(short);
                }
                out.push(fixed(
                    le(raw, 4, 2) << 8,
                    le(raw, 6, 2) << 8,
                    le(raw, 10, 2) << 8,
                ));
            }
            // 32-bit memory range.
            0x85 => {
                if raw.len() < 20 {
                    return Err(short);
                }
                out.push(fixed(le(raw, 4, 4), le(raw, 8, 4), le(raw, 16, 4)));
            }
            // 32-bit fixed memory range.
            0x86 => {
                if raw.len() < 12 {
                    return Err(short);
                }
                let base = le(raw, 4, 4);
                out.push(fixed(base, base, le(raw, 8, 4)));
            }
            // Word, DWord, QWord and Extended address space descriptors.
            0x87 | 0x88 | 0x8a | 0x8b => {
                let (width, first) = match tag {
                    0x88 => (2, 6),
                    0x87 => (4, 6),
                    0x8a => (8, 6),
                    _ => (8, 8),
                };
                if raw.len() < first + 5 * width {
                    return Err(short);
                }
                // Resource type 0 is memory.
                if raw[3] != 0 {
                    at += length;
                    continue;
                }
                let field = |n: usize| le(raw, first + n * width, width);
                let (minimum, maximum, translation, bytes) =
                    (field(1), field(2), field(3), field(4));
                let producer = raw[4] & 1 == 0;
                let mut claim = Vec::new();
                let end = maximum.max(minimum.saturating_add(bytes.saturating_sub(1)));
                claim.extend(inclusive(minimum, end));
                if translation != 0 {
                    claim.extend(inclusive(
                        minimum.wrapping_add(translation),
                        end.wrapping_add(translation),
                    ));
                }
                let window = (producer && bytes != 0)
                    .then(|| inclusive(minimum, maximum.min(minimum.saturating_add(bytes - 1))))
                    .flatten();
                out.push(AcpiMemory {
                    claim,
                    window,
                    translation,
                });
            }
            _ => {}
        }
        at += length;
    }
    Err(ScanError::Truncated { offset: at })
}

/// The ranges an ACPI table lists for hardware outside every PCI BAR:
/// remapping units' registers and the memory they reserve (DMAR, IVRS),
/// ECAM (MCFG), interrupt controllers (APIC) and the HPET. A table this
/// does not know, or a truncated entry, lists nothing more.
pub fn table_claims(table: &[u8]) -> Vec<Range<u64>> {
    let mut out = Vec::new();
    if table.len() < 36 {
        return out;
    }
    let length = (le(table, 4, 4) as usize).min(table.len());
    let table = &table[..length];
    let entries = |start: usize, each: &mut dyn FnMut(&[u8]) -> usize| {
        let mut at = start;
        while at + 4 <= table.len() {
            let step = each(&table[at..]);
            if step == 0 {
                break;
            }
            at += step;
        }
    };
    match &table[..4] {
        b"DMAR" => entries(48, &mut |entry: &[u8]| {
            let length = le(entry, 2, 2) as usize;
            if length < 4 || length > entry.len() {
                return 0;
            }
            match le(entry, 0, 2) {
                // DRHD: 2^size pages of registers, at least two pages.
                0 if length >= 16 => {
                    let pages = 1u64 << (entry[5] & 0xf);
                    let base = le(entry, 8, 8);
                    out.extend(inclusive(
                        base,
                        base.saturating_add(PAGE * pages.max(2) - 1),
                    ));
                }
                // RMRR: reserved memory, base to inclusive limit.
                1 if length >= 24 => out.extend(inclusive(le(entry, 8, 8), le(entry, 16, 8))),
                _ => {}
            }
            length
        }),
        b"IVRS" => entries(48, &mut |entry: &[u8]| {
            let length = le(entry, 2, 2) as usize;
            if length < 4 || length > entry.len() {
                return 0;
            }
            match entry[0] {
                // IVHD: the IOMMU's registers, 512 KiB with its counters.
                0x10 | 0x11 | 0x40 if length >= 16 => {
                    let base = le(entry, 8, 8);
                    out.extend(inclusive(base, base.saturating_add(0x7_ffff)));
                }
                // IVMD: memory it reserves.
                0x20..=0x22 if length >= 32 => {
                    let start = le(entry, 16, 8);
                    let bytes = le(entry, 24, 8);
                    if bytes != 0 {
                        out.extend(inclusive(start, start.saturating_add(bytes - 1)));
                    }
                }
                _ => {}
            }
            length
        }),
        b"MCFG" => {
            let mut at = 44;
            while at + 16 <= table.len() {
                let base = le(table, at, 8);
                let (first, last) = (u64::from(table[at + 10]), u64::from(table[at + 11]));
                if first <= last {
                    out.extend(inclusive(
                        base.saturating_add(first << 20),
                        base.saturating_add(((last + 1) << 20) - 1),
                    ));
                }
                at += 16;
            }
        }
        b"APIC" => {
            if table.len() >= 40 {
                let local = le(table, 36, 4);
                out.extend(inclusive(local, local + PAGE - 1));
            }
            entries(44, &mut |entry: &[u8]| {
                let length = usize::from(entry[1]);
                if length < 2 || length > entry.len() {
                    return 0;
                }
                match entry[0] {
                    // I/O APIC.
                    1 if length >= 12 => {
                        let base = le(entry, 4, 4);
                        out.extend(inclusive(base, base + PAGE - 1));
                    }
                    // Local APIC address override.
                    5 if length >= 12 => {
                        let base = le(entry, 4, 8);
                        out.extend(inclusive(base, base.saturating_add(PAGE - 1)));
                    }
                    _ => {}
                }
                length
            });
        }
        // The Event Timer Block's address, when it is in memory space.
        b"HPET" if table.len() >= 52 && table[40] == 0 => {
            let base = le(table, 44, 8);
            out.extend(inclusive(base, base.saturating_add(0x3ff)));
        }
        _ => {}
    }
    out
}

/// What the firmware's namespace and tables say about the memory around a
/// function to place: its root bridge's windows and everything else's.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Firmware {
    /// The root bridge's `_CRS` memory windows, untranslated.
    pub windows: Vec<Range<u64>>,
    /// Every other device's `_CRS` memory and the tables' ranges.
    pub claimed: Vec<Range<u64>>,
}

#[cfg(test)]
mod tests;
