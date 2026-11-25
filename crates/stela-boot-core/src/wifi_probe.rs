//! The V2 Wi-Fi probe's admission (`--v2-wifi-probe`,
//! [`crate::options::V2_WIFI_PROBE`]): phase 0 of native Wi-Fi, logging
//! stubs only (`stela_soc::wifi_probe`).
//!
//! Without the option nothing here runs and the boot's tree, its admission
//! ledger and its board are what they always were. With it, the boot's own
//! tree (never the bundle's, so no bundle revision changes) gets back the
//! J473AP PCIe root complex, port 0, the BCM4387 Wi-Fi function and the
//! nodes its driver waits for (`stela_loader::devices::WIFI_PROBE_NODES`),
//! from the bundle's original tree. The change is kept only when the result
//! still fits the DeviceTree's reservation, still parses as the board and
//! gives the probe a placement; otherwise the tree and ledger stay as they
//! were and the probe is absent, with the reason in the log. Nothing here
//! stops a boot.
use alloc::{format, string::String, vec::Vec};
use stela_loader::apple_device_tree::DeviceTree;
use stela_soc_data::board::wifi_probe::{self, Placement};

/// What admission did: the probe's placement and a line per change.
#[derive(Debug)]
pub struct Admitted {
    pub placement: Placement,
    pub lines: Vec<String>,
}

/// `--v2-wifi-probe`'s admission: `None` without the option, which leaves
/// `tree` and `hidden` untouched; else the placement, or why the probe
/// stays absent with `tree` and `hidden` as they were. `original` is the
/// bundle's unchanged original DeviceTree and `reservation` the guest RAM
/// the loaded DeviceTree region reserves.
pub fn admit(
    enabled: bool,
    tree: &mut DeviceTree,
    original: &[u8],
    hidden: &mut Vec<String>,
    reservation: u64,
) -> Option<Result<Admitted, String>> {
    enabled.then(|| admit_in(tree, original, hidden, reservation))
}

fn admit_in(
    tree: &mut DeviceTree,
    original: &[u8],
    hidden: &mut Vec<String>,
    reservation: u64,
) -> Result<Admitted, String> {
    let original =
        DeviceTree::parse(original).map_err(|error| format!("the original tree: {error}"))?;
    let mut next = tree.clone();
    let mut ledger = hidden.clone();
    let lines = stela_loader::devices::admit_wifi_probe(&mut next, &original, &mut ledger)?;
    let bytes = next.serialize();
    if bytes.len() as u64 > reservation {
        return Err(format!(
            "the tree would grow to {} bytes, past its {reservation}-byte reservation",
            bytes.len()
        ));
    }
    stela_soc_data::board::board(&bytes)
        .map_err(|error| format!("the board would no longer parse: {error}"))?;
    let placement =
        wifi_probe::placement(&bytes).map_err(|error| format!("no placement: {error}"))?;
    *tree = next;
    *hidden = ledger;
    Ok(Admitted { placement, lines })
}

/// The boot's line for an admission result.
pub fn line(result: &Result<Admitted, String>) -> String {
    match result {
        Ok(admitted) => format!(
            "WIFI-PROBE admitted: {} (ECAM {:#x}+{:#x}, {} banks, {} windows); \
             logging stubs only, no firmware runs\n",
            admitted.lines.join(", "),
            admitted.placement.config.0,
            admitted.placement.config.1,
            admitted.placement.registers.len(),
            admitted.placement.windows.len(),
        ),
        Err(reason) => format!("WIFI-PROBE absent: {reason}; the boot goes on without it\n"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use stela_loader::apple_device_tree::Node;

    fn tree() -> DeviceTree {
        let mut root = Node::named("device-tree").unwrap();
        root.children.push(Node::named("arm-io").unwrap());
        DeviceTree { root }
    }

    #[test]
    fn without_the_option_nothing_changes() {
        let mut boot = tree();
        let before = boot.serialize();
        let mut hidden = vec![String::from("/arm-io/apcie")];
        // Not even a malformed original is looked at.
        assert!(admit(false, &mut boot, &[0xff; 3], &mut hidden, 0).is_none());
        assert_eq!(boot.serialize(), before);
        assert_eq!(hidden, ["/arm-io/apcie"]);
    }

    #[test]
    fn a_tree_without_the_probe_s_nodes_keeps_everything_and_says_why() {
        let mut boot = tree();
        let before = boot.serialize();
        let mut hidden = vec![String::from("/arm-io/apcie")];
        let original = tree().serialize();
        let result = admit(true, &mut boot, &original, &mut hidden, 1 << 20).unwrap();
        let reason = result.as_ref().unwrap_err();
        assert!(reason.contains("has no /arm-io/apcie"), "{reason}");
        assert!(line(&result).starts_with("WIFI-PROBE absent: "));
        assert_eq!(boot.serialize(), before);
        assert_eq!(hidden, ["/arm-io/apcie"]);
        // A malformed original is a reason too.
        let result = admit(true, &mut boot, &[0xff; 3], &mut hidden, 1 << 20).unwrap();
        assert!(result.unwrap_err().starts_with("the original tree: "));
        assert_eq!(boot.serialize(), before);
    }
}
