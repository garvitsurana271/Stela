use super::*;

const DISK: Disk = Disk {
    block_bytes: 512,
    blocks: 200 * 1024 * 1024 * 1024 / 512,
};

fn counter() -> impl FnMut() -> [u8; 16] {
    let mut n = 0u8;
    move || {
        n += 1;
        [n; 16]
    }
}

fn request() -> Request {
    Request {
        staging_bytes: 3 * MIB + 5,
        machine_state_bytes: MACHINE_STATE_BYTES,
        cache_bytes: DEFAULT_CACHE_BYTES,
        min_apple_bytes: DEFAULT_MIN_APPLE_BYTES,
    }
}

fn read_back(disk: Disk, writes: &[Write]) -> Table {
    let at = |lba: u64| &writes.iter().find(|w| w.lba == lba).unwrap().bytes;
    let info = parse_header(disk, 1, at(1)).unwrap().unwrap();
    parse_entries(&info, at(info.entries_lba)).unwrap()
}

#[test]
fn crc32_matches_the_ieee_check_value() {
    assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
    assert_eq!(crc32(b""), 0);
}

#[test]
fn guids_round_trip_in_mixed_endian_form() {
    let text = "C12A7328-F81F-11D2-BA4B-00A0C93EC93B";
    let guid = Guid::parse(text).unwrap();
    assert_eq!(guid.0[..4], [0x28, 0x73, 0x2a, 0xc1]);
    assert_eq!(alloc::format!("{guid}"), text);
    assert!(Guid::parse("C12A7328F81F11D2BA4B00A0C93EC93B").is_none());
    assert!(Guid::parse("Z12A7328-F81F-11D2-BA4B-00A0C93EC93B").is_none());
    let random = Guid::from_entropy([0xff; 16]);
    assert_eq!(random.0[7] >> 4, 4);
    assert_eq!(random.0[8] >> 6, 2);
}

#[test]
fn a_blank_disk_gets_aligned_stela_partitions_after_a_reserved_esp_gap() {
    let plan = plan(DISK, None, request(), counter()).unwrap();
    assert!(plan.write);
    let roles: Vec<_> = plan.table.partitions.iter().map(|p| p.role()).collect();
    assert_eq!(roles, Role::STELA.map(Some).to_vec());
    let alignment = ALIGNMENT / DISK.block_bytes;
    let first = &plan.table.partitions[0];
    assert!(first.first_lba * DISK.block_bytes >= RESERVED_ESP_BYTES);
    for pair in plan.table.partitions.windows(2) {
        assert!(pair[0].last_lba < pair[1].first_lba);
    }
    for partition in &plan.table.partitions {
        assert_eq!(partition.first_lba % alignment, 0);
        assert_eq!((partition.last_lba + 1) % alignment, 0);
    }
    let staging = plan.partition(Role::Staging).unwrap();
    assert_eq!(staging.blocks() * DISK.block_bytes, 4 * MIB);
    assert!(
        plan.table
            .partitions
            .iter()
            .all(|p| !p.unique_guid.is_zero())
    );
    assert_ne!(plan.table.disk_guid, plan.table.partitions[0].unique_guid);
}

#[test]
fn encoded_tables_parse_back_with_valid_crcs_at_both_block_sizes() {
    for block_bytes in [512, 4096] {
        let disk = Disk {
            block_bytes,
            blocks: DISK.blocks * 512 / block_bytes,
        };
        let plan = plan(disk, None, request(), counter()).unwrap();
        let writes = encode(disk, &plan.table).unwrap();
        assert_eq!(writes.last().unwrap().lba, 0);
        assert_eq!(writes.last().unwrap().bytes[450], 0xee);
        assert_eq!(read_back(disk, &writes), plan.table);
        let backup = &writes[1];
        assert_eq!(backup.lba, disk.blocks - 1);
        let backup = parse_header(disk, disk.blocks - 1, &backup.bytes)
            .unwrap()
            .unwrap();
        assert_eq!(backup.entries_lba, disk.blocks - 1 - 16384 / block_bytes);
    }
}

#[test]
fn an_existing_esp_is_kept_and_foreign_partitions_are_refused() {
    let esp = Partition {
        type_guid: Role::Esp.type_guid(),
        unique_guid: Guid([9; 16]),
        first_lba: 2048,
        last_lba: 2048 + 1024 * 1024 * 2 - 1,
        attributes: 0,
        name: "EFI".into(),
    };
    let table = Table {
        disk_guid: Guid([7; 16]),
        slots: Default::default(),
        partitions: vec![esp.clone()],
    };
    let planned = plan(DISK, Some(&table), request(), counter()).unwrap();
    assert_eq!(planned.table.disk_guid, table.disk_guid);
    assert_eq!(planned.table.partitions[0], esp);
    assert!(planned.table.partitions[1].first_lba > esp.last_lba);
    // The same layout again is reused without a write.
    let again = plan(DISK, Some(&planned.table), request(), counter()).unwrap();
    assert!(!again.write);
    let bigger = Request {
        staging_bytes: 1024 * MIB,
        ..request()
    };
    assert!(plan(DISK, Some(&planned.table), bigger, counter()).is_err());
    let mut foreign = table.clone();
    foreign.partitions.push(Partition {
        type_guid: Guid([1; 16]),
        ..esp
    });
    assert!(plan(DISK, Some(&foreign), request(), counter()).is_err());
    let mut partial = planned.table.clone();
    partial.partitions.pop();
    assert!(plan(DISK, Some(&partial), request(), counter()).is_err());
}

/// A sparse disk: absent blocks read as zero.
#[derive(Clone, Default)]
struct Sparse {
    disk: Option<Disk>,
    blocks: alloc::collections::BTreeMap<u64, Vec<u8>>,
}

impl Sparse {
    fn new(disk: Disk) -> Self {
        Self {
            disk: Some(disk),
            ..Self::default()
        }
    }

    fn block_bytes(&self) -> usize {
        self.disk.unwrap().block_bytes as usize
    }

    /// Write the first `blocks` blocks of `write` (all with `None`).
    fn apply(&mut self, write: &Write, blocks: Option<usize>) {
        let size = self.block_bytes();
        for (n, block) in write
            .bytes
            .chunks(size)
            .enumerate()
            .take(blocks.unwrap_or(usize::MAX))
        {
            self.blocks.insert(write.lba + n as u64, block.to_vec());
        }
    }

    fn read(&self) -> Result<Found, PayloadError> {
        let disk = self.disk.unwrap();
        read(disk, |lba, count| {
            let mut out = Vec::new();
            for at in lba..lba + count {
                match self.blocks.get(&at) {
                    Some(block) => out.extend_from_slice(block),
                    None => out.resize(out.len() + disk.block_bytes as usize, 0),
                }
            }
            Ok(out)
        })
    }

    /// What setup does next: plan on what it read, and write the table
    /// again when the plan changes it or the read asks for a repair.
    fn rerun(&mut self) -> Table {
        let disk = self.disk.unwrap();
        let (existing, repair) = match self.read().unwrap() {
            Found::Blank | Found::Interrupted => (None, None),
            Found::Table { table, repair } => (Some(table), repair),
        };
        let plan = plan(disk, existing.as_ref(), request(), counter()).unwrap();
        if plan.write || repair.is_some() {
            for write in encode(disk, &plan.table).unwrap() {
                self.apply(&write, None);
            }
        }
        match self.read().unwrap() {
            Found::Table {
                table,
                repair: None,
            } => table,
            other => panic!("not repaired: {other:?}"),
        }
    }
}

#[test]
fn every_interrupted_write_to_a_blank_disk_is_resumed() {
    for block_bytes in [512, 4096] {
        let disk = Disk {
            block_bytes,
            blocks: DISK.blocks * 512 / block_bytes,
        };
        let planned = plan(disk, None, request(), counter()).unwrap().table;
        let writes = encode(disk, &planned).unwrap();
        assert_eq!(Sparse::new(disk).read().unwrap(), Found::Blank);
        for done in 0..=writes.len() {
            // Each write cut off after its first block, then each whole.
            for torn in [true, false] {
                if torn
                    && writes
                        .get(done)
                        .is_none_or(|w| w.bytes.len() <= block_bytes as usize)
                {
                    continue;
                }
                let mut image = Sparse::new(disk);
                for write in &writes[..done] {
                    image.apply(write, None);
                }
                if torn {
                    image.apply(&writes[done], Some(1));
                }
                let found = image.read().unwrap();
                let expected = match (done, torn) {
                    (0, false) => Found::Blank,
                    (0 | 1, _) => Found::Interrupted,
                    (2 | 3, _) => Found::Table {
                        table: planned.clone(),
                        repair: Some("the primary GPT is missing"),
                    },
                    (4, _) => Found::Table {
                        table: planned.clone(),
                        repair: Some("the protective MBR is missing"),
                    },
                    _ => Found::Table {
                        table: planned.clone(),
                        repair: None,
                    },
                };
                // Primary entries without their header read as missing too.
                assert_eq!(found, expected, "{block_bytes} {done} {torn}");
                let table = image.rerun();
                if done >= 2 {
                    // Committed to the backup: the same table, whole.
                    assert_eq!(table, planned);
                }
                assert_eq!(table.partitions.len(), 4);
            }
        }
    }
}

#[test]
fn an_interrupted_rewrite_keeps_the_existing_esp_and_its_bytes() {
    let esp = Partition {
        type_guid: Role::Esp.type_guid(),
        unique_guid: Guid([9; 16]),
        first_lba: 2048,
        last_lba: 2048 + 1024 * 1024 * 2 - 1,
        attributes: 0,
        name: "EFI".into(),
    };
    let old = Table {
        disk_guid: Guid([7; 16]),
        slots: Default::default(),
        partitions: vec![esp.clone()],
    };
    let mut base = Sparse::new(DISK);
    for write in encode(DISK, &old).unwrap() {
        base.apply(&write, None);
    }
    // The ESP's own blocks, which no GPT write may touch.
    let marker = vec![0x5a; 512];
    base.blocks.insert(esp.first_lba, marker.clone());
    base.blocks.insert(esp.last_lba, marker.clone());
    let new = plan(DISK, Some(&old), request(), counter()).unwrap().table;
    let writes = encode(DISK, &new).unwrap();
    for write in &writes {
        let end = write.lba + write.bytes.len() as u64 / 512;
        assert!(
            new.partitions
                .iter()
                .all(|p| end <= p.first_lba || write.lba > p.last_lba)
        );
    }
    for done in 0..=writes.len() {
        let mut image = base.clone();
        for write in &writes[..done] {
            image.apply(write, None);
        }
        let Found::Table { table, repair } = image.read().unwrap() else {
            panic!("no table after {done} writes");
        };
        let (want, why) = match done {
            0 => (&old, None),
            1 => (&old, Some("the backup GPT is damaged")),
            2 => (&old, Some("the backup GPT differs from the primary")),
            3 => (&new, Some("the primary GPT is damaged")),
            _ => (&new, None),
        };
        assert_eq!((&table, repair), (want, why), "{done}");
        let table = image.rerun();
        assert_eq!(table.partitions[0], esp);
        assert_eq!(table.disk_guid, old.disk_guid);
        assert_eq!(image.blocks[&esp.first_lba], marker);
        assert_eq!(image.blocks[&esp.last_lba], marker);
    }
}

#[test]
fn a_disk_without_a_gpt_that_is_not_blank_is_refused() {
    let mut mbr = Sparse::new(DISK);
    mbr.blocks.insert(0, vec![0x33; 512]);
    assert!(mbr.read().is_err());
    // Foreign bytes where a backup entry array would be.
    let mut tail = Sparse::new(DISK);
    tail.blocks.insert(DISK.blocks - 33, vec![0x44; 512]);
    assert!(tail.read().is_err());
    // A Stela entry array, but another block written too.
    let planned = plan(DISK, None, request(), counter()).unwrap().table;
    let mut mixed = Sparse::new(DISK);
    mixed.apply(&encode(DISK, &planned).unwrap()[0], None);
    mixed.blocks.insert(DISK.blocks - 34, vec![1; 512]);
    assert!(mixed.read().is_err());
    // A damaged primary with no backup is refused, never blank.
    let mut damaged = Sparse::new(DISK);
    let mut header = encode(DISK, &planned).unwrap()[3].clone();
    header.bytes[60] ^= 1;
    damaged.apply(&header, None);
    assert!(damaged.read().is_err());
    // A backup that names another system's partition is read, and the
    // plan refuses to write beside it.
    let mut other = planned.clone();
    other.partitions[0].type_guid = Guid([1; 16]);
    let mut foreign = Sparse::new(DISK);
    for write in &encode(DISK, &other).unwrap()[..2] {
        foreign.apply(write, None);
    }
    let Found::Table { table, repair } = foreign.read().unwrap() else {
        panic!("the backup is not read");
    };
    assert_eq!(repair, Some("the primary GPT is missing"));
    assert!(plan(DISK, Some(&table), request(), counter()).is_err());
}

#[test]
fn small_disks_damaged_headers_and_blank_blocks_are_told_apart() {
    let small = Disk {
        block_bytes: 512,
        blocks: 32 * 1024 * 1024 * 1024 / 512,
    };
    assert!(plan(small, None, request(), counter()).is_err());
    let plan = plan(DISK, None, request(), counter()).unwrap();
    let writes = encode(DISK, &plan.table).unwrap();
    let mut header = writes[3].bytes.clone();
    assert!(parse_header(DISK, 1, &vec![0; 512]).unwrap().is_none());
    header[60] ^= 1;
    assert!(parse_header(DISK, 1, &header).is_err());
    assert!(parse_header(DISK, 2, &writes[3].bytes).is_err());
    let info = parse_header(DISK, 1, &writes[3].bytes).unwrap().unwrap();
    let mut entries = writes[2].bytes.clone();
    entries[40] ^= 1;
    assert!(parse_entries(&info, &entries).is_err());
}

#[test]
fn guid_text_is_hex_digits_only_and_parses_as_from_str() {
    let esp: Guid = "C12A7328-F81F-11D2-BA4B-00A0C93EC93B".parse().unwrap();
    assert_eq!(esp, Guid::ESP);
    assert_eq!(Role::Esp.type_guid(), Guid::ESP);
    // A sign is not a hex digit, though `u8::from_str_radix` takes one.
    assert!(Guid::parse("+12A7328-F81F-11D2-BA4B-00A0C93EC93B").is_none());
    assert!(
        "C12A7328-F81F-11D2-BA4B-00A0C93EC93"
            .parse::<Guid>()
            .is_err()
    );
}

#[test]
fn entries_outside_the_header_usable_range_are_refused() {
    let plan = plan(DISK, None, request(), counter()).unwrap();
    let writes = encode(DISK, &plan.table).unwrap();
    let mut info = parse_header(DISK, 1, &writes[3].bytes).unwrap().unwrap();
    assert_eq!(info.first_usable, 34);
    assert_eq!(info.last_usable, DISK.blocks - 34);
    // A partition over the primary entry array.
    let mut entries = writes[2].bytes.clone();
    entries[32..40].copy_from_slice(&2u64.to_le_bytes());
    info.entries_crc = crc32(&entries[..info.entry_array_bytes() as usize]);
    assert_eq!(
        parse_entries(&info, &entries),
        Err(PayloadError::Disk("GPT entry outside the usable range"))
    );
    let mut header = writes[3].bytes.clone();
    header[40..48].copy_from_slice(&(DISK.blocks).to_le_bytes());
    header[16..20].fill(0);
    let crc = crc32(&header[..HEADER_BYTES as usize]);
    header[16..20].copy_from_slice(&crc.to_le_bytes());
    assert!(parse_header(DISK, 1, &header).is_err());
}

#[test]
fn a_usable_range_past_the_disk_reads_the_backup() {
    let plan = plan(DISK, None, request(), counter()).unwrap();
    let mut disk = Sparse::new(DISK);
    for write in encode(DISK, &plan.table).unwrap() {
        disk.apply(&write, None);
    }
    // The primary claims one more block than the disk has.
    let header = disk.blocks.get_mut(&1).unwrap();
    header[48..56].copy_from_slice(&DISK.blocks.to_le_bytes());
    header[16..20].fill(0);
    let crc = crc32(&header[..HEADER_BYTES as usize]);
    header[16..20].copy_from_slice(&crc.to_le_bytes());
    assert_eq!(
        disk.read(),
        Ok(Found::Table {
            table: plan.table,
            repair: Some("the primary GPT is damaged"),
        })
    );
}

#[test]
fn encoding_refuses_what_it_cannot_write_and_terminates_names() {
    let mut table = plan(DISK, None, request(), counter()).unwrap().table;
    table.partitions[0].name = "N".repeat(36);
    let writes = encode(DISK, &table).unwrap();
    assert_eq!(read_back(DISK, &writes).partitions[0].name, "N".repeat(35));
    assert_eq!(writes[2].bytes[56 + 70..56 + 72], [0, 0]);

    let mut reversed = table.clone();
    let partition = &mut reversed.partitions[0];
    core::mem::swap(&mut partition.first_lba, &mut partition.last_lba);
    assert!(encode(DISK, &reversed).is_err());
    let mut crowded = table.clone();
    let extra = crowded.partitions[0].clone();
    crowded.partitions.resize(ENTRY_COUNT as usize + 1, extra);
    assert_eq!(
        encode(DISK, &crowded),
        Err(PayloadError::Disk("too many partitions"))
    );
}

fn data_partition(tag: u8, first: u64) -> Partition {
    Partition {
        type_guid: "0FC63DAF-8483-4772-8E79-3D69D8477DE4".parse().unwrap(),
        unique_guid: Guid([tag; 16]),
        first_lba: first,
        last_lba: first + 2047,
        attributes: 0,
        name: "data".into(),
    }
}

#[test]
fn every_partition_stays_in_its_entry_across_a_rewrite() {
    // Entries 0 and 2 used, 1 left empty, and not in disk order: as a disk
    // that had a partition deleted, or one added in front of the others.
    let (a, b) = (data_partition(1, 1 << 20), data_partition(2, 2048));
    let table = Table {
        disk_guid: Guid([7; 16]),
        partitions: vec![a.clone(), b.clone()],
        slots: vec![0, 2],
    };
    let mut read = read_back(DISK, &encode(DISK, &table).unwrap());
    assert_eq!(read, table);
    // A partition added takes the empty entry; one removed frees its own
    // and moves no other.
    let c = data_partition(3, 4096);
    read.add(c.clone());
    let read = read_back(DISK, &encode(DISK, &read).unwrap());
    assert_eq!(read.partitions, [a.clone(), c.clone(), b.clone()]);
    assert_eq!(read.slots, [0, 1, 2]);
    let mut removed = read.clone();
    assert_eq!(removed.remove(0), a);
    let removed = read_back(DISK, &encode(DISK, &removed).unwrap());
    assert_eq!(removed.partitions, [c, b]);
    assert_eq!(removed.slots, [1, 2]);
    // A table made in memory takes the entries in order.
    let fresh = Table {
        disk_guid: Guid([7; 16]),
        partitions: vec![a.clone()],
        slots: Vec::new(),
    };
    assert_eq!(read_back(DISK, &encode(DISK, &fresh).unwrap()).slots, [0]);
}

#[test]
fn entries_the_table_cannot_hold_are_refused_not_moved() {
    let a = data_partition(1, 2048);
    let table = |slots: Vec<u32>| Table {
        disk_guid: Guid([7; 16]),
        partitions: vec![a.clone(); slots.len()],
        slots,
    };
    assert_eq!(
        encode(DISK, &table(vec![ENTRY_COUNT])),
        Err(PayloadError::Disk(
            "a partition is in an entry past the 128 this table holds"
        ))
    );
    assert_eq!(
        encode(DISK, &table(vec![3, 3])),
        Err(PayloadError::Disk("two partitions in one entry"))
    );
}

/// `header` with `value` at byte `at` and its checksum made anew, as
/// another tool writes a field of it.
fn relabel(header: &mut [u8], at: usize, value: &[u8]) {
    header[at..at + value.len()].copy_from_slice(value);
    reseal(header);
}

/// `header`'s checksum made anew over the bytes its size names.
fn reseal(header: &mut [u8]) {
    let size = le32(header, 12) as usize;
    header[16..20].fill(0);
    let crc = crc32(&header[..size]);
    header[16..20].copy_from_slice(&crc.to_le_bytes());
}

/// The primary and backup headers [`encode`] writes for `table`, each
/// changed by `edit` as another tool lays the GPT out.
fn headers(disk: Disk, table: &Table, edit: impl Fn(u64, &mut Vec<u8>)) -> (Vec<u8>, Vec<u8>) {
    let writes = encode(disk, table).unwrap();
    let (mut primary, mut backup) = (writes[3].bytes.clone(), writes[1].bytes.clone());
    edit(1, &mut primary);
    edit(disk.blocks - 1, &mut backup);
    (primary, backup)
}

/// Linux's fdisk (libfdisk): the usable blocks from 1 MiB.
fn fdisk(disk: Disk) -> impl Fn(u64, &mut Vec<u8>) {
    move |_, header: &mut Vec<u8>| {
        relabel(header, 40, &(ALIGNMENT / disk.block_bytes).to_le_bytes())
    }
}

fn data_table(partitions: Vec<Partition>) -> Table {
    Table {
        disk_guid: Guid([7; 16]),
        partitions,
        slots: Vec::new(),
    }
}

#[test]
fn a_rewrite_keeps_another_tools_headers_but_for_their_checksums() {
    for block_bytes in [512, 4096] {
        let disk = Disk {
            block_bytes,
            blocks: DISK.blocks * 512 / block_bytes,
        };
        let mib = ALIGNMENT / block_bytes;
        let was = data_table(vec![data_partition(1, mib)]);
        let (primary, backup) = headers(disk, &was, fdisk(disk));
        assert_eq!(
            kept_layout(disk, &primary, &backup),
            Ok(Usable {
                first: mib,
                last: disk.last_usable(),
            })
        );
        let mut table = was.clone();
        table.add(data_partition(2, 100 * mib));
        let writes = encode_keeping(disk, &table, &primary, &backup).unwrap();
        let lbas: Vec<u64> = writes.iter().map(|write| write.lba).collect();
        let last = disk.blocks - 1;
        assert_eq!(lbas, [last - disk.entry_blocks(), last, 2, 1, 0]);
        // The headers differ from the disk's only in their checksums, and
        // parse back to the table, its usable blocks the disk's own.
        for (write, was) in [(&writes[3], &primary), (&writes[1], &backup)] {
            let changed: Vec<usize> = (0..block_bytes as usize)
                .filter(|&at| write.bytes[at] != was[at])
                .collect();
            assert!(
                changed
                    .iter()
                    .all(|at| (16..20).contains(at) || (88..92).contains(at)),
                "{changed:?}"
            );
            let info = parse_header(disk, write.lba, &write.bytes)
                .unwrap()
                .unwrap();
            assert_eq!(
                (info.first_usable, info.last_usable),
                (mib, disk.last_usable())
            );
        }
        assert_eq!(read_back(disk, &writes), table);
        let info = keepable(disk, last, &writes[1].bytes).unwrap();
        assert_eq!(
            parse_entries(&info, &writes[0].bytes).unwrap(),
            table,
            "{block_bytes}"
        );
    }
}

#[test]
fn on_a_disk_encode_laid_out_a_rewrite_that_keeps_writes_what_encode_writes() {
    for block_bytes in [512, 1024, 2048, 4096] {
        let disk = Disk {
            block_bytes,
            blocks: DISK.blocks * 512 / block_bytes,
        };
        let mib = ALIGNMENT / block_bytes;
        let was = data_table(vec![data_partition(1, mib), data_partition(3, 50 * mib)]);
        let (primary, backup) = headers(disk, &was, |_, _| {});
        let mut table = was.clone();
        table.add(data_partition(2, 100 * mib));
        assert_eq!(
            encode_keeping(disk, &table, &primary, &backup).unwrap(),
            encode(disk, &table).unwrap(),
            "{block_bytes}"
        );
    }
}

#[test]
fn a_rewrite_that_keeps_refuses_a_partition_outside_the_headers_usable_blocks() {
    let mib = ALIGNMENT / DISK.block_bytes;
    let was = data_table(vec![data_partition(1, mib)]);
    let (primary, backup) = headers(DISK, &was, fdisk(DISK));
    // Before the first usable block fdisk names, though after encode's,
    // and past a last usable block narrowed by a MiB.
    let mut early = was.clone();
    early.add(Partition {
        first_lba: 34,
        last_lba: mib - 1,
        ..data_partition(2, 0)
    });
    assert_eq!(
        encode_keeping(DISK, &early, &primary, &backup),
        Err(PayloadError::Disk("partition outside the usable range"))
    );
    let narrowed = |_: u64, header: &mut Vec<u8>| {
        relabel(header, 48, &(DISK.last_usable() - mib).to_le_bytes())
    };
    let (primary, backup) = headers(DISK, &was, narrowed);
    let mut late = was.clone();
    late.add(Partition {
        first_lba: DISK.last_usable() - mib + 1,
        last_lba: DISK.last_usable(),
        ..data_partition(2, 0)
    });
    assert_eq!(
        encode_keeping(DISK, &late, &primary, &backup),
        Err(PayloadError::Disk("partition outside the usable range"))
    );
    // Another disk's GUID.
    let other = Table {
        disk_guid: Guid([8; 16]),
        ..was.clone()
    };
    assert_eq!(
        encode_keeping(DISK, &other, &primary, &backup),
        Err(PayloadError::Disk(
            "the table's disk GUID is not the one its headers name"
        ))
    );
}

#[test]
fn headers_a_rewrite_does_not_keep_are_refused_naming_the_field() {
    let last = DISK.blocks - 1;
    let was = data_table(vec![data_partition(1, 2048)]);
    let exactly = |field, found, kept| Unkept::Field {
        field,
        found,
        kept: Kept::Exactly(kept),
    };
    type Edit = fn(u64, &mut Vec<u8>);
    let cases: [(Edit, u64, Unkept); 13] = [
        (
            |lba, h| {
                if lba == 1 {
                    relabel(h, 8, &0x0001_0001u32.to_le_bytes())
                }
            },
            1,
            exactly("revision", 0x0001_0001, 0x0001_0000),
        ),
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
        (
            |lba, h| {
                if lba != 1 {
                    relabel(h, 72, &(lba - 33).to_le_bytes())
                }
            },
            last,
            exactly("entry array LBA", last - 33, last - 32),
        ),
        // Cloned from a smaller disk.
        (
            |lba, h| {
                if lba == 1 {
                    relabel(h, 32, &1_000_000u64.to_le_bytes())
                }
            },
            1,
            exactly("other copy's LBA", 1_000_000, last),
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
        // Over the primary entry array, and the backup's.
        (
            |_, h| relabel(h, 40, &33u64.to_le_bytes()),
            1,
            Unkept::Field {
                field: "first usable LBA",
                found: 33,
                kept: Kept::AtLeast(34),
            },
        ),
        (
            |lba, h| relabel(h, 48, &(lba.max(DISK.blocks - 1) - 32).to_le_bytes()),
            1,
            Unkept::Field {
                field: "last usable LBA",
                found: last - 32,
                kept: Kept::AtMost(last - 33),
            },
        ),
        // The copies disagree.
        (
            |lba, h| {
                if lba == 1 {
                    relabel(h, 40, &2048u64.to_le_bytes())
                }
            },
            last,
            Unkept::Disagree {
                field: "first usable LBA",
                primary: 2048,
                backup: 34,
            },
        ),
        (
            |lba, h| {
                if lba != 1 {
                    relabel(h, 56, &[9; 16])
                }
            },
            last,
            Unkept::OtherDisk,
        ),
        (|_, _| {}, 1, Unkept::Missing),
    ];
    for (index, (edit, lba, why)) in cases.into_iter().enumerate() {
        let (mut primary, backup) = headers(DISK, &was, edit);
        if why == Unkept::Missing {
            primary.fill(0);
        }
        assert_eq!(
            kept_layout(DISK, &primary, &backup),
            Err((lba, why)),
            "{index}"
        );
        assert_eq!(
            encode_keeping(DISK, &was, &primary, &backup),
            Err(PayloadError::Disk(
                "the disk's GPT headers are not ones a rewrite keeps"
            )),
            "{index}"
        );
    }
    // A damaged header: its checksum.
    let (mut primary, backup) = headers(DISK, &was, |_, _| {});
    primary[20] ^= 1;
    assert_eq!(
        kept_layout(DISK, &primary, &backup),
        Err((1, Unkept::Damaged("GPT header CRC mismatch")))
    );
    assert_eq!(
        alloc::format!(
            "{}",
            Unkept::Field {
                field: "first usable LBA",
                found: 33,
                kept: Kept::AtLeast(34),
            }
        ),
        "a GPT header names another layout than the one a rewrite keeps: its first usable LBA \
         is 33, where a rewrite keeps 34 or more"
    );
}

#[test]
fn a_protective_mbr_is_its_boot_signature_and_a_type_0xee_entry() {
    let plan = plan(DISK, None, request(), counter()).unwrap();
    let mut mbr = encode(DISK, &plan.table).unwrap().pop().unwrap().bytes;
    assert!(protective(&mbr));
    // Windows' own: a disk signature and other CHS bytes, the 0xEE entry
    // second.
    mbr[440..444].copy_from_slice(&[0xf4, 0x08, 0xdb, 0xc8]);
    mbr.copy_within(446..462, 462);
    mbr[446..462].fill(0);
    assert!(protective(&mbr));
    mbr[510] = 0;
    assert!(!protective(&mbr));
    assert!(!protective(&[0; 512]));
    assert!(!protective(&[0x55; 511]));
}

/// What a firmware reported for disks it lists without a usable size: the
/// UFS logical unit of a Lenovo 500w Gen 5 reported 0 bytes, and its
/// geometry divided by zero in `entry_blocks` when the picker drew it.
const NO_SIZE: [Disk; 3] = [
    Disk {
        block_bytes: 0,
        blocks: 1 << 20,
    },
    Disk {
        block_bytes: 4096,
        blocks: 0,
    },
    Disk {
        block_bytes: 0,
        blocks: 0,
    },
];

#[test]
fn a_disk_that_reports_no_size_has_no_usable_blocks_and_never_panics() {
    for disk in NO_SIZE {
        assert!(!disk.reports_size(), "{disk:?}");
        assert_eq!(disk.usable(), None, "{disk:?}");
        assert!(
            Disk::new(disk.block_bytes, disk.blocks).is_err(),
            "{disk:?}"
        );
        // Every query answers, whatever it answers, instead of panicking.
        let _ = (disk.entry_blocks(), disk.first_usable(), disk.last_usable());
        let _ = disk.minimum_blocks();
        let mut reads = 0;
        let found = read(disk, |_, _| {
            reads += 1;
            Ok(Vec::new())
        });
        assert!(found.is_err(), "{disk:?}");
        assert_eq!(reads, 0, "{disk:?} was read");
        assert!(plan(disk, None, request(), counter()).is_err(), "{disk:?}");
        assert!(parse_header(disk, 1, &[0; 512]).is_err(), "{disk:?}");
        let table = Table {
            disk_guid: Guid([1; 16]),
            partitions: Vec::new(),
            slots: Vec::new(),
        };
        assert!(encode(disk, &table).is_err(), "{disk:?}");
    }
}

#[test]
fn a_disk_too_small_for_a_gpt_has_no_usable_blocks() {
    for block_bytes in [512, 4096] {
        let least = Disk {
            block_bytes,
            blocks: 1,
        }
        .minimum_blocks();
        // 512-byte blocks: 2 * (2 + 32) + 1; 4096-byte blocks: 2 * (2 + 4) + 1.
        assert_eq!(least, if block_bytes == 512 { 69 } else { 13 });
        for blocks in [1, 2, 3, least / 2, least - 1] {
            let disk = Disk {
                block_bytes,
                blocks,
            };
            assert!(disk.reports_size());
            assert_eq!(disk.usable(), None, "{disk:?}");
            assert!(Disk::new(block_bytes, blocks).is_err(), "{disk:?}");
            // The usable range does not underflow on a tiny disk.
            assert_eq!(
                disk.last_usable(),
                blocks.saturating_sub(2 + disk.entry_blocks())
            );
            assert!(read(disk, |_, _| Ok(Vec::new())).is_err());
        }
        // The smallest disk that holds one has two usable blocks.
        let disk = Disk::new(block_bytes, least).unwrap();
        let (first, end) = disk.usable().unwrap();
        assert_eq!(end - first, 2);
    }
}

#[test]
fn a_valid_disk_keeps_its_geometry_and_usable_range() {
    assert_eq!(Disk::new(DISK.block_bytes, DISK.blocks).unwrap(), DISK);
    assert_eq!(DISK.entry_blocks(), 32);
    assert_eq!(DISK.first_usable(), 34);
    assert_eq!(DISK.last_usable(), DISK.blocks - 34);
    assert_eq!(DISK.usable(), Some((34, DISK.blocks - 33)));
    let big = Disk::new(4096, DISK.blocks / 8).unwrap();
    assert_eq!(big.usable(), Some((6, big.blocks - 5)));
    // A block size no GPT here is written with keeps its usable range,
    // as before, and `new` still refuses it.
    let odd = Disk {
        block_bytes: 8192,
        blocks: 1 << 20,
    };
    assert_eq!(odd.usable(), Some((4, (1 << 20) - 3)));
    assert!(Disk::new(odd.block_bytes, odd.blocks).is_err());
}
