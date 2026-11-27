//! Whether the partition Install macOS chose for the Mac's SSD
//! (`EFI/stela/disk.cfg`, `SERIAL:GUID`) is still the one to boot.
//!
//! The USB keeps that record when the disk is later replaced, wiped,
//! repartitioned or unplugged, and when the USB moves to another PC. Such a
//! record is stale: the macOS choice is left out, Install macOS chooses a
//! disk again, and the record is replaced only when a new disk is
//! confirmed. Only a store that cannot be read on a partition that is
//! otherwise the chosen one, with a Mac system on it, still stops the boot.

use stela_formats::payload::gpt;

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
    /// Why its geometry holds no partition table ([`size_refusal`]).
    pub size: Option<&'static str>,
    /// Why Stela's own drivers cannot reach its controller.
    pub controller: Option<&'static str>,
}

/// The picker's refusal of a disk that reports a block size of 0 or no
/// blocks, as a UFS logical unit without media does.
pub const NO_SIZE: &str = "Reports no size";
/// The picker's refusal of a disk with fewer blocks than both copies of a
/// GPT take.
pub const NO_ROOM_FOR_TABLE: &str = "Too small for a partition table";

/// Why a disk of `geometry`, as its firmware reports it, cannot hold a GPT;
/// `None` when it can. The picker shows such a disk refused, and nothing
/// reads or lays out a GPT on it.
pub fn size_refusal(geometry: gpt::Disk) -> Option<&'static str> {
    if !geometry.reports_size() {
        Some(NO_SIZE)
    } else if geometry.usable().is_none() {
        Some(NO_ROOM_FOR_TABLE)
    } else {
        None
    }
}

/// The picker's refusal of a disk whose serial another listed disk also
/// reports: after the firmware exits, Stela finds the chosen disk by its
/// serial alone, and two disks with one serial name neither
/// ([`Search::SeveralDisks`]), so the install would stop.
pub const SAME_SERIAL: &str = "Same serial as another disk";

/// Why the picker refuses disk `index` of `disks`, which it shows by the
/// disk; `None` when it can be chosen. A disk that reports no size, or too
/// small a one for a GPT, is refused first, as nothing else about it can be
/// used; then one Stela's drivers cannot reach or cannot find again by its
/// serial, before its size is.
pub fn picker_refusal(
    disks: &[Candidate<'_>],
    index: usize,
    min_bytes: u64,
) -> Option<&'static str> {
    let disk = disks.get(index)?;
    if disk.size.is_some() {
        return disk.size;
    }
    if disk.controller.is_some() {
        return disk.controller;
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
            size: None,
            controller: None,
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
            size: None,
            controller: None,
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
    fn a_disk_without_a_size_or_room_for_a_gpt_is_refused_by_its_size() {
        let geometry = |block_bytes, blocks| gpt::Disk {
            block_bytes,
            blocks,
        };
        for none in [geometry(0, 0), geometry(0, 1 << 20), geometry(512, 0)] {
            assert_eq!(size_refusal(none), Some(NO_SIZE), "{none:?}");
        }
        for tiny in [geometry(512, 1), geometry(512, 68), geometry(4096, 12)] {
            assert_eq!(size_refusal(tiny), Some(NO_ROOM_FOR_TABLE), "{tiny:?}");
        }
        for fits in [
            geometry(512, 69),
            geometry(4096, 13),
            geometry(512, 1_953_458_176),
            geometry(4096, 31_246_336),
        ] {
            assert_eq!(size_refusal(fits), None, "{fits:?}");
        }
    }

    #[test]
    fn the_picker_refuses_a_disk_that_reports_no_size_by_that() {
        const MIN: u64 = 60_000_000_000;
        // The two UFS logical units a Lenovo 500w Gen 5 listed: 128 GB
        // with a GPT, and one of 0 bytes. Both sit behind a controller
        // Stela has no driver for and report no serial; the empty one is
        // refused for having no size, so nothing lays out a GPT on it.
        let ufs = |bytes, geometry| Candidate {
            serial: None,
            bytes,
            size: size_refusal(geometry),
            controller: Some("Stambha has no driver for its controller"),
        };
        let disks = [
            ufs(
                127_984_992_256,
                gpt::Disk {
                    block_bytes: 4096,
                    blocks: 127_984_992_256 / 4096,
                },
            ),
            ufs(
                0,
                gpt::Disk {
                    block_bytes: 0,
                    blocks: 0,
                },
            ),
        ];
        let refusals: Vec<_> = (0..disks.len())
            .map(|index| picker_refusal(&disks, index, MIN))
            .collect();
        assert_eq!(
            refusals,
            [
                Some("Stambha has no driver for its controller"),
                Some(NO_SIZE)
            ]
        );
        // Its size is refused before anything else it lacks.
        let alone = [Candidate {
            serial: Some("S1"),
            bytes: 0,
            size: Some(NO_SIZE),
            controller: None,
        }];
        assert_eq!(picker_refusal(&alone, 0, MIN), Some(NO_SIZE));
        let tiny = [Candidate {
            size: size_refusal(gpt::Disk {
                block_bytes: 512,
                blocks: 8,
            }),
            bytes: 4096,
            ..alone[0]
        }];
        assert_eq!(picker_refusal(&tiny, 0, MIN), Some(NO_ROOM_FOR_TABLE));
        // A disk that holds a GPT is judged as before.
        let sized = [Candidate {
            size: None,
            bytes: 500_000_000_000,
            ..alone[0]
        }];
        assert_eq!(picker_refusal(&sized, 0, MIN), None);
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
