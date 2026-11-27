use super::*;
use alloc::collections::BTreeMap;
use stela_drivers::block::{DiscardLimits, Features, Identity, RamDisk, Transport, WriteZeroes};

pub(in crate::platform::pc) const SERIAL: &str = "FAKE-SERIAL-0001";
const NAME: &str = "Stela macOS";
pub(in crate::platform::pc) const NEW: &str = "CCCCCCCC-0000-4000-8000-000000000001";
const DISK: &str = "DDDDDDDD-0000-0000-0000-000000000001";
const ESP: &str = "C12A7328-F81F-11D2-BA4B-00A0C93EC93B";
const RESERVED: &str = "E3C9E316-0B5C-4DB8-817D-F92DF00215AE";
const WINDOWS: &str = "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7";
const RECOVERY: &str = "DE94BBA4-06D1-4D40-A16A-BFD50179D6AC";
/// The Windows partition.
const C: &str = "AAAAAAAA-0000-0000-0000-000000000003";
pub(in crate::platform::pc) const GIB: u64 = 1 << 30;
/// The disk: 128 GiB.
pub(in crate::platform::pc) const BYTES: u64 = 128 * GIB;
/// The free space between Windows and its recovery partition, in MiB.
const FREE: (u64, u64) = (40 * 1024, 70 * 1024);

pub(in crate::platform::pc) fn guid(text: &str) -> Guid {
    text.parse().unwrap()
}

/// What a disk was asked to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::platform::pc) enum Done {
    Write { lba: u64, blocks: u64 },
    Zeroes { lba: u64, blocks: u64 },
    Discard { lba: u64, blocks: u64 },
    Flush,
}

/// A disk of [`BYTES`] holding only the blocks written to it, every other
/// block reading as 0xa5 but near its ends, which read as zero; it keeps
/// what it was asked to do.
pub(in crate::platform::pc) struct Sparse {
    pub(in crate::platform::pc) identity: Identity,
    blocks: BTreeMap<u64, Vec<u8>>,
    pub(in crate::platform::pc) done: Vec<Done>,
    /// Writes from these blocks are taken and kept nowhere.
    lost: core::ops::Range<u64>,
    /// A write or zeroing of this block fails.
    fails_at: Option<u64>,
    /// A write to the first block also changes the second's first byte,
    /// as a write that lands in two places.
    scribble: Option<(u64, u64)>,
}

impl Sparse {
    pub(in crate::platform::pc) fn new(block: u64) -> Self {
        Self {
            identity: Identity {
                transport: Transport::Nvme,
                location: "test".into(),
                model: "test".into(),
                serial: SERIAL.into(),
                blocks: BYTES / block,
                block_size: block as u32,
                read_only: false,
                features: Features {
                    write_cache: true,
                    fua: false,
                    max_transfer: 1 << 20,
                    discard: Some(DiscardLimits {
                        blocks: u64::from(u32::MAX),
                        ranges: 256,
                    }),
                    write_zeroes: Some(WriteZeroes {
                        blocks: 65536,
                        unmap: true,
                    }),
                    ..Features::default()
                },
            },
            blocks: BTreeMap::new(),
            done: Vec::new(),
            lost: 0..0,
            fails_at: None,
            scribble: None,
        }
    }

    /// The disk holding `table` as Windows leaves one: [`shared::encode`]'s
    /// blocks, its protective MBR with a disk signature.
    pub(in crate::platform::pc) fn with(block: u64, table: &Table) -> Self {
        let mut disk = Self::new(block);
        for write in shared::encode(gpt::geometry(&disk.identity), table).unwrap() {
            disk.put(write.lba, &write.bytes);
        }
        let mut mbr = disk.block_at(0);
        mbr[440..444].copy_from_slice(&[0xf4, 0x08, 0xdb, 0xc8]);
        disk.put(0, &mbr);
        disk
    }

    fn block_bytes(&self) -> usize {
        self.identity.block_size as usize
    }

    fn last(&self) -> u64 {
        self.identity.blocks - 1
    }

    fn block_at(&self, lba: u64) -> Vec<u8> {
        match self.blocks.get(&lba) {
            Some(bytes) => bytes.clone(),
            None if lba < 64 || lba >= self.identity.blocks - 64 => vec![0; self.block_bytes()],
            None => vec![0xa5; self.block_bytes()],
        }
    }

    fn put(&mut self, lba: u64, data: &[u8]) {
        let block = self.block_bytes();
        for (index, part) in data.chunks(block).enumerate() {
            self.blocks.insert(lba + index as u64, part.to_vec());
        }
    }

    /// `blocks` blocks from `lba`, as they read.
    pub(in crate::platform::pc) fn bytes(&self, lba: u64, blocks: u64) -> Vec<u8> {
        (lba..lba + blocks)
            .flat_map(|lba| self.block_at(lba))
            .collect()
    }

    /// The disk's GPT blocks, as the picker reads them.
    pub(in crate::platform::pc) fn ends(&self) -> Ends {
        let [head, tail] = Ends::spans(gpt::geometry(&self.identity)).unwrap();
        Ends {
            head: self.bytes(head.lba, head.blocks),
            tail: self.bytes(tail.lba, tail.blocks),
        }
    }

    /// What the disk was asked to change, flushes left out.
    fn changes(&self) -> Vec<Done> {
        self.done
            .iter()
            .copied()
            .filter(|done| *done != Done::Flush)
            .collect()
    }

    fn fails(&self, lba: u64, blocks: u64) -> bool {
        self.fails_at
            .is_some_and(|at| (lba..lba + blocks).contains(&at))
    }
}

impl BlockDevice for Sparse {
    fn identity(&self) -> &Identity {
        &self.identity
    }

    fn read(&mut self, lba: u64, buffer: &mut [u8]) -> Result<(), BlockError> {
        self.identity.check(lba, buffer.len())?;
        let block = self.block_bytes();
        for (index, part) in buffer.chunks_mut(block).enumerate() {
            part.copy_from_slice(&self.block_at(lba + index as u64));
        }
        Ok(())
    }

    fn write(&mut self, lba: u64, data: &[u8]) -> Result<(), BlockError> {
        let blocks = self.identity.check_write(lba, data.len())?;
        self.done.push(Done::Write { lba, blocks });
        if self.fails(lba, blocks) {
            return Err(BlockError::Timeout);
        }
        if !self.lost.contains(&lba) {
            self.put(lba, data);
        }
        if let Some((_, victim)) = self.scribble.filter(|(at, _)| *at == lba) {
            let mut block = self.block_at(victim);
            block[0] ^= 1;
            self.put(victim, &block);
        }
        Ok(())
    }

    fn flush_cache(&mut self) -> Result<(), BlockError> {
        self.done.push(Done::Flush);
        Ok(())
    }

    fn discard(&mut self, lba: u64, blocks: u64) -> Result<(), BlockError> {
        self.done.push(Done::Discard { lba, blocks });
        Ok(())
    }

    fn write_zeroes(&mut self, lba: u64, blocks: u64, _unmap: bool) -> Result<(), BlockError> {
        self.identity
            .check_write(lba, (blocks as usize) * self.block_bytes())?;
        self.done.push(Done::Zeroes { lba, blocks });
        if self.fails(lba, blocks) {
            return Err(BlockError::Timeout);
        }
        let zero = vec![0; self.block_bytes()];
        for lba in lba..lba + blocks {
            self.blocks.insert(lba, zero.clone());
        }
        Ok(())
    }
}

fn partition(kind: &str, unique: &str, mib: (u64, u64), block: u64, name: &str) -> Partition {
    let (first, size) = mib;
    Partition {
        type_guid: guid(kind),
        unique_guid: guid(unique),
        first_lba: first * MIB / block,
        last_lba: (first + size) * MIB / block - 1,
        attributes: 0,
        name: name.into(),
    }
}

/// A disk as Windows leaves one after its partition was shrunk: an ESP, a
/// reserved partition, Windows to 40 GiB, then 70 GiB free, a recovery
/// partition and another, in entries 1, 2, 3, 5 and 6; entry 4 is unused,
/// as a partition deleted leaves one.
fn layout(block: u64) -> Table {
    Table {
        disk_guid: guid(DISK),
        partitions: vec![
            partition(
                ESP,
                "AAAAAAAA-0000-0000-0000-000000000001",
                (1, 100),
                block,
                "EFI system partition",
            ),
            partition(
                RESERVED,
                "AAAAAAAA-0000-0000-0000-000000000002",
                (101, 16),
                block,
                "Microsoft reserved partition",
            ),
            partition(
                WINDOWS,
                C,
                (117, 40 * 1024 - 117),
                block,
                "Basic data partition",
            ),
            partition(
                RECOVERY,
                "AAAAAAAA-0000-0000-0000-000000000005",
                (110 * 1024, 1024),
                block,
                "",
            ),
            partition(
                WINDOWS,
                "AAAAAAAA-0000-0000-0000-000000000006",
                (111 * 1024, 1024),
                block,
                "Basic data partition",
            ),
        ],
        slots: vec![0, 1, 2, 4, 5],
    }
}

/// [`layout`] with a Stela partition in its free space, in entry 4.
fn with_stela(block: u64) -> Table {
    let mut table = layout(block);
    table.add(Partition {
        type_guid: Role::AppleRegion.type_guid(),
        unique_guid: guid(NEW),
        first_lba: FREE.0 * MIB / block,
        last_lba: (FREE.0 + FREE.1) * MIB / block - 1,
        attributes: 0,
        name: NAME.into(),
    });
    table
}

fn free(block: u64) -> Place {
    Place::Free {
        first_lba: FREE.0 * MIB / block,
        bytes: FREE.1 * MIB,
    }
}

fn decision<'a>(
    disk: &Sparse,
    ends: &'a Ends,
    table: Option<&'a Table>,
    place: Place,
) -> Decision<'a> {
    Decision {
        serial: SERIAL,
        geometry: gpt::geometry(&disk.identity),
        ends,
        table,
        place,
        name: NAME,
        mode: Mode::Install,
    }
}

/// The plan of `place` on `disk`, which holds `table`, the new partition
/// [`NEW`].
fn planned(disk: &Sparse, table: &Table, place: Place, mode: Mode) -> Plan {
    let ends = disk.ends();
    plan(
        &Decision {
            mode,
            ..decision(disk, &ends, Some(table), place)
        },
        &mut || guid(NEW),
    )
    .unwrap()
}

/// The table `plan`'s writes hold, read from its primary's.
fn written(plan: &Plan) -> Table {
    let at = |lba| {
        &plan
            .table
            .iter()
            .find(|write| write.lba == lba)
            .unwrap()
            .bytes
    };
    let info = shared::parse_header(plan.geometry, 1, at(1))
        .unwrap()
        .unwrap();
    shared::parse_entries(&info, at(2)).unwrap()
}

/// `plan` with its table encoded again after `edit`, and its partition
/// what `edit` leaves it.
fn rewritten(plan: &Plan, edit: impl FnOnce(&mut Table, &mut Planned)) -> Plan {
    let mut table = written(plan);
    let mut partition = plan.partition;
    edit(&mut table, &mut partition);
    let mut writes = shared::encode(plan.geometry, &table).unwrap();
    writes.retain(|write| write.lba != 0);
    Plan {
        table: writes,
        partition,
        ..plan.clone()
    }
}

/// The new partition in `table`.
fn new_in(table: &mut Table) -> &mut Partition {
    table
        .partitions
        .iter_mut()
        .find(|partition| partition.unique_guid == guid(NEW))
        .unwrap()
}

/// `plan` as the picker would have made it had it read `disk` as it is
/// now: its GPT blocks and first blocks `disk`'s, its writes `plan`'s.
fn as_read(plan: &Plan, disk: &Sparse) -> Plan {
    Plan {
        ends: disk.ends(),
        seen: seen(&disk.bytes(0, SEEN_BLOCKS)),
        ..plan.clone()
    }
}

/// `header` with `value` at byte `at` and its checksum made anew, as
/// another tool writes a field of it.
fn relabel(header: &mut [u8], at: usize, value: &[u8]) {
    header[at..at + value.len()].copy_from_slice(value);
    let size = u32::from_le_bytes(header[12..16].try_into().unwrap()) as usize;
    header[16..20].fill(0);
    let crc = shared::crc32(&header[..size]);
    header[16..20].copy_from_slice(&crc.to_le_bytes());
}

impl Sparse {
    /// A disk of `blocks` blocks of `block` bytes holding `table` as
    /// another tool lays one out: [`shared::encode`]'s blocks, each header
    /// then changed by `edit`, given its LBA, and a protective MBR with a
    /// disk signature.
    fn laid_out(block: u64, blocks: u64, table: &Table, edit: impl Fn(u64, &mut [u8])) -> Self {
        let mut disk = Self::new(block);
        disk.identity.blocks = blocks;
        for write in shared::encode(gpt::geometry(&disk.identity), table).unwrap() {
            disk.put(write.lba, &write.bytes);
        }
        for lba in [1, blocks - 1] {
            let mut header = disk.block_at(lba);
            edit(lba, &mut header);
            disk.put(lba, &header);
        }
        let mut mbr = disk.block_at(0);
        mbr[440..444].copy_from_slice(&[0x5e, 0x1f, 0x7a, 0x0b]);
        disk.put(0, &mbr);
        disk
    }
}

/// Linux's fdisk (libfdisk, also behind sfdisk, cfdisk and GNOME Disks):
/// the usable blocks from 1 MiB, the last and the rest as Windows'.
fn fdisk(block: u64) -> impl Fn(u64, &mut [u8]) {
    move |_: u64, header: &mut [u8]| relabel(header, 40, &(MIB / block).to_le_bytes())
}

/// Windows, gdisk, parted and macOS: the header as [`shared::encode`]'s.
fn standard(_: u64, _: &mut [u8]) {}

/// The usable blocks `disk`'s primary header names.
fn usable_of(disk: &Sparse) -> (u64, u64) {
    let geometry = gpt::geometry(&disk.identity);
    let info = shared::parse_header(geometry, 1, &disk.block_at(1))
        .unwrap()
        .unwrap();
    (info.first_usable, info.last_usable)
}

/// The free space after `table`'s last partition as the picker offers
/// it: from a MiB boundary, to the last MiB boundary of the usable blocks,
/// all of it.
fn free_after(disk: &Sparse, table: &Table) -> Place {
    let block = u64::from(disk.identity.block_size);
    let mib = MIB / block;
    let (first, last) = usable_of(disk);
    let start = table
        .partitions
        .iter()
        .map(|partition| partition.last_lba + 1)
        .fold(first, u64::max)
        .div_ceil(mib)
        * mib;
    let end = (last + 1) / mib * mib;
    Place::Free {
        first_lba: start,
        bytes: (end - start) * block / MIB * MIB,
    }
}

/// Check only, then install, macOS in the free space after the last of
/// the partitions `disk` holds, as the picker reads them; and prove what
/// the install leaves: every write in the GPT's own blocks or the new
/// partition, the MBR, every partition's entry and every header byte but
/// the two checksums as they were, both copies whole and alike with the
/// same layout, and the table the disk's with one partition more.
fn installs_beside(mut disk: Sparse, what: &str) -> Plan {
    let geometry = gpt::geometry(&disk.identity);
    let block = geometry.block_bytes as usize;
    let table = gpt::read(&mut disk).unwrap();
    let before = disk.ends();
    let made = planned(&disk, &table, free_after(&disk, &table), Mode::Check);
    let (result, lines) = checked(&made, &mut disk);
    assert!(result.is_ok(), "{what}: {result:?}\n{lines:#?}");
    assert_eq!(disk.done, [], "{what}");
    let made = Plan {
        mode: Mode::Install,
        ..made
    };
    made.apply(&mut disk)
        .unwrap_or_else(|error| panic!("{what}: {error}"));
    let [head, tail] = Ends::spans(geometry).unwrap();
    let (first, end) = (
        made.partition.first_lba,
        made.partition.first_lba + made.partition.blocks,
    );
    for change in disk.changes() {
        let (Done::Write { lba, blocks } | Done::Zeroes { lba, blocks }) = change else {
            panic!("{what}: {change:?}");
        };
        assert!(
            lba + blocks <= head.blocks || lba >= tail.lba || (lba >= first && lba + blocks <= end),
            "{what}: {change:?}"
        );
    }
    let after = disk.ends();
    assert_eq!(after.head[..block], before.head[..block], "{what}");
    let entries = geometry.entry_blocks() as usize * block;
    let array = |ends: &Ends| ends.head[2 * block..2 * block + entries].to_vec();
    let (was, now) = (array(&before), array(&after));
    let changed: Vec<usize> = (0..128)
        .filter(|slot| was[slot * 128..][..128] != now[slot * 128..][..128])
        .collect();
    assert_eq!(changed.len(), 1, "{what}");
    assert!(was[changed[0] * 128..][..128].iter().all(|&byte| byte == 0));
    assert_eq!(after.tail[..entries], now[..], "{what}");
    let headers = |ends: &Ends| {
        (
            ends.head[block..2 * block].to_vec(),
            ends.tail[entries..].to_vec(),
        )
    };
    let (was_headers, now_headers) = (headers(&before), headers(&after));
    for (was, now) in [
        (&was_headers.0, &now_headers.0),
        (&was_headers.1, &now_headers.1),
    ] {
        let changed: Vec<usize> = (0..block).filter(|&at| was[at] != now[at]).collect();
        assert!(
            changed
                .iter()
                .all(|at| CHECKSUMS.iter().any(|c| c.contains(at))),
            "{what}: {changed:?}"
        );
    }
    assert_eq!(
        shared::kept_layout(geometry, &now_headers.0, &now_headers.1),
        shared::kept_layout(geometry, &was_headers.0, &was_headers.1),
        "{what}"
    );
    // As the firmware and the systems on the disk read it: both copies
    // whole and alike, and a protective MBR.
    let found = shared::read(geometry, |lba, blocks| Ok(disk.bytes(lba, blocks))).unwrap();
    let shared::Found::Table {
        table: read,
        repair: None,
    } = found
    else {
        panic!("{what}: {found:?}");
    };
    assert_eq!(read, written(&made), "{what}");
    let mut kept = read.clone();
    let new = kept
        .partitions
        .iter()
        .position(|partition| partition.unique_guid == guid(NEW))
        .unwrap();
    assert_eq!(kept.slot(new) as usize, changed[0], "{what}");
    kept.remove(new);
    assert_eq!(kept, table, "{what}");
    made
}

const LINUX: &str = "0FC63DAF-8483-4772-8E79-3D69D8477DE4";
const APFS: &str = "7C3457EF-0000-11AA-AA11-00306543ECAC";
/// The tester's NVMe disk: a Samsung 970 EVO Plus, 500 GB.
const EVO: u64 = 976_773_168;
/// The tester's SATA disk: a PNY, 250 GB.
const PNY: u64 = 488_397_168;

fn blocks_partition(kind: &str, tag: u8, first: u64, last: u64, name: &str) -> Partition {
    Partition {
        type_guid: guid(kind),
        unique_guid: Guid([tag; 16]),
        first_lba: first,
        last_lba: last,
        attributes: 0,
        name: name.into(),
    }
}

fn table_of(partitions: Vec<Partition>) -> Table {
    Table {
        disk_guid: guid(DISK),
        partitions,
        slots: Vec::new(),
    }
}

/// The tester's SATA disk as the log shows it: a Linux filesystem from
/// 1 MiB, then a 2 GiB ESP, ending where the plan's free space starts.
fn pny() -> Table {
    table_of(vec![
        blocks_partition(LINUX, 1, 2048, 103_426_047, ""),
        blocks_partition(ESP, 2, 103_426_048, 107_620_351, ""),
    ])
}

/// Check `plan` on `disk`; its lines.
fn checked(plan: &Plan, disk: &mut Sparse) -> (Result<Checked, ApplyError>, Vec<String>) {
    let mut lines = Vec::new();
    let result = plan.check(&mut ReadOnly::new(disk), &mut |line| {
        lines.push(String::from(line))
    });
    (result, lines)
}

#[test]
fn a_new_partition_takes_the_first_unused_entry_and_the_mbr_stays() {
    for block in [512, 4096] {
        let table = layout(block);
        let disk = Sparse::with(block, &table);
        let made = planned(&disk, &table, free(block), Mode::Install);
        let first = FREE.0 * MIB / block;
        assert_eq!(
            made.partition,
            Planned {
                guid: guid(NEW),
                first_lba: first,
                blocks: FREE.1 * MIB / block,
            }
        );
        assert_eq!(
            made.target(),
            format!("{SERIAL}:{NEW}").parse::<Target>().unwrap()
        );
        assert_eq!(made.ends, disk.ends());
        assert_eq!(made.seen, seen(&disk.bytes(0, SEEN_BLOCKS)));
        // The first MiB, and the machine state: the last 16 MiB, from a
        // MiB boundary.
        assert_eq!(
            made.zero,
            [
                Extent {
                    lba: first,
                    blocks: MIB / block
                },
                Extent {
                    lba: first + (FREE.1 - 16) * MIB / block,
                    blocks: 16 * MIB / block
                },
            ]
        );
        // The backup copy first, the primary header after its entries,
        // and no MBR: the disk's is valid.
        let last = BYTES / block - 1;
        let entry_blocks = 16384 / block;
        let lbas: Vec<u64> = made.table.iter().map(|write| write.lba).collect();
        assert_eq!(lbas, [last - entry_blocks, last, 2, 1], "{block}");
        let new = written(&made);
        assert_eq!(new.disk_guid, guid(DISK));
        // In entry 4, which was unused, though it lies before entry 5 on
        // the disk: the others keep their entries, and so their numbers.
        assert_eq!(new.slots, [0, 1, 2, 3, 4, 5]);
        assert_eq!(new.partitions[3].unique_guid, guid(NEW));
        assert_eq!(new.partitions[3].name, NAME);
        assert_eq!(new.partitions[3].role(), Some(Role::AppleRegion));
        let mut kept = new.partitions.clone();
        kept.remove(3);
        assert_eq!(kept, table.partitions);
    }
}

#[test]
fn checking_says_every_write_and_writes_nothing() {
    let table = layout(512);
    let mut disk = Sparse::with(512, &table);
    let made = planned(&disk, &table, free(512), Mode::Check);
    let (result, lines) = checked(&made, &mut disk);
    let found = result.unwrap();
    assert_eq!(found.table.as_ref(), Some(&written(&made)));
    assert_eq!(disk.done, []);
    for line in [
        "PLAN check only: nothing below is written",
        "PLAN the disk: 268435456 blocks of 512 bytes",
        "PLAN the macOS partition: new, at LBA 83886080-230686719 (146800640 blocks, \
         75161927680 bytes)",
        "PLAN write 1: a trial write in the new partition, read back and zeroed below, LBA \
         83886080-83886111 (32 blocks, 16384 bytes)",
        "PLAN write 2: the backup GPT entries, LBA 268435423-268435454 (32 blocks, 16384 bytes)",
        "PLAN write 3: the backup GPT header, LBA 268435455 (1 block, 512 bytes)",
        "PLAN write 4: the primary GPT entries, LBA 2-33 (32 blocks, 16384 bytes)",
        "PLAN write 5: the primary GPT header, LBA 1 (1 block, 512 bytes)",
        "PLAN the protective MBR at LBA 0 is valid: it is not written",
        "PLAN zero 1: the macOS partition's first MiB, LBA 83886080-83888127 (2048 blocks, \
         1048576 bytes), by Write Zeroes, not TRIM",
        "PLAN zero 2: the machine state in its tail, LBA 230653952-230686719 (32768 blocks, \
         16777216 bytes), by Write Zeroes, not TRIM",
        "PLAN nothing else is written, and nothing is trimmed",
        "PASS the disk has 268435456 blocks of 512 bytes, as the picker saw",
        "PASS the primary GPT's blocks, LBA 0-33 (34 blocks, 17408 bytes), read through \
         Stela's driver are byte for byte what the picker read through the firmware",
        "PASS the backup GPT's blocks, LBA 268435423-268435455 (33 blocks, 16896 bytes), read \
         through Stela's driver are byte for byte what the picker read through the firmware",
        "PASS both GPT copies are whole and alike: headers at LBA 1 and 268435455, 128 entries \
         of 128 bytes at LBA 2 and 268435423, usable LBA 34-268435422, 5 partitions",
        "PASS the new partition, LBA 83886080-230686719 (146800640 blocks, 75161927680 bytes), \
         lies in free space: it overlaps none of the disk's 5 partitions and holds at least \
         60017825792 bytes",
        "PASS entry 3 stays byte for byte: \"Basic data partition\", type \
         EBD0A0A2-B9E5-4433-87C0-68B6B72699C7, LBA 239616-83886079",
        "PASS entry 4, unused on the disk, takes the new partition: \"Stela macOS\", type \
         09137F55-F9C2-4DE6-B5DE-5A68DF821BDC, LBA 83886080-230686719",
        "PASS the primary and backup GPT headers change only their header and entries \
         checksums, bytes 16-19 and 88-91",
        "PASS the protective MBR at LBA 0 is valid, and stays byte for byte",
        "PASS every write lies in the GPT's own blocks or in the macOS partition, and none is a \
         TRIM",
    ] {
        assert!(lines.iter().any(|said| said == line), "{line}\n{lines:#?}");
    }
    // Every write is said before anything is checked, and the checks in
    // order.
    assert!(lines[..12].iter().all(|line| line.starts_with("PLAN ")));
    assert!(lines[12..].iter().all(|line| line.starts_with("PASS ")));
    assert_eq!(lines.len(), 12 + 5 + 5 + 3 + 1);
    // Applying a plan that only checks writes nothing either.
    assert_eq!(
        made.apply(&mut disk),
        Err(ApplyError {
            step: Step::Check,
            lba: None,
            cause: Cause::CheckOnly,
        })
    );
    assert_eq!(disk.done, []);
    // Nor does checking a plan that installs.
    let install = planned(&disk, &table, free(512), Mode::Install);
    assert!(checked(&install, &mut disk).0.is_ok());
    assert_eq!(disk.done, []);
}

#[test]
fn applying_writes_the_trial_then_the_backup_then_the_primary_each_read_back() {
    for block in [512, 4096] {
        let table = layout(block);
        let mut disk = Sparse::with(block, &table);
        let before = disk.ends();
        let made = planned(&disk, &table, free(block), Mode::Install);
        let mut lines = Vec::new();
        made.apply_saying(&mut disk, &mut |line| lines.push(String::from(line)))
            .unwrap();
        let (last, entry_blocks) = (BYTES / block - 1, 16384 / block);
        let first = made.partition.first_lba;
        let mib = MIB / block;
        let mut expected = vec![
            Done::Write {
                lba: first,
                blocks: entry_blocks,
            },
            Done::Write {
                lba: last - entry_blocks,
                blocks: entry_blocks,
            },
            Done::Write {
                lba: last,
                blocks: 1,
            },
            Done::Write {
                lba: 2,
                blocks: entry_blocks,
            },
            Done::Write { lba: 1, blocks: 1 },
            Done::Zeroes {
                lba: first,
                blocks: mib,
            },
        ];
        let tail = made.zero[1].lba;
        expected.extend((0..16).map(|index| Done::Zeroes {
            lba: tail + index * mib,
            blocks: mib,
        }));
        assert_eq!(disk.changes(), expected, "{block}");
        // Each write was made durable before it was read back and the next
        // was made.
        assert_eq!(
            disk.done
                .iter()
                .filter(|done| **done == Done::Flush)
                .count(),
            5 + 1
        );
        // The MBR as it was, every entry but the new one byte for byte, and
        // the headers but for their checksums.
        let after = disk.ends();
        let b = block as usize;
        assert_eq!(after.head[..b], before.head[..b]);
        let entries = |ends: &Ends| ends.head[2 * b..2 * b + 16384].to_vec();
        let (was, now) = (entries(&before), entries(&after));
        for slot in 0..128 {
            let span = slot * 128..(slot + 1) * 128;
            assert_eq!(was[span.clone()] == now[span], slot != 3, "{slot}");
        }
        assert_eq!(after.tail[..16384], now[..]);
        for (was, now) in [
            (&before.head[b..2 * b], &after.head[b..2 * b]),
            (&before.tail[16384..], &after.tail[16384..]),
        ] {
            let changed: Vec<usize> = (0..b).filter(|&at| was[at] != now[at]).collect();
            assert!(
                changed
                    .iter()
                    .all(|at| CHECKSUMS.iter().any(|c| c.contains(at)))
            );
        }
        assert_eq!(gpt::read(&mut disk).unwrap(), written(&made));
        // The trial write zeroed with the first MiB, and the namespace
        // between the two ranges untouched.
        assert!(disk.bytes(first, mib).iter().all(|&byte| byte == 0));
        assert!(disk.bytes(first + mib, 4).iter().all(|&byte| byte == 0xa5));
        assert!(disk.bytes(tail - 4, 4).iter().all(|&byte| byte == 0xa5));
        for line in [
            "PASS wrote the backup GPT entries",
            "PASS wrote the backup GPT header",
            "PASS the backup GPT on the disk is the new table, whole, and the primary is as it was",
            "PASS wrote the primary GPT entries",
            "PASS wrote the primary GPT header",
            "PASS both GPT copies read back whole, alike and as planned",
        ] {
            assert!(lines.iter().any(|said| said.starts_with(line)), "{line}");
        }
    }
}

#[test]
fn a_write_that_does_not_read_back_stops_before_the_next() {
    let table = layout(512);
    let made = planned(&Sparse::with(512, &table), &table, free(512), Mode::Install);
    let new = written(&made);
    let last = BYTES / 512 - 1;
    let first = made.partition.first_lba;
    for (step, lost, reads_new) in [
        (Step::Probe, first, false),
        (Step::Table, last - 32, false),
        (Step::Table, last, false),
        (Step::Table, 2, false),
        (Step::Table, 1, true),
    ] {
        let mut disk = Sparse::with(512, &table);
        disk.lost = lost..lost + 1;
        let primary = disk.bytes(0, 34);
        assert_eq!(
            made.apply(&mut disk),
            Err(ApplyError {
                step,
                lba: Some(lost),
                cause: Cause::ReadBack,
            }),
            "{lost}"
        );
        // The write that did not read back was the last one made.
        let writes: Vec<Done> = disk
            .changes()
            .into_iter()
            .filter(|done| matches!(done, Done::Write { .. }))
            .collect();
        assert!(matches!(writes.last(), Some(Done::Write { lba, .. }) if *lba == lost));
        // Until the primary's entries are written the primary copy is as it
        // was; there is a whole table on the disk either way, every Windows
        // partition in it.
        if lost != 1 && lost != 2 {
            assert_eq!(disk.bytes(0, 34), primary);
        }
        let found = gpt::read(&mut disk).unwrap();
        assert_eq!(
            found,
            if reads_new {
                new.clone()
            } else {
                table.clone()
            }
        );
    }
}

#[test]
fn a_write_that_lands_in_another_copy_too_is_found() {
    let table = layout(512);
    let made = planned(&Sparse::with(512, &table), &table, free(512), Mode::Install);
    let last = BYTES / 512 - 1;
    // The backup header's write damages the backup's entries: the backup
    // is read whole before the primary is touched.
    let mut disk = Sparse::with(512, &table);
    disk.scribble = Some((last, last - 30));
    let primary = disk.bytes(0, 34);
    assert_eq!(
        made.apply(&mut disk),
        Err(ApplyError {
            step: Step::Table,
            lba: Some(last - 32),
            cause: Cause::ReadBack,
        })
    );
    assert_eq!(disk.bytes(0, 34), primary);
    assert_eq!(gpt::read(&mut disk).unwrap(), table);
    // The primary header's write damages the backup's entries: both copies
    // are read back at the end.
    let mut disk = Sparse::with(512, &table);
    disk.scribble = Some((1, last - 30));
    assert_eq!(
        made.apply(&mut disk),
        Err(ApplyError {
            step: Step::Verify,
            lba: Some(last - 32),
            cause: Cause::Copies("a GPT entry array is damaged"),
        })
    );
    assert_eq!(gpt::read(&mut disk).unwrap(), written(&made));
}

#[test]
fn a_backup_write_that_changes_the_primary_stops_before_the_primary_is_written() {
    let table = layout(512);
    let made = planned(&Sparse::with(512, &table), &table, free(512), Mode::Install);
    let last = BYTES / 512 - 1;
    // The backup entries' write lands in the MBR's boot code too, the
    // backup header's in the primary header or in an unused primary
    // entry: the backup reads back whole and the new table, and the
    // primary is not what the picker read.
    for (at, victim) in [(last - 32, 0), (last, 1), (last, 20)] {
        let mut disk = Sparse::with(512, &table);
        disk.scribble = Some((at, victim));
        assert_eq!(
            made.apply(&mut disk),
            Err(ApplyError {
                step: Step::Table,
                lba: Some(victim),
                cause: Cause::Changed,
            }),
            "{victim}"
        );
        // Nothing was written to the primary's blocks, and nothing zeroed.
        assert!(
            disk.changes()
                .iter()
                .all(|done| matches!(done, Done::Write { lba, .. } if *lba >= 34)),
            "{victim}"
        );
    }
}

#[test]
fn a_disk_unlike_what_the_picker_read_is_not_written() {
    let table = layout(512);
    let made = planned(&Sparse::with(512, &table), &table, free(512), Mode::Install);
    let last = BYTES / 512 - 1;
    // The MBR's boot code, a header's reserved bytes past its checksum, an
    // unused entry of each array and the backup header.
    for (lba, at) in [
        (0, 300),
        (1, 200),
        (2 + 100 * 128 / 512, 100 * 128 % 512),
        (last - 32 + 100 * 128 / 512, 100 * 128 % 512),
        (last, 200),
    ] {
        let mut disk = Sparse::with(512, &table);
        let mut block = disk.block_at(lba);
        block[at] ^= 1;
        disk.put(lba, &block);
        let changed = Err(ApplyError {
            step: Step::Identify,
            lba: Some(lba),
            cause: Cause::Changed,
        });
        assert_eq!(made.apply(&mut disk), changed, "{lba}");
        assert_eq!(checked(&made, &mut disk).0.map(drop), changed.map(drop));
        assert_eq!(disk.done, []);
    }
}

#[test]
fn a_disk_whose_gpt_copies_are_not_whole_and_alike_is_not_written() {
    let table = layout(512);
    let last = BYTES / 512 - 1;
    let mut other = table.clone();
    other.remove(4);
    let backup = shared::encode(gpt::geometry(&Sparse::new(512).identity), &other).unwrap();
    /// Damage done to the disk, given another table's encoded GPT.
    type Damage = fn(&mut Sparse, &[Write]);
    type Unplanned = Option<(u64, shared::Unkept)>;
    // Each with the block at which the picker's plan is refused, when it is:
    // a plan's headers are the disk's own, so one missing or damaged makes
    // no plan.
    let cases: [(Damage, u64, &str, Unplanned); 4] = [
        (
            |disk, _| disk.put(disk.last(), &[0; 512]),
            last,
            "a GPT header is missing",
            Some((last, shared::Unkept::Missing)),
        ),
        (
            |disk, _| {
                let mut header = disk.block_at(1);
                header[20] ^= 1;
                disk.put(1, &header);
            },
            1,
            "a GPT header is damaged",
            Some((1, shared::Unkept::Damaged("GPT header CRC mismatch"))),
        ),
        (
            |disk, _| {
                let mut entries = disk.block_at(20);
                entries[0] ^= 1;
                disk.put(20, &entries);
            },
            2,
            "a GPT entry array is damaged",
            None,
        ),
        // A backup that is whole, of another table.
        (
            |disk, backup| {
                disk.put(backup[0].lba, &backup[0].bytes);
                disk.put(backup[1].lba, &backup[1].bytes);
            },
            last - 32,
            "the backup GPT is not the primary's copy",
            None,
        ),
    ];
    let whole = planned(&Sparse::with(512, &table), &table, free(512), Mode::Check);
    for (damage, lba, why, unplanned) in cases {
        let mut disk = Sparse::with(512, &table);
        damage(&mut disk, &backup);
        let ends = disk.ends();
        let picked = plan(
            &Decision {
                mode: Mode::Check,
                ..decision(&disk, &ends, Some(&table), free(512))
            },
            &mut || guid(NEW),
        );
        assert_eq!(
            picked.as_ref().err(),
            unplanned
                .map(|(lba, why)| PlanError::Layout { lba, why })
                .as_ref(),
            "{why}"
        );
        // The picker read the disk as it is now; a plan it refused to make
        // is refused by the check too.
        let made = picked.unwrap_or_else(|_| as_read(&whole, &disk));
        assert_eq!(
            checked(&made, &mut disk).0,
            Err(ApplyError {
                step: Step::Check,
                lba: Some(lba),
                cause: Cause::Copies(why),
            })
        );
        let made = Plan {
            mode: Mode::Install,
            ..made
        };
        assert!(made.apply(&mut disk).is_err());
        assert_eq!(disk.done, []);
    }
}

#[test]
fn a_plan_that_writes_what_no_plan_writes_writes_nothing() {
    let table = layout(512);
    let made = planned(&Sparse::with(512, &table), &table, free(512), Mode::Install);
    let last = BYTES / 512 - 1;
    let gib = GIB / 512;
    let refused = |lba, why| (Some(lba), Cause::Refused(why));
    let cases: Vec<(Plan, (Option<u64>, Cause))> = vec![
        // Without the ESP.
        (
            rewritten(&made, |table, _| {
                table.remove(0);
            }),
            (Some(2), Cause::Entry { entry: 1 }),
        ),
        // Windows' attributes changed.
        (
            rewritten(&made, |table, _| table.partitions[2].attributes = 1),
            (Some(2), Cause::Entry { entry: 3 }),
        ),
        // Over the end of Windows.
        (
            rewritten(&made, |table, partition| {
                partition.first_lba -= gib;
                new_in(table).first_lba -= gib;
                new_in(table).last_lba -= gib;
            }),
            (Some(39 * gib), Cause::Overlap { entry: 3 }),
        ),
        // Too small.
        (
            rewritten(&made, |table, partition| {
                partition.blocks = gib;
                let new = new_in(table);
                new.last_lba = new.first_lba + gib - 1;
            }),
            (Some(40 * gib), Cause::TooSmall { bytes: GIB }),
        ),
        // Two partitions added.
        (
            rewritten(&made, |table, _| {
                let second = Partition {
                    unique_guid: guid("CCCCCCCC-0000-4000-8000-000000000002"),
                    first_lba: 100 * gib,
                    last_lba: 101 * gib - 1,
                    ..new_in(table).clone()
                };
                table.add(second);
            }),
            refused(3, "the plan adds more than one partition"),
        ),
        // Another disk GUID.
        (
            rewritten(&made, |table, _| table.disk_guid = guid(NEW)),
            refused(
                1,
                "a GPT header the plan writes changes more than its two checksums",
            ),
        ),
        // Its partition another than the one its entry holds.
        (
            Plan {
                partition: Planned {
                    guid: guid("CCCCCCCC-0000-4000-8000-000000000009"),
                    ..made.partition
                },
                ..made.clone()
            },
            refused(
                2,
                "the entry the plan adds is not the planned Stambha partition",
            ),
        ),
        // The primary entries elsewhere.
        (
            {
                let mut moved = made.clone();
                moved.table[2].lba = 40;
                moved
            },
            refused(
                40,
                "a table write is not in the GPT's own blocks, in the order that keeps a whole \
                 copy",
            ),
        ),
        // A write more, and the MBR though the disk's is valid.
        (
            {
                let mut more = made.clone();
                more.table.push(Write {
                    lba: 100,
                    bytes: vec![0; 512],
                });
                more
            },
            refused(
                100,
                "the table's writes are not the backup GPT, the primary and, for a disk without \
                 one, a protective MBR",
            ),
        ),
        (
            {
                let mut mbr = made.clone();
                mbr.table.push(
                    shared::encode(made.geometry, &written(&made))
                        .unwrap()
                        .remove(4),
                );
                mbr
            },
            refused(
                0,
                "the table's writes are not the backup GPT, the primary and, for a disk without \
                 one, a protective MBR",
            ),
        ),
        // The backup's entries not the primary's.
        (
            {
                let mut unlike = made.clone();
                unlike.table[0].bytes[600] ^= 1;
                unlike
            },
            refused(last - 32, "the plan's backup entries are not its primary's"),
        ),
        // Zeroes in Windows.
        (
            Plan {
                zero: vec![Extent {
                    lba: gib,
                    blocks: 8,
                }],
                ..made.clone()
            },
            refused(gib, "a range to zero lies outside the macOS partition"),
        ),
        // A Stela partition to reuse that the disk does not hold.
        (
            Plan {
                table: Vec::new(),
                ..made.clone()
            },
            refused(
                40 * gib,
                "the partition to reuse is not that Stambha partition on the disk",
            ),
        ),
        // Windows reused: smaller than macOS, and no Stela partition.
        (
            Plan {
                table: Vec::new(),
                partition: Planned {
                    guid: guid(C),
                    first_lba: table.partitions[2].first_lba,
                    blocks: table.partitions[2].blocks(),
                },
                zero: vec![Extent {
                    lba: table.partitions[2].first_lba,
                    blocks: 8,
                }],
                ..made.clone()
            },
            (
                Some(table.partitions[2].first_lba),
                Cause::TooSmall {
                    bytes: table.partitions[2].blocks() * 512,
                },
            ),
        ),
    ];
    for (tampered, (lba, cause)) in cases {
        let mut disk = Sparse::with(512, &table);
        assert_eq!(
            tampered.apply(&mut disk),
            Err(ApplyError {
                step: Step::Check,
                lba,
                cause: cause.clone(),
            }),
            "{cause:?}"
        );
        assert_eq!(disk.done, [], "{cause:?}");
    }
}

#[test]
fn blocks_of_no_length_or_past_the_last_lba_are_said_without_overflow() {
    let span = |lba, blocks| {
        Span {
            lba,
            blocks,
            block: 512,
        }
        .to_string()
    };
    assert_eq!(span(0, 0), "LBA 0 (0 blocks, 0 bytes)");
    assert_eq!(span(7, 1), "LBA 7 (1 block, 512 bytes)");
    assert_eq!(
        span(u64::MAX, 2),
        "LBA 18446744073709551615-18446744073709551616 (2 blocks, 1024 bytes)"
    );
    assert_eq!(
        span(1, u64::MAX),
        "LBA 1-18446744073709551615 (18446744073709551615 blocks, 9444732965739290426880 bytes)"
    );
    // A damaged plan's lines are said before it is checked; it is said
    // whole, refused, and nothing is written.
    let table = layout(512);
    let mut disk = Sparse::with(512, &table);
    let made = planned(&disk, &table, free(512), Mode::Check);
    let outside = "a range to zero lies outside the macOS partition";
    for zero in [(0, 0), (u64::MAX, u64::MAX), (1, u64::MAX)] {
        let damaged = Plan {
            zero: vec![Extent {
                lba: zero.0,
                blocks: zero.1,
            }],
            ..made.clone()
        };
        let (result, lines) = checked(&damaged, &mut disk);
        assert_eq!(
            result,
            Err(error(Step::Check, Some(zero.0), Cause::Refused(outside))),
            "{zero:?}"
        );
        assert!(lines.iter().any(|line| line.starts_with("PLAN zero 1:")));
    }
    // A partition at the last LBAs, whose trial write's block numbers pass
    // the last.
    let damaged = Plan {
        partition: Planned {
            first_lba: u64::MAX - 1,
            ..made.partition
        },
        ..made.clone()
    };
    assert_eq!(
        checked(&damaged, &mut disk).0.unwrap_err().step,
        Step::Check
    );
    // A disk of no blocks of no bytes, which no GPT here uses.
    let damaged = Plan {
        geometry: Disk {
            block_bytes: 0,
            blocks: 0,
        },
        ..made.clone()
    };
    let (result, lines) = checked(&damaged, &mut disk);
    assert_eq!(result.unwrap_err().step, Step::Identify);
    assert!(
        lines
            .iter()
            .all(|line| !line.starts_with("PLAN write") && !line.starts_with("PLAN zero"))
    );
    assert_eq!(disk.done, []);
}

#[test]
fn a_reused_partition_writes_no_table_and_zeroes_only_itself() {
    for block in [512, 4096] {
        let table = with_stela(block);
        let mut disk = Sparse::with(block, &table);
        let made = planned(&disk, &table, Place::Reuse(5), Mode::Install);
        assert!(made.table.is_empty());
        assert_eq!(made.partition.guid, guid(NEW));
        let before = disk.ends();
        made.apply(&mut disk).unwrap();
        let (first, end) = (
            made.partition.first_lba,
            made.partition.first_lba + made.partition.blocks,
        );
        for change in disk.changes() {
            let Done::Zeroes { lba, blocks } = change else {
                panic!("{change:?}");
            };
            assert!(lba >= first && lba + blocks <= end);
        }
        assert_eq!(disk.ends(), before);
        // A check writes nothing.
        let mut disk = Sparse::with(block, &table);
        let made = planned(&disk, &table, Place::Reuse(5), Mode::Check);
        let (result, lines) = checked(&made, &mut disk);
        assert_eq!(result.unwrap().table, None);
        assert!(lines.iter().any(|line| {
            line.starts_with("PASS the partition reused, entry 4, is the Stela partition at")
        }));
        assert_eq!(disk.done, []);
    }
}

#[test]
fn a_failed_write_names_its_step_and_block_and_leaves_one_whole_table() {
    let table = layout(512);
    let made = planned(&Sparse::with(512, &table), &table, free(512), Mode::Install);
    let new = written(&made);
    let first = made.partition.first_lba;
    let mut disk = Sparse::with(512, &table);
    disk.fails_at = Some(first + 3);
    assert_eq!(
        made.apply(&mut disk),
        Err(ApplyError {
            step: Step::Probe,
            lba: Some(first),
            cause: Cause::Device(BlockError::Timeout),
        })
    );
    assert_eq!(gpt::read(&mut disk).unwrap(), table);
    // Each of the table's writes failing in turn.
    for (index, write) in made.table.iter().enumerate() {
        let mut disk = Sparse::with(512, &table);
        disk.fails_at = Some(write.lba);
        assert_eq!(
            made.apply(&mut disk),
            Err(ApplyError {
                step: Step::Table,
                lba: Some(write.lba),
                cause: Cause::Device(BlockError::Timeout),
            })
        );
        // The earlier table until the primary's entries are written, the
        // new one after.
        let found = gpt::read(&mut disk).unwrap();
        assert_eq!(
            found,
            if index < 3 {
                table.clone()
            } else {
                new.clone()
            }
        );
        // Nothing was zeroed.
        assert!(
            !disk
                .changes()
                .iter()
                .any(|done| matches!(done, Done::Zeroes { .. }))
        );
    }
    // A zeroing failing, in the machine state's third MiB.
    let at = made.zero[1].lba + 2 * MIB / 512;
    let mut disk = Sparse::with(512, &table);
    disk.fails_at = Some(at + 5);
    let error = made.apply(&mut disk).unwrap_err();
    assert_eq!(
        error,
        ApplyError {
            step: Step::Zero,
            lba: Some(at),
            cause: Cause::Device(BlockError::Timeout),
        }
    );
    assert_eq!(
        error.to_string(),
        format!(
            "zeroing the partition's first MiB and machine state at block {at}: {}",
            BlockError::Timeout
        )
    );
    assert_eq!(gpt::read(&mut disk).unwrap(), new);
}

#[test]
fn a_disk_without_a_protective_mbr_gets_one() {
    let table = layout(512);
    let mut disk = Sparse::with(512, &table);
    disk.put(0, &[0; 512]);
    let made = planned(&disk, &table, free(512), Mode::Install);
    assert_eq!(made.table.last().map(|write| write.lba), Some(0));
    let mut lines = Vec::new();
    made.apply_saying(&mut disk, &mut |line| lines.push(String::from(line)))
        .unwrap();
    assert!(shared::protective(&disk.block_at(0)));
    assert!(lines.iter().any(
        |line| line == "PASS the disk has no protective MBR, and the plan writes one at LBA 0"
    ));
}

#[test]
fn another_disk_under_the_serial_is_not_written() {
    let table = layout(512);
    let made = planned(&Sparse::with(512, &table), &table, free(512), Mode::Install);
    // Another size, and the same disk seen with 4096-byte blocks.
    for (blocks, block) in [(2 * BYTES / 512, 512), (BYTES / 4096, 4096)] {
        let mut other = Sparse::with(block, &layout(block));
        other.identity.blocks = blocks;
        let error = made.apply(&mut other).unwrap_err();
        assert_eq!(
            error,
            ApplyError {
                step: Step::Identify,
                lba: None,
                cause: Cause::Geometry {
                    found: Disk {
                        block_bytes: block,
                        blocks
                    }
                },
            }
        );
        assert_eq!(other.done, []);
    }
    // The same size with another table.
    let mut other = Sparse::with(
        512,
        &Table {
            disk_guid: guid(NEW),
            ..table.clone()
        },
    );
    let error = made.apply(&mut other).unwrap_err();
    assert_eq!(error.cause, Cause::Changed);
    assert_eq!(
        error.to_string(),
        "checking that it is the disk chosen at block 1: the disk's partition table blocks are \
         not what the picker read through the firmware"
    );
    assert_eq!(other.done, []);
}

#[test]
fn a_decision_that_cannot_be_carried_out_makes_no_plan() {
    let table = layout(512);
    let disk = Sparse::with(512, &table);
    let ends = disk.ends();
    let refused = |decision: Decision<'_>| plan(&decision, &mut || guid(NEW)).unwrap_err();
    let at = |first_mib: u64, mib: u64| Place::Free {
        first_lba: first_mib * MIB / 512,
        bytes: mib * MIB,
    };
    // A disk without a table is not given one.
    assert_eq!(
        refused(decision(&disk, &ends, None, free(512))),
        PlanError::NoTable
    );
    let short = Ends {
        head: ends.head[..512].to_vec(),
        tail: ends.tail.clone(),
    };
    assert_eq!(
        refused(decision(&disk, &short, Some(&table), free(512))),
        PlanError::Ends
    );
    // Only a Stela partition is reused.
    for index in [0, 2, 9] {
        assert_eq!(
            refused(decision(&disk, &ends, Some(&table), Place::Reuse(index))),
            PlanError::NotReusable
        );
    }
    // Over Windows, past the usable blocks, not whole blocks.
    for place in [
        at(39 * 1024, FREE.1),
        Place::Free {
            first_lba: FREE.0 * MIB / 512,
            bytes: FREE.1 * MIB + 100,
        },
    ] {
        assert_eq!(
            refused(decision(&disk, &ends, Some(&table), place)),
            PlanError::Overlap
        );
    }
    assert_eq!(
        refused(decision(
            &disk,
            &ends,
            Some(&table),
            at(112 * 1024, 17 * 1024)
        )),
        PlanError::Table("partition outside the usable range")
    );
    // Too small for a namespace and the machine state.
    assert_eq!(
        refused(decision(&disk, &ends, Some(&table), at(FREE.0, 16))),
        PlanError::TooSmall
    );
    assert_eq!(
        refused(Decision {
            serial: "",
            ..decision(&disk, &ends, Some(&table), free(512))
        }),
        PlanError::NoSerial
    );
    assert_eq!(
        refused(Decision {
            geometry: Disk {
                block_bytes: 520,
                blocks: 1 << 20
            },
            ..decision(&disk, &ends, Some(&table), free(512))
        }),
        PlanError::Blocks(520)
    );
}

#[test]
fn a_plan_survives_its_bytes() {
    for (table, place, mode) in [
        (layout(4096), free(4096), Mode::Install),
        (with_stela(4096), Place::Reuse(5), Mode::Check),
    ] {
        let made = planned(&Sparse::with(4096, &table), &table, place, mode);
        let bytes = made.encode();
        assert_eq!(Plan::decode(&bytes), Some(made));
        // Cut short, grown, under another mark or of another mode it is no
        // plan.
        assert_eq!(Plan::decode(&bytes[..bytes.len() - 1]), None);
        assert_eq!(Plan::decode(&[&bytes[..], &[0]].concat()), None);
        assert_eq!(Plan::decode(&bytes[1..]), None);
        let mut other = bytes.clone();
        other[8] = 2;
        assert_eq!(Plan::decode(&other), None);
    }
    assert_eq!(Plan::decode(&[]), None);
}

#[test]
fn the_disk_is_found_by_its_serial_or_by_what_the_picker_read() {
    let table = layout(512);
    let made = planned(&Sparse::with(512, &table), &table, free(512), Mode::Install);
    let renamed = |serial: &str, table: &Table| -> Box<dyn BlockDevice> {
        let mut disk = Sparse::with(512, table);
        disk.identity.serial = serial.into();
        Box::new(disk)
    };
    let other = Table {
        disk_guid: guid(NEW),
        ..table.clone()
    };
    let mut devices = vec![renamed("FAKE-OTHER-0002", &other), renamed(SERIAL, &table)];
    assert_eq!(made.locate(&mut devices), Ok(1));
    // Two disks under the serial.
    devices.push(renamed(SERIAL, &other));
    assert_eq!(made.locate(&mut devices), Err(LocateError::Ambiguous(2)));
    // No disk under the serial: the one with the same size and first
    // blocks.
    let mut devices = vec![
        renamed("FAKE-OTHER-0002", &other),
        renamed("FAKE-BRIDGE-0003", &table),
        Box::new(RamDisk::new("FAKE-OTHER-0004", 16, 512)),
    ];
    assert_eq!(made.locate(&mut devices), Ok(1));
    devices.push(renamed("FAKE-BRIDGE-0005", &table));
    assert_eq!(made.locate(&mut devices), Err(LocateError::Ambiguous(2)));
    devices.truncate(1);
    assert_eq!(made.locate(&mut devices), Err(LocateError::NoDisk));
    assert_eq!(made.locate(&mut []), Err(LocateError::NoDisk));
}

#[test]
fn an_empty_libfdisk_gpt_takes_macos() {
    // The tester's NVMe disk, wiped in Linux: no partitions, the usable
    // blocks from 1 MiB.
    let table = table_of(Vec::new());
    let disk = Sparse::laid_out(512, EVO, &table, fdisk(512));
    assert_eq!(usable_of(&disk), (2048, EVO - 34));
    assert_eq!(
        free_after(&disk, &table),
        Place::Free {
            first_lba: 2048,
            bytes: 976_771_072 * 512
        }
    );
    let made = installs_beside(disk, "fdisk, empty");
    let lbas: Vec<u64> = made.table.iter().map(|write| write.lba).collect();
    assert_eq!(lbas, [EVO - 33, EVO - 1, 2, 1]);
    assert_eq!(
        (made.partition.first_lba, made.partition.blocks),
        (2048, 976_771_072)
    );
}

#[test]
fn a_libfdisk_disk_with_linux_and_an_esp_takes_macos() {
    let table = pny();
    let disk = Sparse::laid_out(512, PNY, &table, fdisk(512));
    assert_eq!(
        free_after(&disk, &table),
        Place::Free {
            first_lba: 107_620_352,
            bytes: 380_776_448 * 512
        }
    );
    let made = installs_beside(disk, "fdisk, Linux and an ESP");
    let new = written(&made);
    assert_eq!(new.slots, [0, 1, 2]);
    assert_eq!(
        (new.partitions[2].first_lba, new.partitions[2].last_lba),
        (107_620_352, 488_396_799)
    );
}

#[test]
fn a_disk_each_tool_partitioned_takes_macos() {
    let msr = |last| blocks_partition(RESERVED, 3, 34, last, "Microsoft reserved partition");
    type Edit = Box<dyn Fn(u64, &mut [u8])>;
    let cases: Vec<(&str, u64, u64, Table, Edit)> = vec![
        // Windows' Initialize Disk: a 16 MiB reserved partition.
        (
            "Windows, initialized",
            512,
            PNY,
            table_of(vec![msr(32_767)]),
            Box::new(standard),
        ),
        (
            "Windows, initialized",
            512,
            EVO,
            table_of(vec![msr(32_767)]),
            Box::new(standard),
        ),
        (
            "Windows, initialized, 4096-byte blocks",
            4096,
            BYTES / 4096,
            table_of(vec![blocks_partition(
                RESERVED,
                3,
                6,
                4095,
                "Microsoft reserved partition",
            )]),
            Box::new(standard),
        ),
        (
            "Windows, installed and shrunk",
            512,
            PNY,
            layout(512),
            Box::new(standard),
        ),
        (
            "Windows, installed and shrunk, 4096-byte blocks",
            4096,
            PNY / 8,
            layout(4096),
            Box::new(standard),
        ),
        (
            "gdisk, empty",
            512,
            PNY,
            table_of(Vec::new()),
            Box::new(standard),
        ),
        (
            "gdisk, a Linux filesystem",
            512,
            PNY,
            table_of(vec![blocks_partition(
                LINUX,
                4,
                2048,
                104_859_647,
                "Linux filesystem",
            )]),
            Box::new(standard),
        ),
        (
            "parted, empty",
            512,
            EVO,
            table_of(Vec::new()),
            Box::new(standard),
        ),
        (
            "parted, an ESP",
            512,
            EVO,
            table_of(vec![blocks_partition(ESP, 5, 2048, 1_050_623, "ESP")]),
            Box::new(standard),
        ),
        // diskutil: a 200 MiB ESP from LBA 40, the container after it, on
        // no MiB boundary.
        (
            "macOS, an ESP and an APFS container",
            512,
            EVO,
            table_of(vec![
                blocks_partition(ESP, 6, 40, 409_639, "EFI System Partition"),
                blocks_partition(APFS, 7, 409_640, 409_640 + 200 * GIB / 512 - 1, ""),
            ]),
            Box::new(standard),
        ),
        (
            "fdisk, empty",
            512,
            PNY,
            table_of(Vec::new()),
            Box::new(fdisk(512)),
        ),
        (
            "fdisk, empty, 4096-byte blocks",
            4096,
            BYTES / 4096,
            table_of(Vec::new()),
            Box::new(fdisk(4096)),
        ),
        (
            "fdisk, a Linux filesystem",
            512,
            EVO,
            table_of(vec![blocks_partition(
                LINUX,
                8,
                2048,
                2048 + 50 * GIB / 512 - 1,
                "",
            )]),
            Box::new(fdisk(512)),
        ),
        // Usable blocks from any block past encode's, and to any before.
        (
            "usable from LBA 40",
            512,
            PNY,
            table_of(Vec::new()),
            Box::new(|_: u64, header: &mut [u8]| relabel(header, 40, &40u64.to_le_bytes())),
        ),
    ];
    for (what, block, blocks, table, edit) in cases {
        let disk = Sparse::laid_out(block, blocks, &table, edit);
        let (first, last) = usable_of(&disk);
        assert!(first >= 2 + 16384 / block && last == blocks - 2 - 16384 / block);
        installs_beside(disk, what);
    }
}

#[test]
fn layouts_a_rewrite_does_not_keep_are_refused_naming_the_field() {
    let table = pny();
    let last = PNY - 1;
    let mut clean = Sparse::laid_out(512, PNY, &table, fdisk(512));
    let read = gpt::read(&mut clean).unwrap();
    let place = free_after(&clean, &read);
    let whole = planned(&clean, &read, place, Mode::Install);
    let exactly = |field, found, kept| shared::Unkept::Field {
        field,
        found,
        kept: shared::Kept::Exactly(kept),
    };
    type Edit = fn(u64, &mut [u8]);
    let cases: [(Edit, u64, shared::Unkept); 10] = [
        (
            |lba, h| {
                if lba == 1 {
                    relabel(h, 80, &64u32.to_le_bytes())
                }
            },
            1,
            exactly("entry count", 64, 128),
        ),
        (
            |lba, h| {
                if lba == 1 {
                    relabel(h, 80, &256u32.to_le_bytes())
                }
            },
            1,
            exactly("entry count", 256, 128),
        ),
        (
            |lba, h| {
                if lba == 1 {
                    relabel(h, 84, &256u32.to_le_bytes())
                }
            },
            1,
            exactly("entry size", 256, 128),
        ),
        (
            |lba, h| {
                if lba == 1 {
                    relabel(h, 72, &3u64.to_le_bytes())
                }
            },
            1,
            exactly("entry array LBA", 3, 2),
        ),
        // Cloned from a smaller disk.
        (
            |lba, h| {
                if lba == 1 {
                    relabel(h, 32, &488_000_000u64.to_le_bytes())
                }
            },
            1,
            exactly("other copy's LBA", 488_000_000, PNY - 1),
        ),
        (
            |_, h| relabel(h, 40, &33u64.to_le_bytes()),
            1,
            shared::Unkept::Field {
                field: "first usable LBA",
                found: 33,
                kept: shared::Kept::AtLeast(34),
            },
        ),
        // Over the backup entries.
        (
            |_, h| relabel(h, 48, &488_397_135u64.to_le_bytes()),
            1,
            shared::Unkept::Field {
                field: "last usable LBA",
                found: 488_397_135,
                kept: shared::Kept::AtMost(488_397_134),
            },
        ),
        (
            |lba, h| {
                if lba == 1 {
                    relabel(h, 12, &96u32.to_le_bytes())
                }
            },
            1,
            exactly("header size", 96, 92),
        ),
        (
            |lba, h| {
                if lba != 1 {
                    relabel(h, 72, &(lba - 33).to_le_bytes())
                }
            },
            last,
            exactly("entry array LBA", last - 33, last - 32),
        ),
        // The copies disagree: the primary from 1 MiB, the backup from 34.
        (
            |lba, h| {
                if lba != 1 {
                    relabel(h, 40, &34u64.to_le_bytes())
                }
            },
            last,
            shared::Unkept::Disagree {
                field: "first usable LBA",
                primary: 2048,
                backup: 34,
            },
        ),
    ];
    for (index, (edit, lba, why)) in cases.into_iter().enumerate() {
        let mut disk = Sparse::laid_out(512, PNY, &table, |lba, header: &mut [u8]| {
            fdisk(512)(lba, header);
            edit(lba, header);
        });
        let ends = disk.ends();
        assert_eq!(
            plan(&decision(&disk, &ends, Some(&read), place), &mut || guid(
                NEW
            )),
            Err(PlanError::Layout { lba, why }),
            "{index}"
        );
        // A plan made anyway is refused before anything is written, a
        // check and an install alike.
        let refused = Err(ApplyError {
            step: Step::Check,
            lba: Some(lba),
            cause: Cause::Layout(why),
        });
        let made = as_read(&whole, &disk);
        assert_eq!(made.apply(&mut disk), refused, "{index}");
        let check = Plan {
            mode: Mode::Check,
            ..made
        };
        assert_eq!(checked(&check, &mut disk).0.map(drop), refused.map(drop));
        assert_eq!(disk.done, [], "{index}");
    }
    // The stop names the field and both values.
    let mut disk = Sparse::laid_out(512, PNY, &table, |_, header: &mut [u8]| {
        relabel(header, 40, &33u64.to_le_bytes())
    });
    assert_eq!(
        as_read(&whole, &disk)
            .apply(&mut disk)
            .unwrap_err()
            .to_string(),
        "checking the plan against the disk at block 1: a GPT header names another layout than \
         the one a rewrite keeps: its first usable LBA is 33, where a rewrite keeps 34 or more"
    );
    let ends = disk.ends();
    assert_eq!(
        plan(&decision(&disk, &ends, Some(&read), place), &mut || guid(
            NEW
        ))
        .unwrap_err()
        .to_string(),
        "the disk's partition table at block 1 is not added to: a GPT header names another \
         layout than the one a rewrite keeps: its first usable LBA is 33, where a rewrite keeps \
         34 or more"
    );
}

#[test]
fn a_disk_whose_partitions_overlap_is_not_written() {
    let table = table_of(vec![
        blocks_partition(LINUX, 1, 2048, 103_426_047, ""),
        blocks_partition(ESP, 2, 103_426_000, 107_620_351, ""),
    ]);
    let mut disk = Sparse::laid_out(512, PNY, &table, fdisk(512));
    let read = gpt::read(&mut disk).unwrap();
    let made = planned(&disk, &read, free_after(&disk, &read), Mode::Install);
    assert_eq!(
        made.apply(&mut disk),
        Err(ApplyError {
            step: Step::Check,
            lba: Some(2),
            cause: Cause::Copies("two of the disk's partitions overlap"),
        })
    );
    assert_eq!(disk.done, []);
}

#[test]
fn a_partition_past_a_narrowed_last_usable_is_not_written() {
    let table = pny();
    let narrowed = PNY - 34 - 2048;
    let edit = |lba: u64, header: &mut [u8]| {
        fdisk(512)(lba, header);
        relabel(header, 48, &narrowed.to_le_bytes());
    };
    let mut disk = Sparse::laid_out(512, PNY, &table, edit);
    assert_eq!(usable_of(&disk), (2048, narrowed));
    let read = gpt::read(&mut disk).unwrap();
    // To the geometry's last MiB, past the header's last usable block.
    let wide = Place::Free {
        first_lba: 107_620_352,
        bytes: 380_776_448 * 512,
    };
    let ends = disk.ends();
    assert_eq!(
        plan(&decision(&disk, &ends, Some(&read), wide), &mut || guid(
            NEW
        )),
        Err(PlanError::Table("partition outside the usable range"))
    );
    // A plan made anyway, from the disk before its usable blocks were
    // narrowed, is refused by the check.
    let clean = Sparse::laid_out(512, PNY, &table, fdisk(512));
    let made = as_read(&planned(&clean, &read, wide, Mode::Install), &disk);
    assert_eq!(
        made.apply(&mut disk),
        Err(error(
            Step::Check,
            Some(107_620_352),
            Cause::Refused("the macOS partition lies outside the blocks the GPT gives partitions"),
        ))
    );
    assert_eq!(disk.done, []);
    // The free space within them takes macOS.
    let made = installs_beside(
        Sparse::laid_out(512, PNY, &table, edit),
        "a narrowed last usable LBA",
    );
    assert!(made.partition.first_lba + made.partition.blocks - 1 <= narrowed);
}
