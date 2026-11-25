//! The V2 Wi-Fi probe (`--v2-wifi-probe`): logging stubs for the J473AP
//! `apcie,t8112` PCIe root complex, its port 0 and the BCM4387 Wi-Fi
//! function behind it. Phase 0 of native Wi-Fi: it runs no firmware and
//! moves no frames. It records what macOS's PCIe and Broadcom drivers do
//! with the hardware, so the next phase models registers it has seen rather
//! than guessed.
//!
//! One model owns everything the probe places: the controller's register
//! banks, its ECAM configuration window and both PCI memory windows. Every
//! access is answered: a read gives what was last written there, or a
//! status the drivers wait for, and nothing is refused. Every access is
//! logged (offset, width, value) except BAR2 writes, which are a firmware
//! download and are summarized as extents. A read repeated with the same
//! value is one line with a count. An endpoint in a state the probe cannot
//! account for (a BAR outside the windows) goes absent: its configuration
//! space then reads all ones, as an empty slot does.
//!
//! Sources, all to be checked against the logs this produces:
//! - Linux `drivers/pci/controller/pcie-apple.c` (T8103/T6000): the root
//!   complex's PHY interface status `0x28` bit 4 (refclk), core status
//!   `0x58` bit 0 (ready), lane `0x84000 + 0x4000 * port` acknowledges
//!   bits 2-3 for requests 0-1, and each port's `0x80` LTSSM start, `0x208`
//!   bit 0 link up and `0x804` bit 0 ready, the port banks 16 MiB apart
//!   from the root complex. [INFERRED for T8112 macOS]
//! - Linux `brcmfmac` `pcie.c` and `chip.c`: BAR0 is a 4 KiB window onto the
//!   chip's backplane, moved by config `0x80`; the chip ID is ChipCommon's
//!   first word, its `0xfc` points at the EROM core table; BAR2 is TCM at
//!   backplane RAM addresses (`rambase` 0x740000 for the 4387); the ARM CR4
//!   runs once its wrapper's `IOCTL` (`0x408`) drops `CPUHALT` (0x20), and
//!   the firmware then writes the address of its `pcie_shared` structure
//!   into the last RAM word the host zeroed and polls. [SOURCED]
//! - The BCM4387's PCI identity 14e4:4433 (Asahi brcmfmac bindings), class
//!   0x028000. Its revision, BAR sizes, core layout and RAM size here are
//!   Stela's choice. [INFERRED]
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use sha2::Digest;
use stela_contracts::bus::{Access, Lifecycle, LifecycleState, MmioDevice, MmioError, Width};
use stela_soc_data::board::wifi_probe::Placement;

#[cfg(test)]
mod tests;

pub const NAME: &str = "wifi-probe";

/// The lines kept between two takes; past these each is only counted.
const PENDING: usize = 4096;
/// The accesses logged one by one in a boot; past these each is only
/// counted, as are the BAR2 writes.
const LOGGED: u64 = 200_000;

/// The root port's identity: Apple's T8103 root port, 106b:100c
/// [INFERRED for T8112].
const ROOT_ID: u32 = 0x100c_106b;
/// The BCM4387: Broadcom 14e4, device 4433.
const WIFI_ID: u32 = 0x4433_14e4;
/// Network controller, other (0x0280), revision 7 [INFERRED].
const WIFI_CLASS: u32 = 0x0280_0007;
/// BAR0's register window and BAR2's TCM [INFERRED sizes].
pub const BAR0_BYTES: u64 = 0x8000;
pub const BAR2_BYTES: u64 = 0x100_0000;
/// Config `0x80`: the backplane address BAR0's first 4 KiB show.
const BAR0_WINDOW: u16 = 0x80;
const WINDOW_BYTES: u32 = 0x1000;

/// The backplane the stub answers: core bases and IDs [INFERRED layout,
/// brcmfmac core IDs].
const CHIPCOMMON: u32 = 0x1800_0000;
const D11: u32 = 0x1800_1000;
const ARM_CR4: u32 = 0x1800_2000;
const PCIE2: u32 = 0x1800_3000;
const GCI: u32 = 0x1801_0000;
/// AXI wrappers sit 1 MiB above their core.
const WRAPPER: u32 = 0x10_0000;
/// The EROM core table ChipCommon's `0xfc` names.
const EROM: u32 = 0x1813_0000;
/// The ARM wrapper's `IOCTL` and its halt bit.
const ARM_IOCTL: u32 = ARM_CR4 + WRAPPER + 0x408;
const CPUHALT: u32 = 0x20;
/// The 4387's chip ID: ID 0x4387, revision 7, AXI (type 1), 5 cores.
const CHIP_ID: u32 = 0x4387 | 7 << 16 | 5 << 24 | 1 << 28;
/// The 4387's backplane RAM base (brcmfmac `brcmf_chip_tcm_rambase`) and
/// a RAM of two 960 KiB banks [INFERRED].
pub const RAM_BASE: u32 = 0x74_0000;
pub const RAM_BYTES: u32 = 0x1e_0000;
/// The last RAM word, where the host waits for the `pcie_shared` address.
pub const SHARED_WORD: u32 = RAM_BASE + RAM_BYTES - 4;

/// Each core: ID, revision, base, whether it masters the bus.
const CORES: [(u32, u32, u32, bool); 5] = [
    (0x800, 0x45, CHIPCOMMON, false),
    (0x812, 0x40, D11, true),
    (0x83e, 0x07, ARM_CR4, true),
    (0x83c, 0x42, PCIE2, true),
    (0x840, 0x0f, GCI, false),
];

/// The EROM table for [`CORES`], in the descriptor format brcmfmac's
/// `brcmf_chip_dmp_erom_scan` reads.
fn erom() -> Vec<u32> {
    const BROADCOM: u32 = 0x4bf;
    let mut table = Vec::new();
    for (unit, &(id, revision, base, master)) in CORES.iter().enumerate() {
        table.push(BROADCOM << 20 | id << 8 | 1);
        let (wrappers, ports) = if master {
            (1 << 14, 1 << 4)
        } else {
            (1 << 19, 0)
        };
        table.push(revision << 24 | wrappers | 1 << 9 | ports | 1);
        if master {
            table.push((unit as u32) << 8 | 3);
        }
        table.push(base | 5);
        let wrapper_type = if master { 3 } else { 2 };
        table.push((base + WRAPPER) | wrapper_type << 6 | 5);
    }
    table.push(0xf);
    table
}

/// What one access was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Event {
    write: bool,
    physical: u64,
    width: u8,
    value: u64,
}

/// The probe's log: lines waiting for the boot's take, with repeats folded.
#[derive(Default)]
struct Log {
    lines: Vec<String>,
    last: Option<(Event, String, u64)>,
    logged: u64,
    counted: u64,
    dropped: u64,
}

impl Log {
    fn line(&mut self, line: String) {
        if self.lines.len() < PENDING {
            self.lines.push(line);
        } else {
            self.dropped += 1;
        }
    }

    fn flush(&mut self) {
        if let Some((event, place, count)) = self.last.take() {
            let kind = if event.write { 'W' } else { 'R' };
            let repeat = if count > 1 {
                format!(" x{count}")
            } else {
                String::new()
            };
            self.line(format!(
                "{kind} {place} pa={:#x} w{} v={:#x}{repeat}",
                event.physical, event.width, event.value
            ));
        }
    }

    /// Whether `event` needs no line of its own: it repeats the last one,
    /// whose count it adds to, or the log is full and only counts.
    fn folded(&mut self, event: Event) -> bool {
        if self.logged >= LOGGED {
            self.counted += 1;
            return true;
        }
        if let Some((last, _, count)) = &mut self.last
            && *last == event
        {
            *count += 1;
            return true;
        }
        false
    }

    /// Start a line for `event` at `place`.
    fn access(&mut self, event: Event, place: String) {
        self.flush();
        self.logged += 1;
        self.last = Some((event, place, 1));
    }

    fn take(&mut self) -> Vec<String> {
        self.flush();
        core::mem::take(&mut self.lines)
    }
}

/// One function's 4 KiB configuration space.
struct Config {
    words: Box<[u32; 1024]>,
    /// Dwords software cannot change.
    fixed: Vec<u16>,
}

impl Config {
    fn new(initial: &[(u16, u32)], fixed: &[u16]) -> Self {
        let mut words = Box::new([0u32; 1024]);
        for &(at, value) in initial {
            words[usize::from(at / 4)] = value;
        }
        Self {
            words,
            fixed: fixed.to_vec(),
        }
    }
    fn word(&self, at: u16) -> u32 {
        self.words[usize::from(at / 4) & 0x3ff]
    }
    fn set(&mut self, at: u16, value: u32) {
        self.words[usize::from(at / 4) & 0x3ff] = value;
    }
}

/// Reads `width` bytes at byte `at` of a little-endian dword.
fn lane(word: u32, at: u64, width: Width) -> u64 {
    (u64::from(word) >> (8 * (at & 3))) & width.mask()
}

/// `word` with `width` bytes at byte `at` replaced by `value`.
fn merge(word: u32, at: u64, width: Width, value: u64) -> u32 {
    let shift = 8 * (at & 3);
    let mask = (width.mask() << shift) as u32;
    (word & !mask) | ((value << shift) as u32 & mask)
}

/// The TCM the host writes through BAR2, kept by page.
#[derive(Default)]
struct Tcm {
    pages: BTreeMap<u32, Box<[u8; 4096]>>,
    /// Written byte ranges, merged, sorted.
    extents: Vec<(u32, u32)>,
    written: u64,
}

impl Tcm {
    fn write(&mut self, at: u32, bytes: &[u8]) {
        for (index, &byte) in bytes.iter().enumerate() {
            let address = at.wrapping_add(index as u32);
            let page = self
                .pages
                .entry(address >> 12)
                .or_insert_with(|| Box::new([0; 4096]));
            page[(address & 0xfff) as usize] = byte;
        }
        self.written += bytes.len() as u64;
        let (start, end) = (at, at.saturating_add(bytes.len() as u32));
        let mut merged = (start, end);
        self.extents.retain(|&(s, e)| {
            if s <= merged.1 && merged.0 <= e {
                merged = (merged.0.min(s), merged.1.max(e));
                false
            } else {
                true
            }
        });
        let index = self.extents.partition_point(|&(s, _)| s < merged.0);
        self.extents.insert(index, merged);
    }

    fn read(&self, at: u32, out: &mut [u8]) {
        for (index, slot) in out.iter_mut().enumerate() {
            let address = at.wrapping_add(index as u32);
            *slot = self
                .pages
                .get(&(address >> 12))
                .map_or(0, |page| page[(address & 0xfff) as usize]);
        }
    }

    fn bytes(&self, start: u32, end: u32) -> Vec<u8> {
        let mut out = alloc::vec![0; (end - start) as usize];
        self.read(start, &mut out);
        out
    }
}

/// The BCM4387 function: configuration space, BAR0's backplane and BAR2's
/// TCM.
struct Endpoint {
    config: Config,
    backplane: BTreeMap<u32, u32>,
    erom: Vec<u32>,
    tcm: Tcm,
    halted: bool,
    released: bool,
    /// Why the function went absent, once it has.
    absent: Option<&'static str>,
    /// BARs reported assigned, as PCI addresses.
    reported: [Option<u64>; 2],
}

impl Endpoint {
    fn new() -> Self {
        let config = Config::new(
            &[
                (0x00, WIFI_ID),
                (0x04, 0x0010_0000),
                (0x08, WIFI_CLASS),
                (0x10, 0x4),
                (0x18, 0x4),
                (0x2c, 0x0000_106b),
                (0x34, 0x48),
                (0x3c, 0x0000_0100),
                // Power management, then MSI (64-bit), then PCIe (endpoint,
                // Gen2 x1, ASPM L0s/L1): brcmfmac's PM_CSR 0x4c, MSI_CAP
                // 0x58 and LINK_STATUS_CTRL 0xbc.
                (0x48, 0x0003_5801),
                (0x58, 0x0080_ac05),
                (0xac, 0x0002_0010),
                (0xb0, 0x0000_8001),
                (0xb8, 0x0000_0c12),
                (0xbc, 0x0012_0000),
                (BAR0_WINDOW, CHIPCOMMON),
            ],
            &[
                0x00, 0x08, 0x0c, 0x20, 0x24, 0x2c, 0x30, 0x34, 0x48, 0x58, 0xac, 0xb0, 0xb8,
            ],
        );
        Self {
            config,
            backplane: BTreeMap::new(),
            erom: erom(),
            tcm: Tcm::default(),
            halted: false,
            released: false,
            absent: None,
            reported: [None; 2],
        }
    }

    /// The backplane address BAR0's first 4 KiB show.
    fn window(&self) -> u32 {
        self.config.word(BAR0_WINDOW) & !(WINDOW_BYTES - 1)
    }

    fn memory_enabled(&self) -> bool {
        self.config.word(0x04) & 2 != 0
    }

    /// BAR `index` (0 or 2)'s PCI address and size.
    fn bar(&self, index: usize) -> (u64, u64) {
        let at = 0x10 + 4 * index as u16;
        let low = u64::from(self.config.word(at) & !0xf);
        let high = u64::from(self.config.word(at + 4));
        let bytes = if index == 0 { BAR0_BYTES } else { BAR2_BYTES };
        (high << 32 | low, bytes)
    }

    fn config_read(&self, at: u16) -> u32 {
        if self.absent.is_some() {
            return u32::MAX;
        }
        let word = self.config.word(at);
        if at & !3 == 0xbc {
            // Link status: Gen2 x1, link up; control as written.
            return 0x0012_0000 | (word & 0xffff);
        }
        word
    }

    fn config_write(&mut self, at: u16, word: u32) {
        let at = at & !3;
        if self.absent.is_some() || self.config.fixed.contains(&at) {
            return;
        }
        let word = match at {
            // Status bits are RO here; only the command word moves.
            0x04 => 0x0010_0000 | (word & 0xffff),
            0x10 => (word & !(BAR0_BYTES as u32 - 1) & !0xf) | 0x4,
            0x18 => (word & !(BAR2_BYTES as u32 - 1) & !0xf) | 0x4,
            0x3c => 0x0000_0100 | (word & 0xff),
            _ => word,
        };
        self.config.set(at, word);
    }

    /// The backplane word at `address`, as BAR0 shows it.
    fn backplane_read(&self, address: u32) -> u32 {
        let address = address & !3;
        match address {
            CHIPCOMMON => CHIP_ID,
            a if a == CHIPCOMMON + 0xfc => EROM,
            a if (EROM..EROM + 4 * self.erom.len() as u32).contains(&a) => {
                self.erom[((a - EROM) / 4) as usize]
            }
            // ARM CR4 capabilities: two TCM banks of type A; each bank's
            // info gives (size + 1) * 8 KiB = 960 KiB.
            a if a == ARM_CR4 + 0x04 => 2,
            a if a == ARM_CR4 + 0x44 => 0x77,
            a => self.backplane.get(&a).copied().unwrap_or(0),
        }
    }
}

/// What a guest PA reaches.
enum Place {
    /// A controller bank: its `reg` index and guest PA.
    Controller(usize, u64),
    Config {
        bus: u32,
        device: u32,
        function: u32,
        at: u16,
    },
    Bar0(u64),
    Bar2(u64),
    Window,
}

/// The probe's model.
pub struct WifiProbe {
    placement: Placement,
    controller: BTreeMap<u64, u32>,
    root: Config,
    endpoint: Endpoint,
    log: Log,
    lifecycle: Option<LifecycleState>,
    /// The RAM word the host polls, read since the ARM's release.
    polls: u64,
}

impl WifiProbe {
    pub fn new(placement: Placement) -> Self {
        let root = Config::new(
            &[
                (0x00, ROOT_ID),
                (0x04, 0x0010_0000),
                (0x08, 0x0604_0001),
                (0x0c, 0x0001_0000),
                (0x34, 0x40),
                // Power management, MSI, then PCIe (root port, Gen2 x1,
                // link active reporting).
                (0x40, 0x0003_5001),
                (0x50, 0x0080_7005),
                (0x70, 0x0142_0010),
                (0x74, 0x0000_0001),
                (0x7c, 0x0010_0012),
            ],
            &[
                0x00, 0x08, 0x0c, 0x10, 0x14, 0x34, 0x38, 0x40, 0x50, 0x70, 0x74, 0x7c,
            ],
        );
        Self {
            placement,
            controller: BTreeMap::new(),
            root,
            endpoint: Endpoint::new(),
            log: Log::default(),
            lifecycle: None,
            polls: 0,
        }
    }

    pub fn placement(&self) -> &Placement {
        &self.placement
    }

    /// The lines since the last take, oldest first, without a prefix.
    pub fn take_lines(&mut self) -> Vec<String> {
        self.log.take()
    }

    /// One line describing what the probe saw in total.
    pub fn summary(&self) -> String {
        let (bar0, _) = self.endpoint.bar(0);
        let (bar2, _) = self.endpoint.bar(2);
        format!(
            "SUMMARY logged={} counted={} dropped={} root-secondary-bus={} endpoint={} \
             bar0=pci:{bar0:#x} bar2=pci:{bar2:#x} memory={} tcm-written={} tcm-extents={} \
             arm-halted={} arm-released={} shared-word-polls={}",
            self.log.logged,
            self.log.counted,
            self.log.dropped,
            self.secondary_bus(),
            self.endpoint.absent.unwrap_or("present"),
            self.endpoint.memory_enabled(),
            self.endpoint.tcm.written,
            self.endpoint.tcm.extents.len(),
            self.endpoint.halted,
            self.endpoint.released,
            self.polls,
        )
    }

    fn secondary_bus(&self) -> u32 {
        (self.root.word(0x18) >> 8) & 0xff
    }

    fn event(&mut self, line: String) {
        self.log.flush();
        self.log.line(format!("EVENT {line}"));
    }

    fn place(&self, physical: u64) -> Place {
        let (ecam, ecam_bytes) = self.placement.config;
        if (ecam..ecam + ecam_bytes).contains(&physical) {
            let offset = physical - ecam;
            return Place::Config {
                bus: (offset >> 20) as u32 & 0xff,
                device: (offset >> 15) as u32 & 0x1f,
                function: (offset >> 12) as u32 & 7,
                at: (offset & 0xfff) as u16,
            };
        }
        if self.endpoint.absent.is_none() && self.endpoint.memory_enabled() {
            for index in [0, 2] {
                let (pci, bytes) = self.endpoint.bar(index);
                if let Some(start) = self.placement.to_physical(pci, bytes)
                    && (start..start + bytes).contains(&physical)
                {
                    let offset = physical - start;
                    return if index == 0 {
                        Place::Bar0(offset)
                    } else {
                        Place::Bar2(offset)
                    };
                }
            }
        }
        if let Some(&(index, start, _)) = self
            .placement
            .registers
            .iter()
            .skip(1)
            .filter(|&&(_, start, bytes)| (start..start + bytes).contains(&physical))
            .min_by_key(|&&(_, _, bytes)| bytes)
        {
            return Place::Controller(index, start);
        }
        Place::Window
    }

    fn describe(&self, physical: u64) -> String {
        match self.place(physical) {
            Place::Controller(index, start) => {
                format!("apcie.reg[{index}]+{:#x}", physical - start)
            }
            Place::Config {
                bus,
                device,
                function,
                at,
            } => format!("cfg {bus:02x}:{device:02x}.{function}+{at:#05x}"),
            Place::Bar0(offset) => {
                let window = self.endpoint.window();
                match offset {
                    0..0x1000 => format!(
                        "bar0+{offset:#x} bp={:#x}",
                        window.wrapping_add(offset as u32)
                    ),
                    0x1000..0x2000 => format!(
                        "bar0+{offset:#x} bp-wrapper={:#x}",
                        window.wrapping_add(WRAPPER + (offset as u32 - 0x1000))
                    ),
                    0x2000..0x3000 => {
                        format!(
                            "bar0+{offset:#x} bp={:#x}",
                            PCIE2 + (offset as u32 - 0x2000)
                        )
                    }
                    _ => format!("bar0+{offset:#x}"),
                }
            }
            Place::Bar2(offset) => format!("bar2+{offset:#x}"),
            Place::Window => String::from("window"),
        }
    }

    /// The function at `bus`/`device`/`function` and whether it is the
    /// endpoint: `None` for an empty slot.
    fn function(&self, bus: u32, device: u32, function: u32) -> Option<bool> {
        let root_bus = self.placement.buses.0;
        if bus == root_bus {
            return (device == 0 && function == 0).then_some(false);
        }
        let secondary = self.secondary_bus();
        (secondary != root_bus && bus == secondary && device == 0 && function == 0).then_some(true)
    }

    fn config_read(&self, bus: u32, device: u32, function: u32, at: u16) -> u32 {
        match self.function(bus, device, function) {
            None => u32::MAX,
            Some(false) => {
                let word = self.root.word(at);
                if at & !3 == 0x80 {
                    // Link status: Gen2 x1, data link layer active once
                    // the port's LTSSM runs; control as written.
                    let up = if self.link_up() { 0x2012 } else { 0 };
                    up << 16 | (word & 0xffff)
                } else {
                    word
                }
            }
            Some(true) => self.endpoint.config_read(at),
        }
    }

    fn config_write(
        &mut self,
        bus: u32,
        device: u32,
        function: u32,
        at: u16,
        width: Width,
        value: u64,
    ) {
        match self.function(bus, device, function) {
            None => {}
            Some(false) => {
                let word_at = at & !3;
                if !self.root.fixed.contains(&word_at) {
                    let word = merge(self.root.word(word_at), u64::from(at), width, value);
                    self.root.set(word_at, word);
                }
            }
            Some(true) => {
                let enabled = self.endpoint.memory_enabled();
                let word = merge(
                    self.endpoint.config.word(at & !3),
                    u64::from(at),
                    width,
                    value,
                );
                self.endpoint.config_write(at, word);
                self.check_bars(enabled);
            }
        }
    }

    /// Report each BAR the host has placed while memory decoding is on.
    /// One outside every window when decoding turns on makes the endpoint
    /// absent: no access could reach it. A BAR moved while decoding stays
    /// on (sizing with decoding left on) is only reported.
    fn check_bars(&mut self, was_enabled: bool) {
        if self.endpoint.absent.is_some() || !self.endpoint.memory_enabled() {
            return;
        }
        if !was_enabled {
            self.event(String::from("endpoint memory decoding on"));
        }
        for (slot, index) in [0usize, 2].into_iter().enumerate() {
            let (pci, bytes) = self.endpoint.bar(index);
            if self.endpoint.reported[slot] == Some(pci) {
                continue;
            }
            self.endpoint.reported[slot] = Some(pci);
            match self.placement.to_physical(pci, bytes) {
                Some(physical) => self.event(format!(
                    "BAR{index} pci={pci:#x} pa={physical:#x} bytes={bytes:#x}"
                )),
                None if !was_enabled => {
                    self.endpoint.absent = Some("a BAR outside the controller's windows");
                    self.event(format!(
                        "BAR{index} pci={pci:#x} bytes={bytes:#x} is outside every window: \
                         endpoint absent"
                    ));
                }
                None => self.event(format!(
                    "BAR{index} pci={pci:#x} bytes={bytes:#x} is outside every window \
                     while decoding is on"
                )),
            }
        }
    }

    /// Port 0's LTSSM was started (`0x80` bit 0).
    fn link_up(&self) -> bool {
        self.port(0)
            .and_then(|base| self.controller.get(&(base + 0x80)))
            .is_some_and(|word| word & 1 != 0)
    }

    /// The root complex bank (`reg[1]`).
    fn rc(&self) -> Option<u64> {
        self.placement.registers.get(1).map(|&(_, start, _)| start)
    }

    fn port(&self, port: u64) -> Option<u64> {
        self.rc().map(|rc| rc + 0x0100_0000 * (port + 1))
    }

    /// A controller word: what was written, with the status bits the
    /// drivers wait for.
    fn controller_read(&self, word: u64) -> u32 {
        let stored = self.controller.get(&word).copied().unwrap_or(0);
        let Some(rc) = self.rc() else {
            return stored;
        };
        if word == rc + 0x28 {
            return stored | 1 << 4;
        }
        if word == rc + 0x58 {
            return stored | 1;
        }
        for port in 0..u64::from(self.placement.ports) {
            if word == rc + 0x84000 + 0x4000 * port {
                return stored | (stored & 3) << 2;
            }
            let base = self.port(port).expect("rc placed");
            if word == base + 0x208 && port == 0 && self.link_up() {
                return stored | 1;
            }
            if word == base + 0x804 {
                return stored | 1;
            }
        }
        stored
    }

    fn bar0_read(&self, offset: u64) -> u32 {
        let window = self.endpoint.window();
        match offset {
            0..0x1000 => self
                .endpoint
                .backplane_read(window.wrapping_add(offset as u32)),
            0x1000..0x2000 => self
                .endpoint
                .backplane_read(window.wrapping_add(WRAPPER + (offset as u32 - 0x1000))),
            0x2000..0x3000 => self
                .endpoint
                .backplane_read(PCIE2 + (offset as u32 - 0x2000)),
            _ => self
                .endpoint
                .backplane
                .get(&(0xffff_0000 | offset as u32 & !3))
                .copied()
                .unwrap_or(0),
        }
    }

    fn bar0_write(&mut self, offset: u64, width: Width, value: u64) {
        let window = self.endpoint.window();
        let address = match offset {
            0..0x1000 => window.wrapping_add(offset as u32),
            0x1000..0x2000 => window.wrapping_add(WRAPPER + (offset as u32 - 0x1000)),
            0x2000..0x3000 => PCIE2 + (offset as u32 - 0x2000),
            _ => 0xffff_0000 | offset as u32,
        };
        let word = address & !3;
        let old = self.endpoint.backplane.get(&word).copied().unwrap_or(0);
        let new = merge(old, u64::from(address), width, value);
        self.endpoint.backplane.insert(word, new);
        if word == ARM_IOCTL {
            if new & CPUHALT != 0 && !self.endpoint.halted {
                self.endpoint.halted = true;
                self.event(String::from(
                    "ARM CR4 halted (IOCTL CPUHALT set): download state",
                ));
            } else if new & CPUHALT == 0 && self.endpoint.halted && !self.endpoint.released {
                self.endpoint.released = true;
                self.released();
            }
        }
    }

    /// The ARM was let run: what the host downloaded, and where it waits.
    fn released(&mut self) {
        self.event(format!(
            "ARM CR4 released: the host now polls TCM {SHARED_WORD:#x} (bar2+{SHARED_WORD:#x}) \
             for the pcie_shared address; no firmware runs, so it stays as written"
        ));
        let extents = self.endpoint.tcm.extents.clone();
        self.event(format!(
            "TCM download bytes={} extents={}",
            self.endpoint.tcm.written,
            extents.len()
        ));
        for &(start, end) in extents.iter().take(32) {
            let bytes = self.endpoint.tcm.bytes(start, end);
            let digest = sha2::Sha256::digest(&bytes);
            let head: String = bytes
                .iter()
                .take(48)
                .map(|&b| {
                    if (0x20..0x7f).contains(&b) {
                        b as char
                    } else {
                        '.'
                    }
                })
                .collect();
            self.event(format!(
                "TCM extent {start:#x}..{end:#x} bytes={} sha256={} head=\"{head}\"",
                end - start,
                hex::encode(digest)
            ));
        }
        let mut word = [0u8; 4];
        self.endpoint.tcm.read(SHARED_WORD, &mut word);
        self.event(format!(
            "TCM last RAM word {SHARED_WORD:#x} = {:#x} at release",
            u32::from_le_bytes(word)
        ));
    }

    fn bar2_read(&mut self, offset: u64, width: Width) -> u64 {
        let mut bytes = [0u8; 8];
        let n = width.bytes() as usize;
        self.endpoint.tcm.read(offset as u32, &mut bytes[..n]);
        if self.endpoint.released && offset as u32 & !3 == SHARED_WORD {
            self.polls += 1;
        }
        u64::from_le_bytes(bytes)
    }

    fn bar2_write(&mut self, offset: u64, width: Width, value: u64) {
        let n = width.bytes() as usize;
        let bytes = value.to_le_bytes();
        self.endpoint.tcm.write(offset as u32, &bytes[..n]);
        if offset as u32 & !3 == SHARED_WORD {
            self.event(format!(
                "TCM last RAM word {SHARED_WORD:#x} written {value:#x}"
            ));
        }
    }

    fn access(&mut self, physical: u64, width: Width, write: Option<u64>) -> u64 {
        let place = self.place(physical);
        let value = match (&place, write) {
            (
                Place::Config {
                    bus,
                    device,
                    function,
                    at,
                },
                None,
            ) => {
                let word = self.config_read(*bus, *device, *function, *at & !3);
                lane(word, u64::from(*at), width)
            }
            (
                Place::Config {
                    bus,
                    device,
                    function,
                    at,
                },
                Some(value),
            ) => {
                if width == Width::Double {
                    self.config_write(
                        *bus,
                        *device,
                        *function,
                        *at,
                        Width::Word,
                        value & 0xffff_ffff,
                    );
                    self.config_write(*bus, *device, *function, *at + 4, Width::Word, value >> 32);
                } else {
                    self.config_write(*bus, *device, *function, *at, width, value);
                }
                value
            }
            (Place::Controller(..), None) => {
                let low = self.controller_read(physical & !3);
                if width == Width::Double {
                    u64::from(low) | u64::from(self.controller_read((physical & !3) + 4)) << 32
                } else {
                    lane(low, physical, width)
                }
            }
            (Place::Controller(..), Some(value)) => {
                let word = physical & !3;
                if width == Width::Double {
                    self.controller.insert(word, value as u32);
                    self.controller.insert(word + 4, (value >> 32) as u32);
                } else {
                    let old = self.controller.get(&word).copied().unwrap_or(0);
                    self.controller
                        .insert(word, merge(old, physical, width, value));
                }
                value
            }
            (Place::Bar0(offset), None) => {
                let low = self.bar0_read(*offset & !3);
                if width == Width::Double {
                    u64::from(low) | u64::from(self.bar0_read((*offset & !3) + 4)) << 32
                } else {
                    lane(low, *offset, width)
                }
            }
            (Place::Bar0(offset), Some(value)) => {
                if width == Width::Double {
                    self.bar0_write(*offset, Width::Word, value & 0xffff_ffff);
                    self.bar0_write(*offset + 4, Width::Word, value >> 32);
                } else {
                    self.bar0_write(*offset, width, value);
                }
                value
            }
            (Place::Bar2(offset), None) => self.bar2_read(*offset, width),
            (Place::Bar2(offset), Some(value)) => {
                // A download: summarized at the ARM's release, not logged.
                self.bar2_write(*offset, width, value);
                return value;
            }
            // An unclaimed PCI address: a read completes with all ones.
            (Place::Window, None) => width.mask(),
            (Place::Window, Some(value)) => value,
        };
        let event = Event {
            write: write.is_some(),
            physical,
            width: width.bytes() as u8,
            value,
        };
        // Name the place only for a line that is not a repeat.
        if !self.log.folded(event) {
            let name = self.describe(physical);
            self.log.access(event, name);
        }
        value
    }
}

impl Lifecycle for WifiProbe {
    fn check_lifecycle(&self, _: LifecycleState) -> crate::Result<()> {
        Ok(())
    }
    fn set_lifecycle(&mut self, proposed: LifecycleState, _: &mut Access<'_>) {
        if self.lifecycle != Some(proposed) {
            self.lifecycle = Some(proposed);
            self.event(format!(
                "controller lifecycle powered={} clocked={} quiesced={} reset={} retention={}",
                proposed.powered,
                proposed.clocked,
                proposed.quiesced,
                proposed.reset,
                proposed.retention
            ));
        }
    }
}

impl MmioDevice for WifiProbe {
    fn advance_policy(&self) -> stela_contracts::bus::AdvancePolicy {
        stela_contracts::bus::AdvancePolicy::Passive
    }
    fn lifecycle(&mut self) -> Option<&mut dyn Lifecycle> {
        Some(self)
    }
    fn name(&self) -> &'static str {
        NAME
    }
    fn read(&mut self, offset: u64, width: Width, _: &mut Access<'_>) -> Result<u64, MmioError> {
        Ok(self.access(offset, width, None))
    }
    fn write(
        &mut self,
        offset: u64,
        width: Width,
        value: u64,
        _: &mut Access<'_>,
    ) -> Result<(), MmioError> {
        self.access(offset, width, Some(value & width.mask()));
        Ok(())
    }
}
