//! The generated identity file (`\EFI\stela\identity.cfg`): the board
//! configuration values and MAC addresses `stambha create --generated-identity`
//! made up for each of a stick's machines, which every boot bundle
//! `stela.efi` builds from an installed system fills the DeviceTree's iBoot
//! fill-ins with (`syscfg/SrNm`, `macaddr/wifiaddr`, ...), as setup's own
//! restore bundles are prepared with them (`stela-dev boot prepare
//! --syscfg`, `--macaddr`).
//!
//! The values are random, made on the computer that ran setup; they are
//! never a real Mac's, and nothing checks them with Apple. Without the
//! file, or without a machine's lines in it, the fill-ins keep their
//! `zeroes/N` fallbacks, as before the opt-in existed.
//!
//! One value per line: the machine's ECID as `0x` and 16 hex digits, the
//! source (`syscfg` or `macaddr`) and `KEY=HEX`, as `boot prepare` takes
//! them, separated by spaces or tabs. Empty lines and lines starting with
//! `#` are skipped. Only the keys setup writes are taken ([`SYSCFG_KEYS`],
//! [`MAC_ADDRESS_NAMES`]); any other line refuses the whole file, so a
//! damaged file never gives a machine half an identity.
use alloc::{format, string::String, vec::Vec};

/// The file, beside `stela.cfg` on the ESP.
pub const FILE: &str = "\\EFI\\stela\\identity.cfg";
/// The most bytes the loader reads: a stick's machines' lines with room.
pub const MAX_BYTES: usize = 16 * 1024;
/// The syscfg keys a generated identity fills: the serial number, the
/// main logic board's serial, the model number and its region
/// (J473AP's `serial-number`, `mlb-serial-number`, `model-number` and
/// `region-info` recipes).
pub const SYSCFG_KEYS: [&str; 4] = ["SrNm", "MLB#", "Mod#", "Regn"];
/// The longest syscfg value: the recipes' `/0x20` size.
pub const SYSCFG_BYTES: usize = 0x20;
/// The MAC address providers a generated identity fills
/// (`/chosen mac-address-wifi0`, `-ethernet0`).
pub const MAC_ADDRESS_NAMES: [&str; 2] = ["wifiaddr", "ethaddr"];
pub const MAC_ADDRESS_BYTES: usize = 6;

const SYSCFG: &str = "syscfg";
const MACADDR: &str = "macaddr";

/// One machine's generated values, by key, in the file's order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Values {
    pub syscfg: Vec<(String, Vec<u8>)>,
    pub mac_addresses: Vec<(String, Vec<u8>)>,
}

impl Values {
    pub fn is_empty(&self) -> bool {
        self.syscfg.is_empty() && self.mac_addresses.is_empty()
    }
}

/// The file's text for `machines`, each an ECID and its values.
pub fn encode<'a>(machines: impl IntoIterator<Item = (u64, &'a Values)>) -> String {
    let mut text = String::from(
        "# Stambha's generated identities: random values made by `stambha create \
         --generated-identity`,\n# never a real Mac's. Keep them private; never copy them to \
         another machine.\n",
    );
    for (ecid, values) in machines {
        for (source, entries) in [(SYSCFG, &values.syscfg), (MACADDR, &values.mac_addresses)] {
            for (key, bytes) in entries {
                let mut hex = String::with_capacity(bytes.len() * 2);
                for byte in bytes {
                    hex.push_str(&format!("{byte:02x}"));
                }
                text.push_str(&format!("{ecid:#018x} {source} {key}={hex}\n"));
            }
        }
    }
    text
}

/// The values `file` holds for the machine `ecid`: `None` when it holds
/// none for it. A file with any line it cannot take is refused whole.
pub fn for_machine(file: &[u8], ecid: u64) -> Result<Option<Values>, &'static str> {
    if file.len() > MAX_BYTES {
        return Err("identity.cfg is larger than any setup writes");
    }
    let text = core::str::from_utf8(file).map_err(|_| "identity.cfg is not UTF-8")?;
    let mut values = Values::default();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut words = line.split_ascii_whitespace();
        let (Some(machine), Some(source), Some(entry), None) =
            (words.next(), words.next(), words.next(), words.next())
        else {
            return Err("an identity.cfg line is not ECID, source and KEY=HEX");
        };
        let machine = parse_ecid(machine)?;
        let (key, hex) = entry
            .split_once('=')
            .ok_or("an identity.cfg value is not KEY=HEX")?;
        let bytes = decode(hex)?;
        let (entries, allowed): (_, &[&str]) = match source {
            SYSCFG => {
                if bytes.is_empty()
                    || bytes.len() > SYSCFG_BYTES
                    || !bytes.iter().all(|&b| b.is_ascii_graphic() || b == b' ')
                {
                    return Err("an identity.cfg syscfg value is not 1 to 32 printable characters");
                }
                (&mut values.syscfg, &SYSCFG_KEYS)
            }
            MACADDR => {
                if bytes.len() != MAC_ADDRESS_BYTES || bytes[0] & 1 != 0 {
                    return Err("an identity.cfg MAC address is not 6 bytes of a unicast address");
                }
                (&mut values.mac_addresses, &MAC_ADDRESS_NAMES)
            }
            _ => return Err("an identity.cfg line names neither syscfg nor macaddr"),
        };
        if !allowed.contains(&key) {
            return Err("an identity.cfg line names a key setup never writes");
        }
        if machine != ecid {
            continue;
        }
        if entries.iter().any(|(known, _)| known == key) {
            return Err("identity.cfg names one key of a machine twice");
        }
        entries.push((key.into(), bytes));
    }
    Ok((!values.is_empty()).then_some(values))
}

fn parse_ecid(word: &str) -> Result<u64, &'static str> {
    let digits = word
        .strip_prefix("0x")
        .filter(|digits| digits.len() == 16)
        .ok_or("an identity.cfg line does not start with a 0x and 16 digit ECID")?;
    match u64::from_str_radix(digits, 16) {
        Ok(0) | Err(_) => Err("an identity.cfg line does not start with a nonzero ECID"),
        Ok(ecid) => Ok(ecid),
    }
}

fn decode(hex: &str) -> Result<Vec<u8>, &'static str> {
    if !hex.len().is_multiple_of(2) {
        return Err("an identity.cfg value is not whole hex bytes");
    }
    (0..hex.len())
        .step_by(2)
        .map(|at| {
            hex.get(at..at + 2)
                .and_then(|pair| u8::from_str_radix(pair, 16).ok())
                .ok_or("an identity.cfg value is not whole hex bytes")
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values() -> Values {
        Values {
            syscfg: vec![
                ("SrNm".into(), b"K7Q2MXW9RT".to_vec()),
                ("MLB#".into(), b"C2XQ8T4K9VW1H7PLM".to_vec()),
                ("Mod#".into(), b"MMFJ3".to_vec()),
                ("Regn".into(), b"LL/A".to_vec()),
            ],
            mac_addresses: vec![
                ("wifiaddr".into(), vec![0x02, 0x11, 0x22, 0x33, 0x44, 0x55]),
                ("ethaddr".into(), vec![0x06, 0x66, 0x77, 0x88, 0x99, 0xaa]),
            ],
        }
    }

    #[test]
    fn each_machine_reads_back_its_own_values() {
        let first = values();
        let mut second = values();
        second.syscfg[0].1 = b"HN4P7R2LWX".to_vec();
        let text = encode([(0x1a, &first), (0x700c_de23_bbf1_c97d, &second)]);
        assert!(text.contains("0x000000000000001a syscfg SrNm=4b37513"));
        assert_eq!(for_machine(text.as_bytes(), 0x1a), Ok(Some(first)));
        assert_eq!(
            for_machine(text.as_bytes(), 0x700c_de23_bbf1_c97d),
            Ok(Some(second))
        );
        assert_eq!(for_machine(text.as_bytes(), 0x2b), Ok(None));
        assert_eq!(for_machine(b"", 0x1a), Ok(None));
        assert_eq!(for_machine(b"# only a comment\n\n", 0x1a), Ok(None));
    }

    #[test]
    fn a_line_setup_never_writes_refuses_the_file() {
        let good = "0x000000000000001a syscfg SrNm=4b37\n";
        assert!(for_machine(good.as_bytes(), 0x1a).unwrap().is_some());
        for bad in [
            "0x1a syscfg SrNm=4b37",
            "0x0000000000000000 syscfg SrNm=4b37",
            "0x000000000000001a syscfg CFG#=4b37",
            "0x000000000000001a syscfg SrNm=4b3",
            "0x000000000000001a syscfg SrNm=",
            "0x000000000000001a syscfg SrNm=0a",
            "0x000000000000001a nvram SrNm=4b37",
            "0x000000000000001a macaddr btaddr=021122334455",
            "0x000000000000001a macaddr wifiaddr=0111223344",
            "0x000000000000001a macaddr wifiaddr=011122334455",
            "0x000000000000001a syscfg SrNm=4b37 extra",
            "0x000000000000001a syscfg SrNm=4b37\n0x000000000000001a syscfg SrNm=4b38",
        ] {
            let file = format!("{good}{bad}\n");
            assert!(for_machine(file.as_bytes(), 0x1a).is_err(), "{bad}");
        }
        // Another machine's damaged line refuses this one's too.
        let file = format!("{good}0x000000000000002b syscfg CFG#=4b37\n");
        assert!(for_machine(file.as_bytes(), 0x1a).is_err());
        assert!(for_machine(&[b'#'; MAX_BYTES + 1], 0x1a).is_err());
        assert!(for_machine(&[0xff, 0xfe], 0x1a).is_err());
    }
}
