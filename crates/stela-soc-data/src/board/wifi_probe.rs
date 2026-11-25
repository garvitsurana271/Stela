//! Where the V2 Wi-Fi probe's logging stubs sit (`--v2-wifi-probe`): the
//! `apcie,t8112` PCIe root complex of a J473AP-shaped tree, its port 0
//! bridge and the BCM4387 Wi-Fi function behind it.
//!
//! The 26B123 J473AP DeviceTree places them so (`/arm-io/apcie`):
//! `reg[0]` is the ECAM window (`bus-range` 0..8), the other 25 entries the
//! root complex's own banks, some of them shared with the PMGR (`reg[4]`)
//! or other blocks (`reg[5]`); `ranges` gives two PCI windows with three
//! PCI address cells, two parent cells and two size cells: 64-bit
//! prefetchable memory at PCI 0x6a0000000 (CPU 0x6a0000000, 512 MiB) and
//! 32-bit memory at PCI 0xc0000000 (CPU 0x6c0000000, 1 GiB). Port 0
//! (`pci-bridge0`, `apcie-port` 0) holds `wlan` (`wlan-pcie,bcm4387`).
//!
//! This module only reads the tree. Nothing here admits a device: the boot
//! does that in its own tree, only with the option given.
use super::apertures;
use crate::Result;
use alloc::vec::Vec;
use stela_contracts::adt::Node;
use stela_contracts::refuse;

/// The root complex the probe stubs.
pub const CONTROLLER: &str = "apcie,t8112";
/// Its root ports.
pub const BRIDGE: &str = "apcie-bridge";
/// The Wi-Fi function on port 0.
pub const WIFI: &str = "wlan-pcie,bcm4387";
/// The DeviceTree paths, below `/arm-io`, the probe needs.
pub const CONTROLLER_NODE: &str = "apcie";
pub const BRIDGE_NODE: &str = "pci-bridge0";
pub const WIFI_NODE: &str = "wlan";

/// One PCI memory window from the controller's `ranges`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    /// The PCI address space cell (`phys.hi`): 0x02000000 32-bit memory,
    /// 0x43000000 64-bit prefetchable memory.
    pub space: u32,
    /// The window's first PCI bus address.
    pub pci: u64,
    /// Where the CPU sees it, in guest PA.
    pub physical: u64,
    pub bytes: u64,
}

impl Window {
    /// The guest PA of PCI bus address `pci`, if this window holds it.
    pub fn to_physical(&self, pci: u64, bytes: u64) -> Option<u64> {
        let end = pci.checked_add(bytes)?;
        (pci >= self.pci && end <= self.pci.checked_add(self.bytes)?)
            .then(|| self.physical + (pci - self.pci))
    }
}

/// The probe's placement, all in guest PA.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placement {
    /// Every controller `reg` entry: its index, guest PA and length.
    /// `reg[0]` is the ECAM window ([`Self::config`]).
    pub registers: Vec<(usize, u64, u64)>,
    /// The ECAM window.
    pub config: (u64, u64),
    /// The first and last bus numbers (`bus-range`).
    pub buses: (u32, u32),
    /// The PCI memory windows (`ranges`).
    pub windows: Vec<Window>,
    /// `#ports`.
    pub ports: u32,
    /// The controller's AIC inputs (`interrupts`), as the tree lists them.
    pub interrupts: Vec<u32>,
}

impl Placement {
    /// The lifecycle owner the PMGR's resource inventory names for the
    /// controller: its first `reg` entry's guest PA.
    pub fn owner(&self) -> u64 {
        self.config.0
    }

    /// The guest PA of PCI bus address `pci` (`bytes` long), if a window
    /// holds it.
    pub fn to_physical(&self, pci: u64, bytes: u64) -> Option<u64> {
        self.windows
            .iter()
            .find_map(|window| window.to_physical(pci, bytes))
    }
}

/// The probe's placement in `tree`, a prepared tree whose `/arm-io/apcie`,
/// `pci-bridge0` and `wlan` keep their `compatible` (the boot restores it
/// before it asks). Anything that differs from the J473AP shape is refused,
/// so that the probe stays absent rather than guess.
pub fn placement(tree: &[u8]) -> Result<Placement> {
    let root = Node::root_exact(tree).ok_or(refuse!("DeviceTree has no root or is malformed"))?;
    let io = root
        .child("arm-io")
        .ok_or(refuse!("DeviceTree has no /arm-io"))?;
    let controller = io
        .child(CONTROLLER_NODE)
        .ok_or(refuse!("DeviceTree has no /arm-io/apcie"))?;
    if !controller.compatible(CONTROLLER) {
        return Err(refuse!("/arm-io/apcie is not {model}", model = CONTROLLER));
    }
    let bridge = controller
        .child(BRIDGE_NODE)
        .ok_or(refuse!("/arm-io/apcie has no pci-bridge0"))?;
    if !bridge.compatible(BRIDGE) || bridge.first_u32("apcie-port") != Some(0) {
        return Err(refuse!(
            "pci-bridge0 is not port 0's {model}",
            model = BRIDGE
        ));
    }
    let wifi = bridge
        .child(WIFI_NODE)
        .ok_or(refuse!("pci-bridge0 has no wlan function"))?;
    if !wifi.compatible(WIFI) {
        return Err(refuse!("pci-bridge0/wlan is not {model}", model = WIFI));
    }
    if controller.first_u32("#address-cells") != Some(3)
        || controller.first_u32("#size-cells") != Some(2)
        || io.first_u32("#address-cells") != Some(2)
    {
        return Err(refuse!(
            "/arm-io/apcie's address cells differ from 3 PCI, 2 parent and 2 size cells"
        ));
    }
    let raw = apertures::registers(&controller)
        .ok_or(refuse!("/arm-io/apcie has no complete reg pairs"))?;
    let mut registers = Vec::new();
    for (index, &(offset, bytes)) in raw.iter().enumerate() {
        let physical = io.translate_range(offset, bytes).ok_or(refuse!(
            "/arm-io/apcie reg[{index}] is outside /arm-io ranges",
            index
        ))?;
        registers.push((index, physical, bytes));
    }
    let config = registers
        .first()
        .map(|&(_, physical, bytes)| (physical, bytes))
        .ok_or(refuse!("/arm-io/apcie has no reg"))?;
    let buses = controller
        .cells32("bus-range")
        .filter(|cells| cells.len() == 2 && cells[0] <= cells[1] && cells[1] <= 0xff)
        .map(|cells| (cells[0], cells[1]))
        .ok_or(refuse!("/arm-io/apcie has no bus-range of two bus numbers"))?;
    // ECAM gives each bus 1 MiB.
    if u64::from(buses.1 - buses.0 + 1) << 20 > config.1 {
        return Err(refuse!(
            "/arm-io/apcie's ECAM window is smaller than its bus-range"
        ));
    }
    let ranges = controller
        .property("ranges")
        .filter(|raw| !raw.is_empty() && raw.len().is_multiple_of(28))
        .ok_or(refuse!("/arm-io/apcie has no ranges of 7-cell records"))?;
    let mut windows = Vec::new();
    for record in ranges.as_chunks::<28>().0 {
        let word = |at: usize| u32::from_le_bytes(record[at..at + 4].try_into().expect("4 bytes"));
        let double = |at: usize| u64::from(word(at)) | u64::from(word(at + 4)) << 32;
        let (space, pci, parent, bytes) = (word(0), double(4), double(12), double(20));
        if space & 0x0300_0000 == 0 || bytes == 0 {
            return Err(refuse!(
                "/arm-io/apcie ranges entry {space:#x} is not a memory window",
                space
            ));
        }
        let physical = io.translate_range(parent, bytes).ok_or(refuse!(
            "/arm-io/apcie window {pci:#x} is outside /arm-io ranges",
            pci
        ))?;
        windows.push(Window {
            space,
            pci,
            physical,
            bytes,
        });
    }
    let ports = controller
        .first_u32("#ports")
        .filter(|ports| (1..=8).contains(ports))
        .ok_or(refuse!("/arm-io/apcie has no #ports"))?;
    let interrupts = controller.cells32("interrupts").unwrap_or_default();
    Ok(Placement {
        registers,
        config,
        buses,
        windows,
        ports,
        interrupts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use stela_contracts::adt::encode;

    fn words(values: &[u32]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_le_bytes()).collect()
    }
    fn doubles(values: &[u64]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    /// A J473AP-shaped `/arm-io` with the probe's three nodes, the
    /// controller's `compatible` given by `controller`.
    fn tree(controller: &[u8]) -> Vec<u8> {
        let wifi = encode(
            &[
                ("name", b"wlan\0"),
                ("compatible", b"wlan-pcie,bcm4387\0wlan-pcie,bcm\0"),
            ],
            &[],
        );
        let bridge = encode(
            &[
                ("name", b"pci-bridge0\0"),
                ("compatible", b"apcie-bridge\0"),
                ("apcie-port", &words(&[0])),
            ],
            &[wifi],
        );
        let reg = doubles(&[
            0x6_9000_0000,
            0x1000_0000,
            0x6_8000_0000,
            0x4000,
            0x3d2c_8000,
            0x1100,
        ]);
        let mut ranges = words(&[0x4300_0000, 0xa000_0000, 6, 0xa000_0000, 6, 0x2000_0000, 0]);
        ranges.extend(words(&[
            0x0200_0000,
            0xc000_0000,
            0,
            0xc000_0000,
            6,
            0x4000_0000,
            0,
        ]));
        let apcie = encode(
            &[
                ("name", b"apcie\0"),
                ("compatible", controller),
                ("#address-cells", &words(&[3])),
                ("#size-cells", &words(&[2])),
                ("reg", &reg),
                ("bus-range", &words(&[0, 8])),
                ("ranges", &ranges),
                ("#ports", &words(&[4])),
                ("interrupts", &words(&[0x30d, 0x310])),
            ],
            &[bridge],
        );
        let io_ranges = doubles(&[
            0,
            0x2_0000_0000,
            0x1_0000_0000,
            0x6_0000_0000,
            0x6_0000_0000,
            0x8000_0000,
            0x6_8000_0000,
            0x6_8000_0000,
            0x8000_0000,
        ]);
        let io = encode(
            &[
                ("name", b"arm-io\0"),
                ("#address-cells", &words(&[2])),
                ("#size-cells", &words(&[2])),
                ("ranges", &io_ranges),
            ],
            &[apcie],
        );
        encode(&[("name", b"device-tree\0")], &[io])
    }

    #[test]
    fn the_j473_shape_places_ecam_banks_and_both_windows() {
        let placement = placement(&tree(b"apcie,t8112\0")).unwrap();
        assert_eq!(placement.config, (0x6_9000_0000, 0x1000_0000));
        assert_eq!(placement.owner(), 0x6_9000_0000);
        assert_eq!(
            placement.registers,
            vec![
                (0, 0x6_9000_0000, 0x1000_0000),
                (1, 0x6_8000_0000, 0x4000),
                (2, 0x2_3d2c_8000, 0x1100),
            ]
        );
        assert_eq!(placement.buses, (0, 8));
        assert_eq!(placement.ports, 4);
        assert_eq!(placement.interrupts, vec![0x30d, 0x310]);
        assert_eq!(
            placement.windows,
            vec![
                Window {
                    space: 0x4300_0000,
                    pci: 0x6_a000_0000,
                    physical: 0x6_a000_0000,
                    bytes: 0x2000_0000,
                },
                Window {
                    space: 0x0200_0000,
                    pci: 0xc000_0000,
                    physical: 0x6_c000_0000,
                    bytes: 0x4000_0000,
                },
            ]
        );
        // 32-bit BARs land at PCI 0xc0000000 + n, which the CPU sees at
        // 0x6c0000000 + n; nothing outside a window translates.
        assert_eq!(
            placement.to_physical(0xc000_8000, 0x8000),
            Some(0x6_c000_8000)
        );
        assert_eq!(placement.to_physical(0xbfff_f000, 0x2000), None);
        assert_eq!(placement.to_physical(0x1_0000_0000, 4), None);
    }

    #[test]
    fn a_hidden_or_different_controller_is_refused() {
        // A prepared bundle's tree, where the controller lost `compatible`.
        assert!(placement(&tree(b"\0")).is_err());
        assert!(placement(&tree(b"apcie,t8103\0")).is_err());
        assert!(placement(&[0; 8]).is_err());
    }
}
