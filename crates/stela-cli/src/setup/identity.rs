//! A machine's generated identity, the opt-in `--generated-identity`: a
//! serial number, main logic board serial (MLB), model number, region and
//! Wi-Fi and Ethernet MAC addresses for its DeviceTree's iBoot fill-ins
//! (`serial-number` and the rest), which otherwise keep their `zeroes/N`
//! fallbacks, so every Stambha machine would look to Apple like the same
//! Mac without a serial.
//!
//! Every value is drawn here from the operating system's random generator,
//! in the format the fill-ins hold; none is copied from a real Mac, none is
//! checked against Apple, and nothing here touches the Secure Enclave's
//! attestation, which Apple Account sign-in very probably needs and Stambha
//! does not provide. Like the ECID (`setup::ecid`), an identity is drawn
//! once per machine, kept with its install in the setup record and given to
//! every bundle prepared for it, and to `stela.efi` in `identity.cfg`
//! (`stela_boot_core::generated_identity`); it is never drawn again for a
//! machine that has one.
//!
//! The format is inferred, not Apple's published rule (UNVERIFIED for
//! Apple silicon): serials since 2021 are 10 random characters, and MLBs
//! are 17, here both from the 34 characters without I and O that Apple's
//! serials use (OpenCore's macserial alphabet). A random serial in that
//! space can match a real Mac's, with odds on the order of 1 in 10^7; the
//! warning says so. The MAC addresses are locally administered unicast
//! ones (IEEE 802 bits 1 and 0 of the first octet), which no company is
//! assigned, so they are never a real device's. Bluetooth keeps the
//! address the guest's controller already derives from the ECID
//! (`stela_soc::usb::hci::model::address`): a second, different one in the
//! tree would disagree with the controller's.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use stela_boot_core::generated_identity::{self, Values};

/// The characters of a serial and an MLB: digits and capitals without I and O.
const ALPHABET: &[u8; 34] = b"0123456789ABCDEFGHJKLMNPQRSTUVWXYZ";
pub(super) const SERIAL_LENGTH: usize = 10;
pub(super) const MLB_LENGTH: usize = 17;
/// The Mac mini (2023) M2's (Mac14,3) part numbers: its 256 GB and 512 GB
/// configurations, as Apple sells them.
const MODEL_NUMBERS: [&str; 2] = ["MMFJ3", "MMFK3"];
/// Apple's region suffixes of part numbers: the United States, Canada,
/// the United Kingdom, Germany, France, Italy, Spain, Japan, Australia,
/// Hong Kong, India and China.
const REGIONS: [&str; 12] = [
    "LL/A", "C/A", "B/A", "D/A", "F/A", "T/A", "Y/A", "J/A", "X/A", "ZP/A", "HN/A", "CH/A",
];

/// What setup prints whenever a machine is given a generated identity.
pub(super) const WARNING: &str = "\
Warning: --generated-identity is an experiment.
  Stambha makes up a serial number, board serial, model number, region and
  network addresses for this machine. They are random, made on this
  computer, never copied from a real Mac, and never sent to Apple to check.
  - Apple Account sign-in will most likely still fail: Apple checks the
    Mac's security chip (the Secure Enclave), which Stambha cannot provide.
  - Apple may flag or lock an Apple Account used from this machine. Try it
    only with an account you can afford to have flagged.
  - Never share this identity or reuse it on another machine. It is kept
    in the stick's EFI/stela/setup.json and EFI/stela/identity.cfg.
  - There is a tiny chance that the random serial matches a real Mac's.";

/// A plan's line, after its two spaces, for a machine with an identity.
pub(super) const PLAN_LINE: &str =
    "identity       generated (experimental), kept in EFI/stela/identity.cfg";

/// One machine's generated identity, as the setup record keeps it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Stored", into = "Stored")]
pub(super) struct Identity {
    pub(super) serial: String,
    pub(super) mlb: String,
    pub(super) model_number: String,
    pub(super) region: String,
    pub(super) wifi_mac: [u8; 6],
    pub(super) ethernet_mac: [u8; 6],
}

impl Identity {
    /// A fresh identity from the operating system's generator.
    pub(super) fn generate() -> Result<Self> {
        let mut random = Random::default();
        let identity = Self {
            serial: random.text(SERIAL_LENGTH)?,
            mlb: random.text(MLB_LENGTH)?,
            model_number: MODEL_NUMBERS[random.below(MODEL_NUMBERS.len())?].into(),
            region: REGIONS[random.below(REGIONS.len())?].into(),
            wifi_mac: random.mac()?,
            ethernet_mac: random.mac()?,
        };
        identity.check()?;
        Ok(identity)
    }

    /// Refuse an identity not in the format [`Self::generate`] draws.
    pub(super) fn check(&self) -> Result<()> {
        let drawn = |text: &str, length| {
            text.len() == length && text.bytes().all(|b| ALPHABET.contains(&b))
        };
        ensure!(
            drawn(&self.serial, SERIAL_LENGTH),
            "a generated serial is {SERIAL_LENGTH} digits and capitals without I and O"
        );
        ensure!(
            drawn(&self.mlb, MLB_LENGTH),
            "a generated MLB is {MLB_LENGTH} digits and capitals without I and O"
        );
        ensure!(
            MODEL_NUMBERS.contains(&self.model_number.as_str()),
            "a generated model number is one of {MODEL_NUMBERS:?}"
        );
        ensure!(
            REGIONS.contains(&self.region.as_str()),
            "a generated region is one of {REGIONS:?}"
        );
        for mac in [self.wifi_mac, self.ethernet_mac] {
            ensure!(
                mac[0] & 0b11 == 0b10 && mac[1..] != [0; 5],
                "a generated MAC address is a locally administered unicast one"
            );
        }
        ensure!(
            self.wifi_mac != self.ethernet_mac,
            "a generated identity's MAC addresses differ"
        );
        Ok(())
    }

    /// The DeviceTree fill-in values, by syscfg key and MAC provider.
    pub(super) fn values(&self) -> Values {
        let [serial, mlb, model, region] = generated_identity::SYSCFG_KEYS;
        let [wifi, ethernet] = generated_identity::MAC_ADDRESS_NAMES;
        Values {
            syscfg: vec![
                (serial.into(), self.serial.as_bytes().to_vec()),
                (mlb.into(), self.mlb.as_bytes().to_vec()),
                (model.into(), self.model_number.as_bytes().to_vec()),
                (region.into(), self.region.as_bytes().to_vec()),
            ],
            mac_addresses: vec![
                (wifi.into(), self.wifi_mac.to_vec()),
                (ethernet.into(), self.ethernet_mac.to_vec()),
            ],
        }
    }

    /// `stela-dev boot prepare`'s `--syscfg` and `--macaddr` arguments.
    pub(super) fn prepare_arguments(&self) -> Vec<String> {
        let values = self.values();
        let mut arguments = Vec::new();
        for (flag, entries) in [
            ("--syscfg", &values.syscfg),
            ("--macaddr", &values.mac_addresses),
        ] {
            for (key, bytes) in entries {
                arguments.push(flag.into());
                arguments.push(format!("{key}={}", hex::encode(bytes)));
            }
        }
        arguments
    }
}

/// The record's spelling: text, MAC addresses with colons.
#[derive(Serialize, Deserialize)]
struct Stored {
    serial: String,
    mlb: String,
    model_number: String,
    region: String,
    wifi_mac: String,
    ethernet_mac: String,
}

impl From<Identity> for Stored {
    fn from(identity: Identity) -> Self {
        let mac = |bytes: [u8; 6]| {
            bytes
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<Vec<_>>()
                .join(":")
        };
        Self {
            serial: identity.serial,
            mlb: identity.mlb,
            model_number: identity.model_number,
            region: identity.region,
            wifi_mac: mac(identity.wifi_mac),
            ethernet_mac: mac(identity.ethernet_mac),
        }
    }
}

impl TryFrom<Stored> for Identity {
    type Error = anyhow::Error;

    fn try_from(stored: Stored) -> Result<Self> {
        let mac = |text: &str| -> Result<[u8; 6]> {
            let bytes = hex::decode(text.replace(':', ""))
                .with_context(|| format!("{text:?} is not a MAC address"))?;
            bytes
                .try_into()
                .map_err(|_| anyhow::anyhow!("{text:?} is not a 6-byte MAC address"))
        };
        let identity = Self {
            wifi_mac: mac(&stored.wifi_mac)?,
            ethernet_mac: mac(&stored.ethernet_mac)?,
            serial: stored.serial,
            mlb: stored.mlb,
            model_number: stored.model_number,
            region: stored.region,
        };
        identity.check()?;
        Ok(identity)
    }
}

/// The identity a machine being set up has: the one `recorded` for it, else
/// a fresh one when `asked`, else none. A machine never gets a second one.
pub(super) fn machine_identity(
    asked: bool,
    recorded: Option<&Identity>,
) -> Result<Option<Identity>> {
    match (recorded, asked) {
        (Some(recorded), _) => Ok(Some(recorded.clone())),
        (None, true) => Identity::generate().map(Some),
        (None, false) => Ok(None),
    }
}

/// Bytes from the operating system's generator, drawn a block at a time.
#[derive(Default)]
struct Random {
    block: Vec<u8>,
}

impl Random {
    fn byte(&mut self) -> Result<u8> {
        if self.block.is_empty() {
            let mut block = [0u8; 64];
            getrandom::getrandom(&mut block).context("the system's random generator failed")?;
            self.block = block.to_vec();
        }
        Ok(self.block.pop().expect("a refilled block"))
    }

    /// A uniform number below `bound` (at most 256), without modulo bias.
    fn below(&mut self, bound: usize) -> Result<usize> {
        let limit = 256 - 256 % bound;
        loop {
            let byte = usize::from(self.byte()?);
            if byte < limit {
                return Ok(byte % bound);
            }
        }
    }

    fn text(&mut self, length: usize) -> Result<String> {
        (0..length)
            .map(|_| Ok(char::from(ALPHABET[self.below(ALPHABET.len())?])))
            .collect()
    }

    /// A locally administered unicast MAC address, never all zero after its
    /// first octet.
    fn mac(&mut self) -> Result<[u8; 6]> {
        loop {
            let mut mac = [0u8; 6];
            for byte in &mut mac {
                *byte = self.byte()?;
            }
            mac[0] = (mac[0] & !0b11) | 0b10;
            if mac[1..] != [0; 5] {
                return Ok(mac);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn generated_identities_are_in_the_fill_ins_format() {
        for _ in 0..200 {
            let identity = Identity::generate().unwrap();
            identity.check().unwrap();
            assert_eq!(identity.serial.len(), SERIAL_LENGTH);
            assert_eq!(identity.mlb.len(), MLB_LENGTH);
            for text in [&identity.serial, &identity.mlb] {
                assert!(
                    text.bytes()
                        .all(|b| b.is_ascii_digit() || b.is_ascii_uppercase()),
                    "{text}"
                );
                assert!(!text.contains(['I', 'O']), "{text}");
            }
            assert!(MODEL_NUMBERS.contains(&identity.model_number.as_str()));
            assert!(REGIONS.contains(&identity.region.as_str()));
            for mac in [identity.wifi_mac, identity.ethernet_mac] {
                assert_eq!(mac[0] & 1, 0, "unicast");
                assert_eq!(mac[0] & 2, 2, "locally administered");
            }
            // Every value fits its fill-in, and the boot side takes them.
            let values = identity.values();
            for (_, bytes) in &values.syscfg {
                assert!(bytes.len() <= generated_identity::SYSCFG_BYTES);
            }
            let text = generated_identity::encode([(0x1a, &values)]);
            assert_eq!(
                generated_identity::for_machine(text.as_bytes(), 0x1a),
                Ok(Some(values))
            );
        }
    }

    #[test]
    fn identities_are_unique_across_many_generations() {
        let mut serials = HashSet::new();
        let mut mlbs = HashSet::new();
        let mut macs = HashSet::new();
        const COUNT: usize = 20_000;
        for _ in 0..COUNT {
            let identity = Identity::generate().unwrap();
            assert!(serials.insert(identity.serial));
            assert!(mlbs.insert(identity.mlb));
            assert!(macs.insert(identity.wifi_mac));
            assert!(macs.insert(identity.ethernet_mac));
        }
        assert_eq!(serials.len(), COUNT);
        assert_eq!(macs.len(), 2 * COUNT);
    }

    #[test]
    fn the_record_keeps_an_identity_exactly_and_refuses_another_format() {
        let identity = Identity {
            serial: "K7Q2MXW9RT".into(),
            mlb: "C2XQ8T4K9VW1H7PLM".into(),
            model_number: "MMFJ3".into(),
            region: "LL/A".into(),
            wifi_mac: [0x02, 0x11, 0x22, 0x33, 0x44, 0x55],
            ethernet_mac: [0x06, 0x66, 0x77, 0x88, 0x99, 0xaa],
        };
        let json = serde_json::to_value(&identity).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "serial": "K7Q2MXW9RT",
                "mlb": "C2XQ8T4K9VW1H7PLM",
                "model_number": "MMFJ3",
                "region": "LL/A",
                "wifi_mac": "02:11:22:33:44:55",
                "ethernet_mac": "06:66:77:88:99:aa",
            })
        );
        assert_eq!(
            serde_json::from_value::<Identity>(json.clone()).unwrap(),
            identity
        );
        for (field, bad) in [
            ("serial", "K7Q2MXW9R"),
            ("serial", "K7Q2MXW9RO"),
            ("mlb", "c2xq8t4k9vw1h7plm"),
            ("model_number", "MNH73"),
            ("region", "ZZ/A"),
            ("wifi_mac", "01:11:22:33:44:55"),
            ("wifi_mac", "00:11:22:33:44:55"),
            ("ethernet_mac", "02:11:22:33:44:55"),
            ("ethernet_mac", "06:66:77:88:99"),
        ] {
            let mut changed = json.clone();
            changed[field] = bad.into();
            assert!(
                serde_json::from_value::<Identity>(changed).is_err(),
                "{field} {bad}"
            );
        }
        assert_eq!(
            identity.prepare_arguments(),
            [
                "--syscfg",
                "SrNm=4b3751324d5857395254",
                "--syscfg",
                "MLB#=433258513854344b395657314837504c4d",
                "--syscfg",
                "Mod#=4d4d464a33",
                "--syscfg",
                "Regn=4c4c2f41",
                "--macaddr",
                "wifiaddr=021122334455",
                "--macaddr",
                "ethaddr=0666778899aa",
            ]
        );
    }

    #[test]
    fn a_machine_keeps_its_identity_and_gets_one_only_when_asked() {
        assert_eq!(machine_identity(false, None).unwrap(), None);
        let fresh = machine_identity(true, None).unwrap().unwrap();
        // Recorded wins, asked or not: never drawn again.
        assert_eq!(
            machine_identity(true, Some(&fresh)).unwrap(),
            Some(fresh.clone())
        );
        assert_eq!(machine_identity(false, Some(&fresh)).unwrap(), Some(fresh));
    }
}
