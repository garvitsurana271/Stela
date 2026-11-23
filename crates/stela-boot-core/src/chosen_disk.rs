//! Whether the partition Install macOS chose for the Mac's SSD
//! (`EFI/stela/disk.cfg`, `SERIAL:GUID`) is still the one to boot.
//!
//! The USB keeps that record when the disk is later replaced, wiped,
//! repartitioned or unplugged, and when the USB moves to another PC. Such a
//! record is stale: the macOS choice is left out, Install macOS chooses a
//! disk again, and the record is replaced only when a new disk is
//! confirmed. Only a store that cannot be read on a partition that is
//! otherwise the chosen one, with a Mac system on it, still stops the boot.

/// What a search for the chosen partition found: every whole disk
/// reporting the serial, then its GPT partitions with the GUID.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Search {
    /// No whole disk reports the serial.
    NoDisk,
    /// Several whole disks report the serial.
    SeveralDisks,
    /// The disk has no GPT, or none that can be read: a blank or wiped disk.
    NoTable,
    /// The GPT has no partition with the GUID.
    NoPartition,
    /// Several of the GPT's partitions have the GUID.
    SeveralPartitions,
    /// The partition with the GUID is an EFI system partition now.
    EspPartition,
    /// Exactly one partition, which is not an EFI system partition.
    Found,
}

/// Why the chosen partition can no longer be booted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stale {
    /// No disk on this PC reports the chosen serial: the disk was replaced
    /// or unplugged, or the USB moved to another PC.
    Absent,
    /// Several disks report the serial, so none can be told to be it.
    Ambiguous,
    /// The disk is there but has no partition table: it was wiped.
    Wiped,
    /// The disk is there but the chosen partition is not: it was deleted
    /// or the disk repartitioned.
    PartitionGone,
    /// The partition is there but holds no Mac system and its NVRAM store
    /// cannot be read, as an interrupted install leaves it.
    NoMacOs,
}

impl Stale {
    /// The record for a search that did not find exactly the chosen
    /// partition; `None` when it did.
    pub fn of(search: Search) -> Option<Self> {
        match search {
            Search::NoDisk => Some(Self::Absent),
            Search::SeveralDisks => Some(Self::Ambiguous),
            Search::NoTable => Some(Self::Wiped),
            Search::NoPartition | Search::SeveralPartitions | Search::EspPartition => {
                Some(Self::PartitionGone)
            }
            Search::Found => None,
        }
    }

    /// The case, for the one log line (`CHOSEN DISK stale: ...`).
    pub fn case(self) -> &'static str {
        match self {
            Self::Absent => "no disk on this PC has the chosen serial",
            Self::Ambiguous => "several disks on this PC report the chosen serial",
            Self::Wiped => "the chosen disk has no partition table",
            Self::PartitionGone => "the chosen disk no longer has the chosen partition",
            Self::NoMacOs => "the chosen partition holds no macOS and no readable NVRAM",
        }
    }

    /// Why the macOS choice is left out of the menu.
    pub fn reason(self) -> &'static str {
        match self {
            Self::NoMacOs => "the disk chosen before has no macOS",
            _ => "the disk chosen before is not there",
        }
    }

    /// The two lines the menu shows while it waits for Install macOS.
    pub fn screen(self) -> &'static str {
        match self {
            Self::NoMacOs => {
                "The disk chosen before has no macOS.\nInstall macOS will choose again."
            }
            _ => "The disk chosen before is not there.\nInstall macOS will choose again.",
        }
    }
}

/// Which partition the macOS choice starts from when the USB holds no
/// record: a USB made new, or made again, has none, yet the PC may hold the
/// macOS an earlier USB installed. Offering only Install macOS there offers
/// only to erase it. A USB with a record keeps it whatever it starts: its
/// disk may be out for now or its install unfinished.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Adopted {
    /// The USB holds a record: nothing is looked for.
    Recorded,
    /// The one partition on this PC's disks that starts macOS, by its place
    /// among those found. The record names it once the macOS choice is
    /// taken.
    One(usize),
    /// No partition found starts macOS: only Install macOS is offered.
    None,
    /// This many partitions start macOS, so none can be told to be the one
    /// meant: only Install macOS is offered, and its picker shows them.
    Several(usize),
}

/// What the macOS choice starts from. `recorded` is whether the USB holds
/// a record; `found` holds, for each of Stela's macOS partitions
/// on this PC's disks, whether macOS can start from it.
pub fn adopt(recorded: bool, found: &[bool]) -> Adopted {
    if recorded {
        return Adopted::Recorded;
    }
    let mut ready = found.iter().enumerate().filter(|(_, ready)| **ready);
    match (ready.next(), ready.count()) {
        (None, _) => Adopted::None,
        (Some((index, _)), 0) => Adopted::One(index),
        (Some(_), more) => Adopted::Several(more + 1),
    }
}

/// The disk a startup choice takes: `disk` as the record names it, or as
/// it was `found` on the PC's disks. A found partition is the macOS
/// choice's alone (`mac`), so no other choice, an update among them,
/// changes a disk nobody chose for it.
pub fn disk_for(mac: bool, disk: Option<&str>, found: bool) -> Option<&str> {
    match found && !mac {
        true => None,
        false => disk,
    }
}

/// A whole disk Install macOS's picker lists, as far as whether it can be
/// chosen goes.
#[derive(Clone, Copy, Debug)]
pub struct Candidate<'a> {
    /// The serial number its controller reports, by which Stela finds it
    /// again once the firmware has exited.
    pub serial: Option<&'a str>,
    pub bytes: u64,
    /// Why Stela's own drivers cannot reach its controller.
    pub controller: Option<&'static str>,
    /// The logical unit of the USB device it is, from its device path
    /// ([`usb_unit`]); `None` for a disk on another bus, or a USB device
    /// whose path names no unit.
    pub unit: Option<u64>,
}

/// The picker's refusal of a disk whose serial another listed disk also
/// reports: after the firmware exits, Stela finds the chosen disk by its
/// serial alone, and two disks with one serial name neither
/// ([`Search::SeveralDisks`]), so the install would stop.
pub const SAME_SERIAL: &str = "Same serial as another disk";

/// The picker's refusal of a USB device's logical unit other than 0: a
/// bridge's phantom unit, which may read and write unit 0's medium, or a
/// card reader's further slot. Stela's own USB driver names such a unit
/// differently from the firmware once it has exited (UAS by its 8-byte
/// address), so it could not be found again either.
pub const EXTRA_UNIT: &str = "Extra unit of a USB device";

/// The serial a USB disk at logical unit `unit` is named by, as Stela's
/// own USB mass-storage driver names it (`stela_drivers::usb::storage`):
/// unit 0, or a path naming no unit, keeps the device's serial; any other
/// unit `n` is `{serial}:lun{n}`, in decimal as the driver's BOT units are.
pub fn usb_unit_serial(serial: &str, unit: Option<u64>) -> alloc::string::String {
    match unit {
        Some(unit) if unit != 0 => alloc::format!("{serial}:lun{unit}"),
        _ => serial.into(),
    }
}

/// UEFI device path node types and sub-types (UEFI 2.10 section 10.3).
const MESSAGING: u8 = 0x03;
const MESSAGING_SCSI: u8 = 0x02;
const MESSAGING_USB: u8 = 0x05;
const MESSAGING_USB_CLASS: u8 = 0x0f;
const MESSAGING_USB_WWID: u8 = 0x10;
const MESSAGING_LOGICAL_UNIT: u8 = 0x11;
const END: u8 = 0x7f;

/// The logical unit a USB disk's binary device path names after its last
/// USB node: the last Logical Unit node's LUN (edk2's mass-storage driver
/// adds one per unit of a device with several), or a SCSI node's LUN (a
/// vendor UAS stack under SCSI pass-through). `None` when the path has no
/// USB node, no unit node after it, or a malformed node, so the disk keeps
/// the device's serial and two such disks still refuse each other
/// ([`SAME_SERIAL`]).
pub fn usb_unit(path: &[u8]) -> Option<u64> {
    let mut rest = path;
    let mut usb = false;
    let mut unit = None;
    loop {
        let [kind, sub, low, high, ..] = *rest else {
            return None;
        };
        let length = usize::from(u16::from_le_bytes([low, high]));
        if length < 4 || length > rest.len() {
            return None;
        }
        let data = &rest[4..length];
        match (kind, sub) {
            (END, _) => break,
            (MESSAGING, MESSAGING_USB | MESSAGING_USB_CLASS | MESSAGING_USB_WWID) => {
                usb = true;
                unit = None;
            }
            (MESSAGING, MESSAGING_LOGICAL_UNIT) if usb => unit = Some(u64::from(*data.first()?)),
            (MESSAGING, MESSAGING_SCSI) if usb => {
                let lun = data.get(2..4)?;
                unit = Some(u64::from(u16::from_le_bytes([lun[0], lun[1]])));
            }
            _ => {}
        }
        rest = &rest[length..];
    }
    unit
}

/// Why the picker refuses disk `index` of `disks`, which it shows by the
/// disk; `None` when it can be chosen. A disk Stela's drivers cannot reach,
/// a USB device's extra unit, or a disk Stela cannot find again by its
/// serial is refused before its size is.
pub fn picker_refusal(
    disks: &[Candidate<'_>],
    index: usize,
    min_bytes: u64,
) -> Option<&'static str> {
    let disk = disks.get(index)?;
    if disk.controller.is_some() {
        return disk.controller;
    }
    if disk.unit.is_some_and(|unit| unit != 0) {
        return Some(EXTRA_UNIT);
    }
    let shared = |serial: &str| {
        disks
            .iter()
            .enumerate()
            .any(|(other, listed)| other != index && listed.serial == Some(serial))
    };
    match disk.serial {
        None => Some("No serial number"),
        Some(serial) if serial.contains(char::is_whitespace) => Some("Serial number has spaces"),
        Some(serial) if shared(serial) => Some(SAME_SERIAL),
        Some(_) if disk.bytes < min_bytes => Some("Smaller than 60 GB"),
        Some(_) => None,
    }
}

/// What a chosen partition whose NVRAM store could not be read is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unreadable {
    /// The partition holds no Mac system container: an install that did not
    /// finish, which Install macOS repeats.
    Unprepared,
    /// The partition holds a Mac system, or whether it does could not be
    /// read: the boot stops with [`UNREADABLE_STORE`].
    Fatal,
}

/// A store that could not be read on the chosen partition, given whether
/// that partition holds no Mac system container (`blank`, `None` when that
/// could not be read either).
pub fn unreadable_store(blank: Option<bool>) -> Unreadable {
    match blank {
        Some(true) => Unreadable::Unprepared,
        Some(false) | None => Unreadable::Fatal,
    }
}

/// Why the boot stops on [`Unreadable::Fatal`], and what to do.
pub const UNREADABLE_STORE: &str = "NVRAM on the disk chosen for macOS cannot be read: choose \
     Install macOS again, or on another computer plug in this USB and run `stambha repair`";

#[cfg(test)]
mod tests {
    #[test]
    fn a_usb_without_a_usable_record_takes_the_one_macos_on_the_pc() {
        // The record starts macOS: the disks are not looked at.
        assert_eq!(adopt(true, &[true, true]), Adopted::Recorded);
        assert_eq!(adopt(true, &[]), Adopted::Recorded);
        // A new USB on a PC with one installed macOS, beside a partition
        // an interrupted install left.
        assert_eq!(adopt(false, &[true]), Adopted::One(0));
        assert_eq!(adopt(false, &[false, true]), Adopted::One(1));
        // Nothing to start, or no way to tell which is meant.
        assert_eq!(adopt(false, &[]), Adopted::None);
        assert_eq!(adopt(false, &[false, false]), Adopted::None);
        assert_eq!(adopt(false, &[true, false, true]), Adopted::Several(2));
    }

    use super::*;

    #[test]
    fn a_slow_disk_is_marked_but_not_refused() {
        // The caution (`disk_speed::caution`) is no refusal: a spinning disk
        // of any size and a unique serial can still be chosen.
        let disks = [Candidate {
            serial: Some("WD-1"),
            bytes: 1_000_000_000_000,
            controller: None,
            unit: None,
        }];
        assert_eq!(picker_refusal(&disks, 0, 60_000_000_000), None);
    }

    #[test]
    fn a_found_partition_goes_to_the_macos_choice_alone() {
        assert_eq!(disk_for(true, Some("A:1"), true), Some("A:1"));
        assert_eq!(disk_for(false, Some("A:1"), true), None);
        // A recorded one goes to every choice, as before.
        assert_eq!(disk_for(true, Some("A:1"), false), Some("A:1"));
        assert_eq!(disk_for(false, Some("A:1"), false), Some("A:1"));
        assert_eq!(disk_for(false, None, false), None);
    }

    #[test]
    fn the_found_partition_is_not_stale() {
        assert_eq!(Stale::of(Search::Found), None);
    }

    #[test]
    fn a_missing_disk_or_partition_is_stale() {
        assert_eq!(Stale::of(Search::NoDisk), Some(Stale::Absent));
        assert_eq!(Stale::of(Search::SeveralDisks), Some(Stale::Ambiguous));
        assert_eq!(Stale::of(Search::NoTable), Some(Stale::Wiped));
        for search in [
            Search::NoPartition,
            Search::SeveralPartitions,
            Search::EspPartition,
        ] {
            assert_eq!(Stale::of(search), Some(Stale::PartitionGone));
        }
    }

    #[test]
    fn each_stale_case_has_its_own_line_and_two_plain_screen_lines() {
        let all = [
            Stale::Absent,
            Stale::Ambiguous,
            Stale::Wiped,
            Stale::PartitionGone,
            Stale::NoMacOs,
        ];
        for (n, a) in all.iter().enumerate() {
            for b in &all[n + 1..] {
                assert_ne!(a.case(), b.case());
            }
            assert_eq!(a.screen().lines().count(), 2, "{a:?}");
            assert!(a.screen().ends_with("Install macOS will choose again."));
            assert!(a.screen().lines().all(|line| line.len() <= 40), "{a:?}");
        }
        assert!(Stale::Wiped.screen().contains("is not there"));
        assert!(Stale::NoMacOs.screen().contains("has no macOS"));
    }

    #[test]
    fn the_picker_refuses_every_disk_whose_serial_another_disk_reports() {
        const MIN: u64 = 60_000_000_000;
        let disk = |serial: Option<&'static str>, bytes: u64| Candidate {
            serial,
            bytes,
            controller: None,
            unit: None,
        };
        // Two disks behind one USB-SATA bridge, or a cloned NVMe disk,
        // report one serial: neither can be chosen; the third can.
        let disks = [
            disk(Some("S1"), 500_000_000_000),
            disk(Some("S1"), 1_000_000_000_000),
            disk(Some("S2"), 500_000_000_000),
        ];
        let refusals: Vec<_> = (0..disks.len())
            .map(|index| picker_refusal(&disks, index, MIN))
            .collect();
        assert_eq!(refusals, [Some(SAME_SERIAL), Some(SAME_SERIAL), None]);
        // The other refusals keep their order: the controller first, then
        // the serial, then the size.
        let others = [
            Candidate {
                controller: Some("Controller not supported"),
                ..disk(None, 1)
            },
            disk(None, 500_000_000_000),
            disk(Some("A B"), 500_000_000_000),
            disk(Some("S3"), 1),
            disk(Some("S4"), MIN),
        ];
        let refusals: Vec<_> = (0..others.len())
            .map(|index| picker_refusal(&others, index, MIN))
            .collect();
        assert_eq!(
            refusals,
            [
                Some("Controller not supported"),
                Some("No serial number"),
                Some("Serial number has spaces"),
                Some("Smaller than 60 GB"),
                None
            ]
        );
        // No serial is shared by two disks that report none.
        let unknown = [disk(None, 1), disk(None, 1)];
        assert_eq!(picker_refusal(&unknown, 0, MIN), Some("No serial number"));
    }

    #[test]
    fn a_usb_unit_other_than_zero_is_named_as_stelas_driver_names_it() {
        // `stela_drivers::usb::storage` names BOT unit n `{serial}:lun{n}`.
        assert_eq!(usb_unit_serial("ABC", None), "ABC");
        assert_eq!(usb_unit_serial("ABC", Some(0)), "ABC");
        assert_eq!(usb_unit_serial("ABC", Some(1)), "ABC:lun1");
        assert_eq!(usb_unit_serial("ABC", Some(15)), "ABC:lun15");
    }

    #[test]
    fn a_usb_bridges_phantom_unit_is_refused_and_its_real_disk_can_be_chosen() {
        const MIN: u64 = 60_000_000_000;
        let usb = |serial: Option<&'static str>, bytes: u64, unit: Option<u64>| Candidate {
            serial,
            bytes,
            controller: None,
            unit,
        };
        // A bridge whose firmware driver publishes LUN 1 beside the real
        // disk: the same USB serial, named apart by its unit, with the junk
        // capacity of a unit that is not there (64 TiB).
        let phantom = [
            usb(Some("ABC"), 1_000_000_000_000, Some(0)),
            usb(Some("ABC:lun1"), 1 << 46, Some(1)),
        ];
        let refusals: Vec<_> = (0..phantom.len())
            .map(|index| picker_refusal(&phantom, index, MIN))
            .collect();
        assert_eq!(refusals, [None, Some(EXTRA_UNIT)]);
        // An extra unit is refused as one even when it is named alike, has
        // no serial or is too small: no other reason makes it choosable,
        // and the reasons keep their order.
        for extra in [
            usb(Some("ABC"), 1 << 46, Some(1)),
            usb(None, 1 << 46, Some(1)),
            usb(Some("A B"), 1 << 46, Some(2)),
            usb(Some("ABC:lun1"), 1, Some(1)),
        ] {
            let disks = [usb(Some("XYZ"), 1_000_000_000_000, Some(0)), extra];
            assert_eq!(picker_refusal(&disks, 1, MIN), Some(EXTRA_UNIT));
        }
        // The controller's refusal still comes first.
        let unreachable = [Candidate {
            controller: Some("Controller not supported"),
            ..usb(Some("ABC:lun1"), 1 << 46, Some(1))
        }];
        assert_eq!(
            picker_refusal(&unreachable, 0, MIN),
            Some("Controller not supported")
        );
        // Two different devices that report one serial and name no unit,
        // or two handles with one path, still refuse each other.
        for unit in [None, Some(0)] {
            let twins = [
                usb(Some("ABC"), 1_000_000_000_000, unit),
                usb(Some("ABC"), 500_000_000_000, unit),
            ];
            let refusals: Vec<_> = (0..twins.len())
                .map(|index| picker_refusal(&twins, index, MIN))
                .collect();
            assert_eq!(refusals, [Some(SAME_SERIAL), Some(SAME_SERIAL)]);
        }
    }

    /// A device path node of `kind`/`sub` with `data`.
    fn node(kind: u8, sub: u8, data: &[u8]) -> Vec<u8> {
        let mut node = vec![kind, sub];
        node.extend_from_slice(&(4 + data.len() as u16).to_le_bytes());
        node.extend_from_slice(data);
        node
    }

    /// `PciRoot(0x0)/Pci(0x14,0x0)/USB(0x3,0x0)` followed by `tail` and
    /// the end node.
    fn usb_path(tail: &[Vec<u8>]) -> Vec<u8> {
        let mut path = node(0x02, 0x01, &[0xd0, 0x41, 0x03, 0x0a, 0, 0, 0, 0]);
        path.extend(node(0x01, 0x01, &[0x00, 0x14]));
        path.extend(node(MESSAGING, MESSAGING_USB, &[3, 0]));
        for tail in tail {
            path.extend_from_slice(tail);
        }
        path.extend(node(END, 0xff, &[]));
        path
    }

    #[test]
    fn a_usb_disks_unit_is_read_from_its_device_path() {
        let unit = |lun: u8| node(MESSAGING, MESSAGING_LOGICAL_UNIT, &[lun]);
        let scsi = |pun: u16, lun: u16| {
            let mut data = pun.to_le_bytes().to_vec();
            data.extend_from_slice(&lun.to_le_bytes());
            node(MESSAGING, MESSAGING_SCSI, &data)
        };
        assert_eq!(usb_unit(&usb_path(&[unit(1)])), Some(1));
        assert_eq!(usb_unit(&usb_path(&[unit(0)])), Some(0));
        assert_eq!(usb_unit(&usb_path(&[])), None);
        assert_eq!(usb_unit(&usb_path(&[scsi(0, 1)])), Some(1));
        assert_eq!(usb_unit(&usb_path(&[scsi(0, 0)])), Some(0));
        // The last unit node counts.
        assert_eq!(usb_unit(&usb_path(&[unit(0), unit(2)])), Some(2));
        // A unit before the USB node is not the USB device's: a USB
        // controller behind a SCSI one names no unit of its own.
        let mut before = node(MESSAGING, MESSAGING_LOGICAL_UNIT, &[1]);
        before.extend(node(MESSAGING, MESSAGING_USB, &[3, 0]));
        before.extend(node(END, 0xff, &[]));
        assert_eq!(usb_unit(&before), None);
        // A hub's port: the unit belongs to the device after the last USB
        // node, and one before it is forgotten.
        let hub = usb_path(&[unit(1), node(MESSAGING, MESSAGING_USB, &[2, 0])]);
        assert_eq!(usb_unit(&hub), None);
        let hub = usb_path(&[node(MESSAGING, MESSAGING_USB, &[2, 0]), unit(1)]);
        assert_eq!(usb_unit(&hub), Some(1));
        // USB WWID and class nodes are USB devices too.
        let mut wwid = node(MESSAGING, MESSAGING_USB_WWID, &[0; 6]);
        wwid.extend(unit(1));
        wwid.extend(node(END, 0xff, &[]));
        assert_eq!(usb_unit(&wwid), Some(1));
        // No USB node: a SATA or SCSI disk's unit is not read.
        let mut sata = node(0x02, 0x01, &[0xd0, 0x41, 0x03, 0x0a, 0, 0, 0, 0]);
        sata.extend(scsi(0, 1));
        sata.extend(node(END, 0xff, &[]));
        assert_eq!(usb_unit(&sata), None);
        // A malformed path names no unit: a node shorter than its header,
        // one longer than the path, a unit node without its LUN, or no end.
        let mut short = usb_path(&[unit(1)]);
        let at = short.len() - 9;
        short[at + 2] = 3;
        assert_eq!(usb_unit(&short), None);
        let mut long = usb_path(&[unit(1)]);
        let at = long.len() - 9;
        long[at + 2] = 200;
        assert_eq!(usb_unit(&long), None);
        assert_eq!(
            usb_unit(&usb_path(&[node(MESSAGING, MESSAGING_LOGICAL_UNIT, &[])])),
            None
        );
        let mut endless = usb_path(&[unit(1)]);
        endless.truncate(endless.len() - 4);
        assert_eq!(usb_unit(&endless), None);
        assert_eq!(usb_unit(&[]), None);
    }

    #[test]
    fn an_unreadable_store_without_a_system_is_an_unfinished_install() {
        assert_eq!(unreadable_store(Some(true)), Unreadable::Unprepared);
        assert_eq!(unreadable_store(Some(false)), Unreadable::Fatal);
        assert_eq!(unreadable_store(None), Unreadable::Fatal);
        assert!(UNREADABLE_STORE.contains("Install macOS"));
        assert!(UNREADABLE_STORE.contains("`stambha repair`"));
    }
}
