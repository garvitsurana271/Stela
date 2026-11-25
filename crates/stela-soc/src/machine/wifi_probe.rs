//! Placing the V2 Wi-Fi probe's logging stubs ([`crate::wifi_probe`]),
//! which only `--v2-wifi-probe` asks for.
use super::*;
use crate::wifi_probe::WifiProbe;
use stela_soc_data::board::wifi_probe::Placement;

impl AppleDevices {
    /// Place the probe over `placement`'s controller banks, its ECAM window
    /// and its PCI memory windows, each only where no device owns the bytes
    /// yet: a bank the PMGR or the eFuse model serves stays theirs. The
    /// model is the controller's lifecycle owner (its first `reg` entry,
    /// as the PMGR's inventory names it). Returns a line for each range it
    /// took or left. It never refuses: what it cannot take stays with its
    /// owner or unmapped, and the log says so.
    pub fn add_wifi_probe(&mut self, placement: Placement) -> Vec<String> {
        let mut lines = Vec::new();
        if self.wifi_probe.is_some() || self.power_configured {
            lines.push(String::from(
                "not placed: already placed or the PMGR's lifecycle is bound",
            ));
            return lines;
        }
        let owner = placement.owner();
        let mut ranges: Vec<(String, u64, u64)> = placement
            .registers
            .iter()
            .map(|&(index, physical, bytes)| (format!("reg[{index}]"), physical, bytes))
            .collect();
        for (index, window) in placement.windows.iter().enumerate() {
            ranges.push((
                format!("window{index}(space {:#x})", window.space),
                window.physical,
                window.bytes,
            ));
        }
        let device = self.bus.add(Box::new(WifiProbe::new(placement)));
        let mut mapped = 0;
        for (name, physical, bytes) in ranges {
            // The model's offsets are guest PAs.
            match self.bus.map_gaps(device, physical, bytes, physical) {
                Ok(()) => {
                    mapped += 1;
                    lines.push(format!("mapped {name} {physical:#x}+{bytes:#x}"));
                }
                Err(MapError::Overlap { existing }) if existing == device => lines.push(format!(
                    "mapped {name} {physical:#x}+{bytes:#x} within an earlier bank"
                )),
                Err(error) => lines.push(format!(
                    "left {name} {physical:#x}+{bytes:#x} to its owner: {error:?}"
                )),
            }
        }
        match self.register_power_device(owner, device, false) {
            Ok(()) => lines.push(format!("lifecycle owner {owner:#x}")),
            Err(error) => lines.push(format!("no lifecycle owner: {error}")),
        }
        lines.push(format!("placed with {mapped} ranges"));
        self.wifi_probe = Some(device);
        lines
    }

    /// The probe's lines since the last take; none without the probe.
    pub fn take_wifi_probe_lines(&mut self) -> Vec<String> {
        let Some(device) = self.wifi_probe else {
            return Vec::new();
        };
        self.bus
            .device_mut::<WifiProbe>(device)
            .map(WifiProbe::take_lines)
            .unwrap_or_default()
    }

    /// The probe's totals, when it is placed.
    pub fn wifi_probe_summary(&self) -> Option<String> {
        self.bus
            .device::<WifiProbe>(self.wifi_probe?)
            .map(WifiProbe::summary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use stela_soc_data::board::wifi_probe::Window;

    fn placement() -> Placement {
        Placement {
            registers: vec![
                (0, 0x6_9000_0000, 0x1000_0000),
                (1, 0x6_8000_0000, 0x4000),
                (2, 0x6_8008_0000, 0x9_0000),
                (3, 0x6_8008_4000, 0x4000),
            ],
            config: (0x6_9000_0000, 0x1000_0000),
            buses: (0, 8),
            windows: vec![Window {
                space: 0x0200_0000,
                pci: 0xc000_0000,
                physical: 0x6_c000_0000,
                bytes: 0x4000_0000,
            }],
            ports: 4,
            interrupts: vec![],
        }
    }

    #[test]
    fn the_probe_takes_free_ranges_and_leaves_owned_ones() {
        let mut devices = AppleDevices::new();
        // An eFuse block already owns the start of reg[1].
        devices.add_aon_fuses(0x6_8000_0000).unwrap();
        let lines = devices.add_wifi_probe(placement());
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("mapped reg[0] 0x690000000"))
        );
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("left reg[1] 0x680000000"))
        );
        assert!(lines.iter().any(|l| l.ends_with("within an earlier bank")));
        assert!(lines.iter().any(|l| l == "lifecycle owner 0x690000000"));
        assert!(lines.iter().any(|l| l == "placed with 3 ranges"));
        assert!(devices.wifi_probe_summary().is_some());
        // The eFuse model still answers its own bank.
        assert!(
            devices
                .bus
                .route(0x6_8000_0000, Width::Word)
                .is_ok_and(|(owner, _)| owner != devices.wifi_probe.unwrap())
        );
        assert!(devices.take_wifi_probe_lines().is_empty());
        // A second placement is refused, not stacked.
        assert!(devices.add_wifi_probe(placement())[0].starts_with("not placed"));
    }

    #[test]
    fn without_the_probe_the_board_has_none() {
        let mut devices = AppleDevices::new();
        assert!(devices.take_wifi_probe_lines().is_empty());
        assert!(devices.wifi_probe_summary().is_none());
    }
}
