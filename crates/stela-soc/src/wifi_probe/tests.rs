use super::*;
use alloc::vec;
use stela_contracts::interrupt::InterruptLatch;
use stela_soc_data::board::wifi_probe::Window;

const ECAM: u64 = 0x6_9000_0000;
const RC: u64 = 0x6_8000_0000;
const PORT0: u64 = 0x6_8100_0000;
/// The 32-bit window: PCI 0xc0000000 at guest PA 0x6c0000000.
const PCI: u64 = 0xc000_0000;
const WINDOW: u64 = 0x6_c000_0000;

fn placement() -> Placement {
    Placement {
        registers: vec![
            (0, ECAM, 0x1000_0000),
            (1, RC, 0x4000),
            (2, RC + 0x8_0000, 0x9_0000),
            (6, PORT0, 0x8000),
        ],
        config: (ECAM, 0x1000_0000),
        buses: (0, 8),
        windows: vec![
            Window {
                space: 0x4300_0000,
                pci: 0x6_a000_0000,
                physical: 0x6_a000_0000,
                bytes: 0x2000_0000,
            },
            Window {
                space: 0x0200_0000,
                pci: PCI,
                physical: WINDOW,
                bytes: 0x4000_0000,
            },
        ],
        ports: 4,
        interrupts: vec![0x30d],
    }
}

struct Probe {
    model: WifiProbe,
    latch: InterruptLatch,
}

impl Probe {
    fn new() -> Self {
        Self {
            model: WifiProbe::new(placement()),
            latch: InterruptLatch::new(),
        }
    }
    fn read(&mut self, pa: u64, width: Width) -> u64 {
        let mut access = Access::new(0, 0, &mut self.latch);
        self.model.read(pa, width, &mut access).unwrap()
    }
    fn write(&mut self, pa: u64, width: Width, value: u64) {
        let mut access = Access::new(0, 0, &mut self.latch);
        self.model.write(pa, width, value, &mut access).unwrap();
    }
    fn cfg(bus: u64, device: u64, function: u64, at: u64) -> u64 {
        ECAM + (bus << 20 | device << 15 | function << 12 | at)
    }
    fn cfg_read(&mut self, bus: u64, at: u64) -> u32 {
        self.read(Self::cfg(bus, 0, 0, at), Width::Word) as u32
    }
    fn cfg_write(&mut self, bus: u64, at: u64, value: u32) {
        self.write(Self::cfg(bus, 0, 0, at), Width::Word, u64::from(value));
    }
    /// Root port 0 forwards bus 1, and the endpoint's BAR0 and BAR2 sit at
    /// the window's start, decoding on.
    fn enumerate(&mut self) {
        self.cfg_write(0, 0x18, 0x0001_0100);
        self.cfg_write(1, 0x10, PCI as u32);
        self.cfg_write(1, 0x14, 0);
        self.cfg_write(1, 0x18, (PCI + 0x100_0000) as u32);
        self.cfg_write(1, 0x1c, 0);
        self.cfg_write(1, 0x04, 0x0006);
    }
    fn bar0(&mut self, offset: u64) -> u32 {
        self.read(WINDOW + offset, Width::Word) as u32
    }
    fn lines(&mut self) -> Vec<String> {
        self.model.take_lines()
    }
}

#[test]
fn configuration_space_presents_the_root_port_and_the_bcm4387_behind_it() {
    let mut probe = Probe::new();
    assert_eq!(probe.cfg_read(0, 0x00), 0x100c_106b);
    assert_eq!(probe.cfg_read(0, 0x08) >> 8, 0x06_0400);
    assert_eq!(probe.cfg_read(0, 0x0c) >> 16 & 0x7f, 1, "type 1 header");
    // The other root ports' slots and any bus before enumeration are empty.
    assert_eq!(probe.read(Probe::cfg(0, 1, 0, 0), Width::Word), 0xffff_ffff);
    assert_eq!(probe.cfg_read(1, 0x00), 0xffff_ffff);
    probe.cfg_write(0, 0x18, 0x0001_0100);
    assert_eq!(probe.cfg_read(0, 0x18), 0x0001_0100);
    assert_eq!(probe.cfg_read(1, 0x00), 0x4433_14e4);
    assert_eq!(probe.read(Probe::cfg(1, 0, 0, 2), Width::Half), 0x4433);
    assert_eq!(probe.cfg_read(1, 0x08), 0x0280_0007);
    assert_eq!(probe.cfg_read(1, 0x0c) >> 16 & 0xff, 0, "single function");
    // Function 1 (Bluetooth) stays absent.
    assert_eq!(probe.read(Probe::cfg(1, 0, 1, 0), Width::Word), 0xffff_ffff);
    // Capability chain: PM 0x48, MSI 0x58, PCIe 0xac.
    assert_eq!(probe.cfg_read(1, 0x34) & 0xff, 0x48);
    assert_eq!(probe.cfg_read(1, 0x48) & 0xffff, 0x5801);
    assert_eq!(probe.cfg_read(1, 0x58) & 0xffff, 0xac05);
    assert_eq!(probe.cfg_read(1, 0xac) & 0xff, 0x10);
    // Identity words do not move.
    probe.cfg_write(1, 0x00, 0);
    assert_eq!(probe.cfg_read(1, 0x00), 0x4433_14e4);
}

#[test]
fn bars_size_as_64_bit_memory_and_root_port_has_none() {
    let mut probe = Probe::new();
    probe.cfg_write(0, 0x18, 0x0001_0100);
    probe.cfg_write(1, 0x10, u32::MAX);
    probe.cfg_write(1, 0x14, u32::MAX);
    assert_eq!(probe.cfg_read(1, 0x10), 0xffff_8004);
    assert_eq!(probe.cfg_read(1, 0x14), u32::MAX);
    probe.cfg_write(1, 0x18, u32::MAX);
    assert_eq!(probe.cfg_read(1, 0x18), 0xff00_0004);
    probe.cfg_write(1, 0x20, u32::MAX);
    assert_eq!(probe.cfg_read(1, 0x20), 0, "BAR4 unimplemented");
    probe.cfg_write(0, 0x10, u32::MAX);
    assert_eq!(probe.cfg_read(0, 0x10), 0, "the root port has no BAR");
}

#[test]
fn bar0_shows_the_chip_id_and_erom_through_its_backplane_window() {
    let mut probe = Probe::new();
    probe.enumerate();
    let lines = probe.lines();
    assert!(
        lines
            .iter()
            .any(|l| l == "EVENT BAR0 pci=0xc0000000 pa=0x6c0000000 bytes=0x8000"),
        "{lines:#?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l == "EVENT BAR2 pci=0xc1000000 pa=0x6c1000000 bytes=0x1000000"),
        "{lines:#?}"
    );
    assert_eq!(probe.bar0(0), 0x1500_4387 | 7 << 16);
    assert_eq!(probe.bar0(0) & 0xffff, 0x4387);
    let erom = probe.bar0(0xfc);
    // Walk the EROM through the window, as brcmfmac does: ChipCommon first.
    probe.cfg_write(1, 0x80, erom);
    let cia = probe.bar0(0);
    assert_eq!(cia & 0xf, 1, "component descriptor");
    assert_eq!(cia >> 8 & 0xfff, 0x800, "ChipCommon");
    let mut cores = Vec::new();
    let mut at = 0;
    loop {
        let word = probe.bar0(at);
        if word == 0xf {
            break;
        }
        if word & 0xf == 1 && word >> 20 == 0x4bf {
            cores.push(word >> 8 & 0xfff);
        }
        at += 4;
        assert!(at < 0x1000);
    }
    assert_eq!(cores, [0x800, 0x812, 0x83e, 0x83c, 0x840]);
    let lines = probe.lines();
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("R bar0+0x0 bp=0x18000000 pa=0x6c0000000 w4 v=0x15074387")),
        "{lines:#?}"
    );
}

#[test]
fn every_access_is_logged_with_offset_width_and_value_and_repeats_fold() {
    let mut probe = Probe::new();
    probe.write(RC + 0x50, Width::Word, 1);
    for _ in 0..3 {
        assert_eq!(probe.read(RC + 0x58, Width::Word), 1, "core ready");
    }
    probe.write(PORT0 + 0x10, Width::Byte, 0x1ab);
    assert_eq!(probe.read(PORT0 + 0x10, Width::Word), 0xab);
    assert_eq!(probe.read(PORT0 + 0x11, Width::Byte), 0);
    let lines = probe.lines();
    assert_eq!(
        lines,
        vec![
            "W apcie.reg[1]+0x50 pa=0x680000050 w4 v=0x1",
            "R apcie.reg[1]+0x58 pa=0x680000058 w4 v=0x1 x3",
            "W apcie.reg[6]+0x10 pa=0x681000010 w1 v=0xab",
            "R apcie.reg[6]+0x10 pa=0x681000010 w4 v=0xab",
            "R apcie.reg[6]+0x11 pa=0x681000011 w1 v=0x0",
        ]
    );
    // A read of the window before any BAR completes with all ones.
    assert_eq!(probe.read(WINDOW, Width::Word), 0xffff_ffff);
    assert_eq!(probe.lines(), ["R window pa=0x6c0000000 w4 v=0xffffffff"]);
}

#[test]
fn port_zero_trains_its_link_only_once_started() {
    let mut probe = Probe::new();
    assert_eq!(probe.read(RC + 0x28, Width::Word) & 0x10, 0x10, "refclk");
    probe.write(RC + 0x84000, Width::Word, 1);
    assert_eq!(
        probe.read(RC + 0x84000, Width::Word),
        0x5,
        "refclk 0 acknowledged"
    );
    assert_eq!(probe.read(PORT0 + 0x208, Width::Word) & 1, 0);
    assert_eq!(probe.cfg_read(0, 0x80) >> 16 & 0x2000, 0);
    probe.write(PORT0 + 0x80, Width::Word, 1);
    assert_eq!(probe.read(PORT0 + 0x208, Width::Word) & 1, 1, "link up");
    assert_eq!(
        probe.cfg_read(0, 0x80) >> 16,
        0x2012,
        "data link layer active"
    );
    assert_eq!(probe.read(PORT0 + 0x804, Width::Word) & 1, 1, "port ready");
}

#[test]
fn a_download_is_summarized_and_the_shared_word_poll_counted() {
    let mut probe = Probe::new();
    probe.enumerate();
    // Halt the ARM through its wrapper, as brcmfmac's download does.
    probe.cfg_write(1, 0x80, ARM_IOCTL & !0xfff);
    probe.write(WINDOW + u64::from(ARM_IOCTL & 0xfff), Width::Word, 0x21);
    let tcm = WINDOW + 0x100_0000;
    let image = b"firmware-image-bytes".repeat(64);
    for (index, chunk) in image.chunks(4).enumerate() {
        let mut word = [0u8; 4];
        word[..chunk.len()].copy_from_slice(chunk);
        probe.write(
            tcm + u64::from(RAM_BASE) + 4 * index as u64,
            Width::Word,
            u64::from(u32::from_le_bytes(word)),
        );
    }
    probe.write(tcm + u64::from(SHARED_WORD), Width::Word, 0);
    probe.lines();
    probe.write(WINDOW + u64::from(ARM_IOCTL & 0xfff), Width::Word, 0x1);
    for _ in 0..5 {
        assert_eq!(probe.read(tcm + u64::from(SHARED_WORD), Width::Word), 0);
    }
    let lines = probe.lines();
    let digest = hex::encode(sha2::Sha256::digest(&image));
    assert!(
        lines.iter().any(|l| l
            == &format!(
                "EVENT TCM extent 0x740000..0x740500 bytes=1280 sha256={digest} \
                 head=\"firmware-image-bytesfirmware-image-bytesfirmware\""
            )),
        "{lines:#?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("EVENT ARM CR4 released"))
    );
    assert!(
        lines
            .iter()
            .any(|l| l == "EVENT TCM download bytes=1284 extents=2")
    );
    assert!(
        lines
            .iter()
            .any(|l| l == "R bar2+0x91fffc pa=0x6c191fffc w4 v=0x0 x5"),
        "{lines:#?}"
    );
    assert!(probe.model.summary().contains("shared-word-polls=5"));
    // BAR2 writes are summarized, never logged one by one.
    assert!(!lines.iter().any(|l| l.starts_with("W bar2")));
}

#[test]
fn a_bar_outside_every_window_makes_the_endpoint_absent() {
    let mut probe = Probe::new();
    probe.cfg_write(0, 0x18, 0x0001_0100);
    probe.cfg_write(1, 0x10, 0x1000_0000);
    probe.cfg_write(1, 0x04, 0x0006);
    assert_eq!(probe.cfg_read(1, 0x00), 0xffff_ffff);
    let lines = probe.lines();
    assert!(
        lines
            .iter()
            .any(|l| l.ends_with("is outside every window: endpoint absent")),
        "{lines:#?}"
    );
    assert!(probe.model.summary().contains("endpoint=a BAR outside"));
}

#[test]
fn nothing_is_refused_at_any_width() {
    let mut probe = Probe::new();
    probe.enumerate();
    for pa in [
        ECAM,
        ECAM + 0x10_0ffc,
        RC + 3,
        PORT0 + 0x7ff8,
        WINDOW,
        WINDOW + 0x7ffc,
        0x6_a000_0000,
    ] {
        for width in [Width::Byte, Width::Half, Width::Word, Width::Double] {
            let pa = pa & !(width.bytes() - 1);
            let mut access = Access::new(0, 0, &mut probe.latch);
            assert!(probe.model.read(pa, width, &mut access).is_ok());
            assert!(probe.model.write(pa, width, 0, &mut access).is_ok());
        }
    }
}

#[test]
fn the_log_keeps_a_bounded_backlog() {
    let mut probe = Probe::new();
    for value in 0..(PENDING as u64 + 10) {
        probe.write(RC + 0x100, Width::Word, value);
    }
    let lines = probe.lines();
    assert_eq!(lines.len(), PENDING);
    assert!(probe.model.summary().contains("dropped="));
}
