//! The change to a PC disk that Install macOS's disk picker decided on,
//! which Stela's own drivers make after the firmware has exited.
//!
//! The picker runs while the firmware's boot services do and writes
//! nothing: a firmware disk driver gives its caller no say over timeouts or
//! recovery, so, as Linux does, only Stela's drivers write a PC disk. What
//! the picker decided is a [`Plan`] ([`plan`]): the disk as the picker saw
//! it, the partition table's blocks, the blocks to zero and the partition
//! macOS gets. The loader carries it in memory with the boot's other
//! pending disk work (`disk_writes`); once the drivers have started and
//! before the partition is looked up, the engine finds the disk
//! ([`Plan::locate`]) and makes the change ([`Plan::apply`]).
//!
//! A plan only adds a partition in free space beside the disk's own, or
//! blanks a Stela partition already there: no decision deletes a partition
//! or erases a disk, a disk without a GPT is left as it is, and a valid
//! protective MBR stays byte for byte. Before anything is written the plan
//! is checked against the disk as Stela's own driver reads it
//! ([`Plan::check`]): both copies of the GPT whole, alike and byte for byte
//! what the picker read through the firmware, every partition's entry the
//! same after the change, and every write in the GPT's own blocks or in the
//! new partition. A plan that only checks ([`Mode::Check`]) stops there: the
//! check is given a disk it can only read ([`ReadOnly`]). Otherwise each
//! write is read back before the next, so the backup GPT is whole and read
//! back before the primary is touched.
//!
//! A plan is made for one boot and never stored. A boot that stops before
//! it is applied leaves the disk as it was, and the next one finds no
//! chosen partition and chooses again.
use super::{
    gpt::{self, GptError, Guid},
    machine_state,
    storage::Target,
};
use alloc::{boxed::Box, format, string::String, vec, vec::Vec};
use core::fmt;
use stela_crypto::Sha256;
use stela_crypto::digest::Digest;
use stela_drivers::block::{BlockDevice, BlockError, Extent, Request};
use stela_formats::payload::{
    PayloadError,
    gpt::{self as shared, Disk, Partition, Role, Table, Write},
};

pub use read_only::ReadOnly;

const MIB: u64 = 1 << 20;
/// The blocks from the disk's start whose bytes tell the disk the picker
/// saw from another ([`seen`]): the MBR, with its disk signature, and the
/// GPT header, with the disk's GUID and its entries' checksum.
pub const SEEN_BLOCKS: u64 = 2;
/// The most bytes one zeroing request covers, so a failure names a block
/// near it and no single request runs long.
const ZERO_BYTES: u64 = MIB;
/// The least partition macOS is given: the system's minimum
/// (`gpt::DEFAULT_MIN_APPLE_BYTES`, which setup also checks) and the
/// machine state's tail after it. The picker offers no less, and a plan of
/// less is not applied.
pub const MIN_BYTES: u64 = shared::DEFAULT_MIN_APPLE_BYTES + machine_state::MINIMUM_BYTES;
/// One GPT entry, of the 128 [`shared::encode`] writes.
const ENTRY_BYTES: usize = 128;
/// The bytes of a GPT header a rewrite of the same layout changes: the
/// header's checksum and its entries'.
const CHECKSUMS: [core::ops::Range<usize>; 2] = [16..20, 88..92];

/// The digest of a disk's first [`SEEN_BLOCKS`] blocks.
pub fn seen(first_blocks: &[u8]) -> [u8; 32] {
    Sha256::digest(first_blocks).into()
}

mod read_only {
    use stela_drivers::block::{BlockDevice, BlockError, Identity};

    /// A disk that can only be read, which [`super::Plan::check`] is
    /// given: whatever the check does, it has no way to write, flush,
    /// discard or zero the disk. The device is out of reach of everything
    /// but this module.
    pub struct ReadOnly<'a>(&'a mut dyn BlockDevice);

    impl<'a> ReadOnly<'a> {
        pub fn new(device: &'a mut dyn BlockDevice) -> Self {
            Self(device)
        }

        pub fn identity(&self) -> &Identity {
            self.0.identity()
        }

        pub fn read(&mut self, lba: u64, buffer: &mut [u8]) -> Result<(), BlockError> {
            self.0.read(lba, buffer)
        }
    }
}

/// What a disk holds where its GPT lives: from block 0 to the first usable
/// block (the protective MBR, the primary header and entries), and after
/// the last usable block (the backup entries and header). A plan's table
/// writes go nowhere else.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ends {
    pub head: Vec<u8>,
    pub tail: Vec<u8>,
}

impl Ends {
    /// The head's and the tail's blocks on a disk of `geometry`; `None` for
    /// a block size or a size no GPT here uses.
    pub fn spans(geometry: Disk) -> Option<[Extent; 2]> {
        if !matches!(geometry.block_bytes, 512 | 1024 | 2048 | 4096)
            || geometry.blocks <= 2 * geometry.first_usable()
        {
            return None;
        }
        let tail = geometry.last_usable() + 1;
        Some([
            Extent {
                lba: 0,
                blocks: geometry.first_usable(),
            },
            Extent {
                lba: tail,
                blocks: geometry.blocks - tail,
            },
        ])
    }
}

/// What the boot does with a plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Make the change.
    Install,
    /// Check it against the disk, say every write it would make and write
    /// nothing ([`Plan::check`]).
    Check,
}

/// The partition macOS gets, as the disk holds it once the plan is applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Planned {
    pub guid: Guid,
    pub first_lba: u64,
    pub blocks: u64,
}

/// What Stela's drivers do to one disk before the engine looks up the
/// chosen partition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    /// The disk's serial number, as the firmware reported it.
    pub serial: String,
    /// The disk's block size and count, as the picker saw them.
    pub geometry: Disk,
    /// [`seen`] of the disk's first blocks, as the picker read them.
    pub seen: [u8; 32],
    /// The disk's GPT blocks as the picker read them through the firmware,
    /// which Stela's driver must read too before anything is written.
    pub ends: Ends,
    pub mode: Mode,
    /// The partition table's blocks in the order to write them
    /// ([`shared::encode_keeping`]'s, the disk's own headers but for their
    /// checksums, without its protective MBR where the disk has one); none
    /// when the table stays as it is.
    pub table: Vec<Write>,
    /// The blocks to zero: the partition's first MiB and the machine
    /// state's tail, so the restore and the NOR start blank.
    pub zero: Vec<Extent>,
    pub partition: Planned,
}

/// Where the picker put macOS.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Place {
    /// A new partition of `bytes` from block `first_lba`, in free space.
    Free { first_lba: u64, bytes: u64 },
    /// The Stela partition at this index of the disk's table.
    Reuse(usize),
}

/// What the picker's last page confirmed.
#[derive(Clone, Copy, Debug)]
pub struct Decision<'a> {
    pub serial: &'a str,
    pub geometry: Disk,
    /// The disk's GPT blocks ([`Ends`]), as the picker read them.
    pub ends: &'a Ends,
    /// The disk's GPT; `None` on a blank disk or one with another scheme,
    /// which no plan changes.
    pub table: Option<&'a Table>,
    pub place: Place,
    /// A new partition's GPT name.
    pub name: &'a str,
    pub mode: Mode,
}

/// Why a decision makes no plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlanError {
    NoSerial,
    /// The block size does not divide a MiB.
    Blocks(u64),
    /// The picker's read of the disk's GPT blocks is not of their size.
    Ends,
    /// The disk has no GPT: only a table that stays beside the new
    /// partition is written, never a new one.
    NoTable,
    /// The reused partition is not in the table, or is not a Stela
    /// partition.
    NotReusable,
    /// The new partition is not whole blocks, or overlaps one that stays.
    Overlap,
    /// The partition cannot hold a namespace and the machine state.
    TooSmall,
    /// The disk's GPT header at `lba` is not one a rewrite keeps
    /// ([`shared::kept_layout`]).
    Layout {
        lba: u64,
        why: shared::Unkept,
    },
    /// The new table cannot be encoded.
    Table(&'static str),
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSerial => f.write_str("the disk reports no serial number"),
            Self::Blocks(bytes) => write!(f, "the disk's {bytes}-byte blocks do not divide a MiB"),
            Self::Ends => f.write_str("the disk's partition table blocks were not read whole"),
            Self::NoTable => f.write_str(
                "the disk has no partition table to add macOS to, and this build never writes a \
                 new one",
            ),
            Self::NotReusable => f.write_str("the partition chosen is not a Stambha partition"),
            Self::Overlap => f.write_str("the new partition overlaps one that stays"),
            Self::TooSmall => f.write_str("the partition is too small"),
            Self::Layout { lba, why } => {
                write!(
                    f,
                    "the disk's partition table at block {lba} is not added to: {why}"
                )
            }
            Self::Table(why) => write!(f, "the new partition table is invalid: {why}"),
        }
    }
}

/// The plan that carries out `decision`. `entropy` gives the new
/// partition's GUID. The table is written only for a new partition, which
/// takes the first unused entry, every other partition keeping its own;
/// the disk's protective MBR is written only when it has none. A reused
/// partition has its blocks zeroed and nothing else. That the partition
/// is as large as macOS needs ([`MIN_BYTES`]) is the picker's to offer
/// and [`Plan::check`]'s to require.
pub fn plan(decision: &Decision<'_>, entropy: &mut dyn FnMut() -> Guid) -> Result<Plan, PlanError> {
    let Decision {
        serial,
        geometry,
        ends,
        table,
        place,
        name,
        mode,
    } = *decision;
    if serial.is_empty() {
        return Err(PlanError::NoSerial);
    }
    let block = geometry.block_bytes;
    if block == 0 || !MIB.is_multiple_of(block) {
        return Err(PlanError::Blocks(block));
    }
    let [head, tail] = Ends::spans(geometry).ok_or(PlanError::Table("the disk is too small"))?;
    if ends.head.len() as u64 != head.blocks * block
        || ends.tail.len() as u64 != tail.blocks * block
    {
        return Err(PlanError::Ends);
    }
    let mut new = table.ok_or(PlanError::NoTable)?.clone();
    let (guid, first_lba, blocks) = match place {
        Place::Reuse(index) => {
            let partition = new
                .partitions
                .get(index)
                .filter(|partition| partition.role() == Some(Role::AppleRegion))
                .ok_or(PlanError::NotReusable)?;
            (
                partition.unique_guid,
                partition.first_lba,
                partition.blocks(),
            )
        }
        Place::Free { first_lba, bytes } => {
            if bytes == 0 || !bytes.is_multiple_of(block) {
                return Err(PlanError::Overlap);
            }
            let blocks = bytes / block;
            let last_lba = first_lba
                .checked_add(blocks - 1)
                .ok_or(PlanError::Overlap)?;
            if new
                .partitions
                .iter()
                .any(|kept| kept.first_lba <= last_lba && first_lba <= kept.last_lba)
            {
                return Err(PlanError::Overlap);
            }
            let guid = entropy();
            // In the first unused entry, the others left in theirs: the
            // partitions' numbers stay what the systems on the disk know
            // them by.
            new.add(Partition {
                type_guid: Role::AppleRegion.type_guid(),
                unique_guid: guid,
                first_lba,
                last_lba,
                attributes: 0,
                name: name.into(),
            });
            (guid, first_lba, blocks)
        }
    };
    let split = machine_state::split(blocks * block).map_err(|_| PlanError::TooSmall)?;
    let table = match place {
        Place::Reuse(_) => Vec::new(),
        Place::Free { .. } => {
            // The disk's own headers, as the picker read them, but for
            // their checksums: the layout another tool chose stays, and
            // every partition lies in the blocks its headers name.
            let entries = (geometry.entry_blocks() * block) as usize;
            let (primary, backup) = (
                &ends.head[block as usize..2 * block as usize],
                &ends.tail[entries..],
            );
            shared::kept_layout(geometry, primary, backup)
                .map_err(|(lba, why)| PlanError::Layout { lba, why })?;
            let mut writes = shared::encode_keeping(geometry, &new, primary, backup).map_err(
                |error| match error {
                    PayloadError::Disk(why) => PlanError::Table(why),
                    _ => PlanError::Table("it cannot be encoded"),
                },
            )?;
            // The disk's own protective MBR stays: the one `encode` builds
            // drops its disk signature and other bytes the systems on the
            // disk wrote.
            if shared::protective(&ends.head[..block as usize]) {
                writes.retain(|write| write.lba != 0);
            }
            writes
        }
    };
    Ok(Plan {
        serial: serial.into(),
        geometry,
        seen: seen(&ends.head[..(SEEN_BLOCKS * block) as usize]),
        ends: ends.clone(),
        mode,
        table,
        zero: vec![
            Extent {
                lba: first_lba,
                blocks: MIB / block,
            },
            Extent {
                lba: first_lba + split.namespace_bytes / block,
                blocks: split.state_bytes / block,
            },
        ],
        partition: Planned {
            guid,
            first_lba,
            blocks,
        },
    })
}

/// A part of [`Plan::apply`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Telling that the disk is the one the picker saw.
    Identify,
    /// Checking the plan against the disk ([`Plan::check`]).
    Check,
    /// The trial write in the new partition, before the table's.
    Probe,
    /// Writing the partition table.
    Table,
    /// Zeroing the partition's first MiB and the machine state.
    Zero,
    /// Making the writes durable.
    Flush,
    /// Reading the table and the zeroed blocks back.
    Verify,
}

impl Step {
    fn name(self) -> &'static str {
        match self {
            Self::Identify => "checking that it is the disk chosen",
            Self::Check => "checking the plan against the disk",
            Self::Probe => "trying a write in the new partition",
            Self::Table => "writing the partition table",
            Self::Zero => "zeroing the partition's first MiB and machine state",
            Self::Flush => "making the writes durable",
            Self::Verify => "reading the partition table back",
        }
    }
}

/// What stopped a step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cause {
    /// The driver's error.
    Device(BlockError),
    /// The disk has another block size or count than the picker saw: a
    /// different disk answers to the serial.
    Geometry { found: Disk },
    /// The disk's GPT blocks are not what the picker read.
    Changed,
    /// The disk's two copies of the GPT are not whole, alike and in the
    /// layout a rewrite keeps: why.
    Copies(&'static str),
    /// A GPT header of the disk names a layout a rewrite does not keep, or
    /// the two name different ones: the field and its values.
    Layout(shared::Unkept),
    /// The plan writes what no plan writes: why.
    Refused(&'static str),
    /// The plan changes the entry of a partition that stays, numbered from
    /// 1 as the systems on the disk number it.
    Entry { entry: usize },
    /// The new partition overlaps this partition.
    Overlap { entry: usize },
    /// The partition holds these bytes, less than [`MIN_BYTES`].
    TooSmall { bytes: u64 },
    /// The plan only checks the disk.
    CheckOnly,
    /// Blocks written read back otherwise.
    ReadBack,
    /// The table read back cannot be used.
    Table(GptError),
    /// The table read back does not hold the chosen partition as planned.
    Missing,
    /// A block that was zeroed does not read as zero.
    NotZero,
}

/// Why a plan was not applied in full: the step, the block it was at when
/// it has one, and the cause.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplyError {
    pub step: Step,
    pub lba: Option<u64>,
    pub cause: Cause,
}

fn error(step: Step, lba: Option<u64>, cause: Cause) -> ApplyError {
    ApplyError { step, lba, cause }
}

impl fmt::Display for ApplyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.step.name())?;
        if let Some(lba) = self.lba {
            write!(f, " at block {lba}")?;
        }
        match &self.cause {
            Cause::Device(error) => write!(f, ": {error}"),
            Cause::Geometry { found } => write!(
                f,
                ": the disk has {} blocks of {} bytes, not what was chosen",
                found.blocks, found.block_bytes
            ),
            Cause::Changed => f.write_str(
                ": the disk's partition table blocks are not what the picker read through the \
                 firmware",
            ),
            Cause::Copies(why) | Cause::Refused(why) => write!(f, ": {why}"),
            Cause::Layout(why) => write!(f, ": {why}"),
            Cause::Entry { entry } => write!(
                f,
                ": the plan changes the entry of partition {entry}, which stays as it is"
            ),
            Cause::Overlap { entry } => {
                write!(f, ": the new partition overlaps partition {entry}")
            }
            Cause::TooSmall { bytes } => write!(
                f,
                ": the partition holds {bytes} bytes, less than macOS's {MIN_BYTES}"
            ),
            Cause::CheckOnly => f.write_str(": the plan only checks the disk and writes nothing"),
            Cause::ReadBack => f.write_str(": the blocks read back are not what was written"),
            Cause::Table(error) => write!(f, ": {error}"),
            Cause::Missing => f.write_str(": the chosen partition is not in it"),
            Cause::NotZero => f.write_str(": a zeroed block does not read as zero"),
        }
    }
}

/// Why no disk is the plan's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocateError {
    /// No disk reports the serial, and none of the chosen size holds the
    /// first blocks the picker read.
    NoDisk,
    /// This many disks could be it.
    Ambiguous(usize),
}

impl fmt::Display for LocateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoDisk => f.write_str("Stambha's drivers found no disk that is the one chosen"),
            Self::Ambiguous(count) => write!(f, "{count} disks could be the one chosen"),
        }
    }
}

/// What [`Plan::check`] found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checked {
    /// The table the plan writes, as its writes hold it; `None` when it
    /// writes none.
    pub table: Option<Table>,
}

/// Blocks as the check's lines name them.
struct Span {
    lba: u64,
    blocks: u64,
    block: u64,
}

impl fmt::Display for Span {
    /// A plan's blocks are said before it is checked ([`Plan::check`]), so
    /// a damaged plan's are said too: its last block and its bytes in
    /// `u128`, where any `u64` count and size fit.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (lba, blocks, block) = (self.lba, self.blocks, self.block);
        match blocks {
            0 => write!(f, "LBA {lba} (0 blocks, 0 bytes)"),
            1 => write!(f, "LBA {lba} (1 block, {block} bytes)"),
            _ => write!(
                f,
                "LBA {lba}-{} ({blocks} blocks, {} bytes)",
                u128::from(lba) + u128::from(blocks) - 1,
                u128::from(blocks) * u128::from(block)
            ),
        }
    }
}

/// The first block of `found` whose bytes are not `expected`'s, counted
/// from the first; `Some(0)` for buffers of different sizes.
fn first_difference(found: &[u8], expected: &[u8], block: u64) -> Option<u64> {
    if found.len() != expected.len() {
        return Some(0);
    }
    found
        .chunks(block as usize)
        .zip(expected.chunks(block as usize))
        .position(|(found, expected)| found != expected)
        .map(|index| index as u64)
}

/// The disk's table as both copies of its GPT hold it, from its [`Ends`],
/// and the blocks its headers give partitions: each header valid at its
/// own block and one a rewrite keeps ([`shared::keepable`]: naming the
/// other copy and 128 entries of 128 bytes beside it, and usable blocks
/// that are none of the GPT's own), both headers naming the same usable
/// blocks, each entry array its header's, both arrays the same bytes, and
/// no two of the partitions overlapping.
#[expect(
    clippy::result_large_err,
    reason = "the driver's error is kept whole for the stop's message, and one plan is checked a boot"
)]
fn copies(geometry: Disk, ends: &Ends) -> Result<(Table, shared::Usable), ApplyError> {
    let block = geometry.block_bytes as usize;
    let entry_blocks = geometry.entry_blocks();
    let entries = entry_blocks as usize * block;
    let last = geometry.blocks - 1;
    let backup_entries_lba = last - entry_blocks;
    let refuse = |lba, why| error(Step::Check, Some(lba), Cause::Copies(why));
    let primary = (
        1,
        &ends.head[block..2 * block],
        2,
        &ends.head[2 * block..2 * block + entries],
    );
    let backup = (
        last,
        &ends.tail[entries..],
        backup_entries_lba,
        &ends.tail[..entries],
    );
    let mut tables = Vec::new();
    let mut usable = Vec::new();
    for (lba, header, entries_lba, array) in [primary, backup] {
        let info = shared::keepable(geometry, lba, header).map_err(|why| match why {
            shared::Unkept::Missing => refuse(lba, "a GPT header is missing"),
            shared::Unkept::Field { .. } => error(Step::Check, Some(lba), Cause::Layout(why)),
            _ => refuse(lba, "a GPT header is damaged"),
        })?;
        let table = shared::parse_entries(&info, array)
            .map_err(|_| refuse(entries_lba, "a GPT entry array is damaged"))?;
        tables.push(table);
        usable.push(shared::Usable {
            first: info.first_usable,
            last: info.last_usable,
        });
    }
    for (field, primary, backup) in [
        ("first usable LBA", usable[0].first, usable[1].first),
        ("last usable LBA", usable[0].last, usable[1].last),
    ] {
        if primary != backup {
            return Err(error(
                Step::Check,
                Some(last),
                Cause::Layout(shared::Unkept::Disagree {
                    field,
                    primary,
                    backup,
                }),
            ));
        }
    }
    if primary.3 != backup.3 || tables[0] != tables[1] {
        return Err(refuse(
            backup_entries_lba,
            "the backup GPT is not the primary's copy",
        ));
    }
    let table = tables.swap_remove(0);
    for (index, one) in table.partitions.iter().enumerate() {
        let overlapped = table.partitions[index + 1..]
            .iter()
            .any(|other| one.first_lba <= other.last_lba && other.first_lba <= one.last_lba);
        if overlapped {
            return Err(refuse(
                2 + (table.slot(index) as usize * ENTRY_BYTES / block) as u64,
                "two of the disk's partitions overlap",
            ));
        }
    }
    Ok((table, usable[0]))
}

/// The table the backup copy in `tail` holds, if it is whole.
fn backup_copy(geometry: Disk, tail: &[u8]) -> Option<Table> {
    let entries = (geometry.entry_blocks() * geometry.block_bytes) as usize;
    let info = shared::parse_header(geometry, geometry.blocks - 1, &tail[entries..]).ok()??;
    shared::parse_entries(&info, &tail[..entries]).ok()
}

/// Read both of `device`'s [`Ends`], at `step`.
#[expect(
    clippy::result_large_err,
    reason = "the driver's error is kept whole for the stop's message, and one plan is checked a boot"
)]
fn read_ends(
    geometry: Disk,
    step: Step,
    mut read: impl FnMut(u64, &mut [u8]) -> Result<(), BlockError>,
) -> Result<Ends, ApplyError> {
    let [head, tail] = Ends::spans(geometry).ok_or(error(
        Step::Check,
        None,
        Cause::Copies("the disk is too small for a GPT"),
    ))?;
    let block = geometry.block_bytes;
    let mut ends = Ends {
        head: vec![0; (head.blocks * block) as usize],
        tail: vec![0; (tail.blocks * block) as usize],
    };
    for (span, buffer) in [(head, &mut ends.head), (tail, &mut ends.tail)] {
        read(span.lba, buffer)
            .map_err(|cause| error(step, Some(span.lba), Cause::Device(cause)))?;
    }
    Ok(ends)
}

const MAGIC: &[u8; 8] = b"STELAPLN";

impl Plan {
    /// The partition as `stela.cfg`'s options name it.
    pub fn target(&self) -> Target {
        Target {
            serial: self.serial.clone(),
            partition: self.partition.guid,
        }
    }

    /// Whether the plan is for the partition `target` names.
    pub fn is_for(&self, target: &Target) -> bool {
        self.serial == target.serial && self.partition.guid == target.partition
    }

    fn geometry_of(device: &dyn BlockDevice) -> Disk {
        gpt::geometry(device.identity())
    }

    /// Whether `device` has the plan's geometry and first blocks.
    fn saw(&self, device: &mut dyn BlockDevice) -> Result<bool, BlockError> {
        if Self::geometry_of(device) != self.geometry {
            return Ok(false);
        }
        let mut first = vec![0; (SEEN_BLOCKS * self.geometry.block_bytes) as usize];
        device.read(0, &mut first)?;
        Ok(seen(&first) == self.seen)
    }

    /// The index in `devices` of the plan's disk: the one reporting its
    /// serial. The firmware read that serial; Stela's own driver may report
    /// the same disk under another (a USB bridge's INQUIRY serial against
    /// its USB string) or none. When no disk has the serial, the disk is
    /// the only one of the chosen size whose first blocks are what the
    /// picker read.
    pub fn locate(&self, devices: &mut [Box<dyn BlockDevice>]) -> Result<usize, LocateError> {
        let named: Vec<usize> = (0..devices.len())
            .filter(|&index| devices[index].identity().serial == self.serial)
            .collect();
        let found = if named.is_empty() {
            (0..devices.len())
                .filter(|&index| self.saw(&mut *devices[index]).unwrap_or(false))
                .collect()
        } else {
            named
        };
        match found[..] {
            [index] => Ok(index),
            [] => Err(LocateError::NoDisk),
            _ => Err(LocateError::Ambiguous(found.len())),
        }
    }

    fn span(&self, lba: u64, blocks: u64) -> Span {
        Span {
            lba,
            blocks,
            block: self.geometry.block_bytes,
        }
    }

    /// What the table write at `lba` holds.
    fn names(&self, lba: u64) -> &'static str {
        let last = self.geometry.blocks - 1;
        match lba {
            0 => "the protective MBR",
            1 => "the primary GPT header",
            2 => "the primary GPT entries",
            lba if lba == last => "the backup GPT header",
            lba if lba == last - self.geometry.entry_blocks() => "the backup GPT entries",
            _ => "blocks outside the GPT",
        }
    }

    /// The write that tries Stela's driver in the new partition before
    /// any of the table is written: as many blocks as an entry array, from
    /// the partition's first, each holding its block number; the first
    /// MiB's zeroing clears it. A damaged plan's, said before the check
    /// refuses a partition past the last LBA, numbers its blocks wrapping.
    fn probe(&self) -> Write {
        let block = self.geometry.block_bytes as usize;
        let mut bytes = vec![0x5a; self.geometry.entry_blocks() as usize * block];
        for (index, part) in bytes.chunks_mut(block).enumerate() {
            let lba = self.partition.first_lba.wrapping_add(index as u64);
            part[..8].copy_from_slice(b"STELATRY");
            part[8..16].copy_from_slice(&lba.to_le_bytes());
        }
        Write {
            lba: self.partition.first_lba,
            bytes,
        }
    }

    /// Say every write the plan makes, in order.
    fn say_plan(&self, say: &mut dyn FnMut(&str)) {
        let block = self.geometry.block_bytes;
        let partition = self.partition;
        say(match self.mode {
            Mode::Check => "PLAN check only: nothing below is written",
            Mode::Install => "PLAN install: each write is read back before the next",
        });
        say(&format!(
            "PLAN the disk: {} blocks of {block} bytes",
            self.geometry.blocks
        ));
        // Every plan [`plan`] makes names a disk a GPT here uses, whose
        // writes the lines below can name; a damaged one is refused next.
        if Ends::spans(self.geometry).is_none() {
            say("PLAN no GPT here uses such a disk: the plan is refused");
            return;
        }
        let whole = self.span(partition.first_lba, partition.blocks);
        say(&if self.table.is_empty() {
            format!("PLAN the macOS partition: the Stela partition at {whole}, blanked")
        } else {
            format!("PLAN the macOS partition: new, at {whole}")
        });
        let mut number = 0;
        let mut write = |say: &mut dyn FnMut(&str), what: &str, lba, bytes: usize| {
            number += 1;
            say(&format!(
                "PLAN write {number}: {what}, {}",
                self.span(lba, bytes as u64 / block)
            ));
        };
        if self.table.is_empty() {
            say("PLAN the partition table is not written");
        } else {
            let probe = self.probe();
            write(
                say,
                "a trial write in the new partition, read back and zeroed below",
                probe.lba,
                probe.bytes.len(),
            );
            for table in &self.table {
                write(say, self.names(table.lba), table.lba, table.bytes.len());
            }
            if self.table.iter().all(|table| table.lba != 0) {
                say("PLAN the protective MBR at LBA 0 is valid: it is not written");
            }
        }
        for (index, extent) in self.zero.iter().enumerate() {
            let what = match index {
                0 => "the macOS partition's first MiB",
                1 => "the machine state in its tail",
                _ => "a range of the macOS partition",
            };
            say(&format!(
                "PLAN zero {}: {what}, {}, by Write Zeroes, not TRIM",
                index + 1,
                self.span(extent.lba, extent.blocks)
            ));
        }
        say("PLAN nothing else is written, and nothing is trimmed");
    }

    /// Check the plan against `disk`, the whole disk as Stela's own driver
    /// reads it, saying every write the plan makes (`PLAN` lines) and each
    /// check it passes (`PASS`). The disk only reads, so the check writes
    /// nothing, whatever it finds.
    ///
    /// The disk must have the geometry the picker saw and hold at both ends
    /// the bytes the picker read through the firmware; both copies of its
    /// GPT must be whole and alike. The partition must lie in the blocks
    /// the GPT gives partitions and hold at least [`MIN_BYTES`]. A reused
    /// one must be that Stela partition on the disk. A new one must overlap
    /// no partition, and the table written must be the backup entries and
    /// header, then the primary's, with a protective MBR only for a disk
    /// without one: every entry of the disk byte for byte as it is, but one
    /// unused entry that takes the planned Stela partition, and each header
    /// as it is but for its two checksums. Each range to zero, and the
    /// trial write, lie in the partition.
    #[expect(
        clippy::result_large_err,
        reason = "the driver's error is kept whole for the stop's message, and one plan is checked a boot"
    )]
    pub fn check(
        &self,
        disk: &mut ReadOnly<'_>,
        say: &mut dyn FnMut(&str),
    ) -> Result<Checked, ApplyError> {
        self.say_plan(say);
        let geometry = self.geometry;
        let block = geometry.block_bytes;
        let found = gpt::geometry(disk.identity());
        if found != geometry {
            return Err(error(Step::Identify, None, Cause::Geometry { found }));
        }
        say(&format!(
            "PASS the disk has {} blocks of {block} bytes, as the picker saw",
            geometry.blocks
        ));
        let now = read_ends(geometry, Step::Identify, |lba, buffer| {
            disk.read(lba, buffer)
        })?;
        let [head, tail] = Ends::spans(geometry).expect("read");
        for (what, span, read, picked) in [
            ("primary", head, &now.head, &self.ends.head),
            ("backup", tail, &now.tail, &self.ends.tail),
        ] {
            if let Some(index) = first_difference(read, picked, block) {
                return Err(error(
                    Step::Identify,
                    Some(span.lba + index),
                    Cause::Changed,
                ));
            }
            say(&format!(
                "PASS the {what} GPT's blocks, {}, read through Stela's driver are byte for byte \
                 what the picker read through the firmware",
                self.span(span.lba, span.blocks)
            ));
        }
        let (existing, usable) = copies(geometry, &now)?;
        say(&format!(
            "PASS both GPT copies are whole and alike: headers at LBA 1 and {}, 128 entries of \
             128 bytes at LBA 2 and {}, usable LBA {}-{}, {} partitions",
            geometry.blocks - 1,
            tail.lba,
            usable.first,
            usable.last,
            existing.partitions.len()
        ));
        let partition = self.partition;
        let refuse = |lba, why| error(Step::Check, Some(lba), Cause::Refused(why));
        let last_lba = partition
            .first_lba
            .checked_add(partition.blocks.saturating_sub(1))
            .filter(|_| partition.blocks != 0)
            .ok_or(refuse(
                partition.first_lba,
                "the macOS partition has no blocks",
            ))?;
        // The blocks the disk's headers give partitions, which another tool
        // may have made fewer than the geometry's.
        if partition.first_lba < usable.first || last_lba > usable.last {
            return Err(refuse(
                partition.first_lba,
                "the macOS partition lies outside the blocks the GPT gives partitions",
            ));
        }
        let bytes = partition.blocks * block;
        if bytes < MIN_BYTES || machine_state::split(bytes).is_err() {
            return Err(error(
                Step::Check,
                Some(partition.first_lba),
                Cause::TooSmall { bytes },
            ));
        }
        let whole = self.span(partition.first_lba, partition.blocks);
        let table = if self.table.is_empty() {
            let reused = existing
                .partitions
                .iter()
                .position(|found| {
                    found.unique_guid == partition.guid
                        && found.role() == Some(Role::AppleRegion)
                        && found.first_lba == partition.first_lba
                        && found.blocks() == partition.blocks
                })
                .ok_or(refuse(
                    partition.first_lba,
                    "the partition to reuse is not that Stambha partition on the disk",
                ))?;
            say(&format!(
                "PASS the partition reused, entry {}, is the Stela partition at {whole}; the \
                 partition table is not written",
                existing.slot(reused) + 1
            ));
            None
        } else {
            for (index, kept) in existing.partitions.iter().enumerate() {
                if kept.first_lba <= last_lba && partition.first_lba <= kept.last_lba {
                    return Err(error(
                        Step::Check,
                        Some(partition.first_lba),
                        Cause::Overlap {
                            entry: existing.slot(index) as usize + 1,
                        },
                    ));
                }
                if kept.unique_guid == partition.guid {
                    return Err(refuse(
                        partition.first_lba,
                        "the new partition's GUID is a partition's on the disk",
                    ));
                }
            }
            say(&format!(
                "PASS the new partition, {whole}, lies in free space: it overlaps none of the \
                 disk's {} partitions and holds at least {MIN_BYTES} bytes",
                existing.partitions.len()
            ));
            Some(self.check_table(&now, &existing, say)?)
        };
        let inside = |lba: u64, blocks: u64| {
            blocks != 0
                && lba >= partition.first_lba
                && lba
                    .checked_add(blocks)
                    .is_some_and(|end| end <= last_lba + 1)
        };
        for extent in &self.zero {
            if !inside(extent.lba, extent.blocks) {
                return Err(refuse(
                    extent.lba,
                    "a range to zero lies outside the macOS partition",
                ));
            }
        }
        if table.is_some() {
            let probe = self.probe();
            let blocks = probe.bytes.len() as u64 / block;
            let zeroed = self.zero.iter().any(|extent| {
                probe.lba >= extent.lba && probe.lba + blocks <= extent.lba + extent.blocks
            });
            if !inside(probe.lba, blocks) || !zeroed {
                return Err(refuse(
                    probe.lba,
                    "the trial write would not be zeroed in the macOS partition",
                ));
            }
        }
        say(
            "PASS every write lies in the GPT's own blocks or in the macOS partition, and none is \
             a TRIM",
        );
        Ok(Checked { table })
    }

    /// The table the plan writes, checked against `now`, the disk's GPT
    /// blocks, and `existing`, the table they hold.
    #[expect(
        clippy::result_large_err,
        reason = "the driver's error is kept whole for the stop's message, and one plan is checked a boot"
    )]
    fn check_table(
        &self,
        now: &Ends,
        existing: &Table,
        say: &mut dyn FnMut(&str),
    ) -> Result<Table, ApplyError> {
        let geometry = self.geometry;
        let block = geometry.block_bytes as usize;
        let entry_blocks = geometry.entry_blocks();
        let last = geometry.blocks - 1;
        let refuse = |lba, why| error(Step::Check, Some(lba), Cause::Refused(why));
        let keep_mbr = shared::protective(&now.head[..block]);
        let mut expected = vec![
            (last - entry_blocks, entry_blocks),
            (last, 1),
            (2, entry_blocks),
            (1, 1),
        ];
        if !keep_mbr {
            expected.push((0, 1));
        }
        let at = |index: usize| self.table.get(index).map_or(0, |write| write.lba);
        if self.table.len() != expected.len() {
            return Err(refuse(
                at(expected.len().min(self.table.len())),
                "the table's writes are not the backup GPT, the primary and, for a disk without \
                 one, a protective MBR",
            ));
        }
        for (index, (write, &(lba, blocks))) in self.table.iter().zip(&expected).enumerate() {
            if write.lba != lba || write.bytes.len() != (blocks as usize) * block {
                return Err(refuse(
                    at(index),
                    "a table write is not in the GPT's own blocks, in the order that keeps a \
                     whole copy",
                ));
            }
        }
        if !keep_mbr && !shared::protective(&self.table[4].bytes) {
            return Err(refuse(0, "the MBR the plan writes is not a protective one"));
        }
        let [
            backup_entries,
            backup_header,
            primary_entries,
            primary_header,
        ] = [0, 1, 2, 3].map(|index| &self.table[index].bytes[..]);
        if backup_entries != primary_entries {
            return Err(refuse(
                last - entry_blocks,
                "the plan's backup entries are not its primary's",
            ));
        }
        let entries = (entry_blocks as usize) * block;
        let old = &now.head[2 * block..2 * block + entries];
        let mut added = None;
        for (slot, (was, will)) in old
            .chunks(ENTRY_BYTES)
            .zip(primary_entries.chunks(ENTRY_BYTES))
            .enumerate()
        {
            if was == will {
                continue;
            }
            let lba = 2 + (slot * ENTRY_BYTES / block) as u64;
            if was.iter().any(|&byte| byte != 0) {
                return Err(error(
                    Step::Check,
                    Some(lba),
                    Cause::Entry { entry: slot + 1 },
                ));
            }
            if added.replace(slot).is_some() {
                return Err(refuse(lba, "the plan adds more than one partition"));
            }
        }
        let slot = added.ok_or(refuse(2, "the plan adds no partition"))?;
        for (lba, was, will) in [
            (1, &now.head[block..2 * block], primary_header),
            (last, &now.tail[entries..], backup_header),
        ] {
            let changed = was.iter().zip(will).enumerate().any(|(at, (was, will))| {
                was != will && !CHECKSUMS.iter().any(|checksum| checksum.contains(&at))
            });
            if changed {
                return Err(refuse(
                    lba,
                    "a GPT header the plan writes changes more than its two checksums",
                ));
            }
        }
        let parsed = |lba, header, array| {
            let info = shared::parse_header(geometry, lba, header).ok().flatten()?;
            shared::parse_entries(&info, array).ok()
        };
        let planned = parsed(1, primary_header, primary_entries)
            .ok_or(refuse(1, "the plan's primary GPT is not valid"))?;
        if parsed(last, backup_header, backup_entries).as_ref() != Some(&planned) {
            return Err(refuse(
                last,
                "the plan's backup GPT is not its primary's copy",
            ));
        }
        let partition = self.partition;
        let new = planned
            .partitions
            .iter()
            .zip(&planned.slots)
            .find(|(_, found)| **found as usize == slot)
            .map(|(new, _)| new)
            .filter(|new| {
                new.role() == Some(Role::AppleRegion)
                    && new.unique_guid == partition.guid
                    && new.first_lba == partition.first_lba
                    && new.blocks() == partition.blocks
                    && new.attributes == 0
            })
            .ok_or(refuse(
                2 + (slot * ENTRY_BYTES / block) as u64,
                "the entry the plan adds is not the planned Stambha partition",
            ))?;
        for (index, kept) in existing.partitions.iter().enumerate() {
            say(&format!(
                "PASS entry {} stays byte for byte: {:?}, type {}, LBA {}-{}",
                existing.slot(index) + 1,
                kept.name,
                kept.type_guid,
                kept.first_lba,
                kept.last_lba
            ));
        }
        say(&format!(
            "PASS entry {}, unused on the disk, takes the new partition: {:?}, type {}, LBA {}-{}",
            slot + 1,
            new.name,
            new.type_guid,
            new.first_lba,
            new.last_lba
        ));
        say(
            "PASS the primary and backup GPT headers change only their header and entries \
             checksums, bytes 16-19 and 88-91",
        );
        say(if keep_mbr {
            "PASS the protective MBR at LBA 0 is valid, and stays byte for byte"
        } else {
            "PASS the disk has no protective MBR, and the plan writes one at LBA 0"
        });
        Ok(planned)
    }

    /// Write `write`, make it durable and read it back, at `step`.
    #[expect(
        clippy::result_large_err,
        reason = "the driver's error is kept whole for the stop's message, and one plan is applied a boot"
    )]
    fn write_back(
        &self,
        device: &mut dyn BlockDevice,
        step: Step,
        write: &Write,
        what: &str,
        say: &mut dyn FnMut(&str),
    ) -> Result<(), ApplyError> {
        let failed = |cause| error(step, Some(write.lba), Cause::Device(cause));
        Request::Write {
            lba: write.lba,
            data: write.bytes.clone(),
            durable: false,
        }
        .run(device)
        .map_err(failed)?;
        device.flush().map_err(failed)?;
        let mut back = vec![0; write.bytes.len()];
        device.read(write.lba, &mut back).map_err(failed)?;
        if let Some(index) = first_difference(&back, &write.bytes, self.geometry.block_bytes) {
            return Err(error(step, Some(write.lba + index), Cause::ReadBack));
        }
        say(&format!(
            "PASS wrote {what}, {}, and read it back the same",
            self.span(
                write.lba,
                write.bytes.len() as u64 / self.geometry.block_bytes
            )
        ));
        Ok(())
    }

    /// Make the plan's change to `device`, the whole disk.
    #[expect(
        clippy::result_large_err,
        reason = "the driver's error is kept whole for the stop's message, and one plan is applied a boot"
    )]
    pub fn apply(&self, device: &mut dyn BlockDevice) -> Result<(), ApplyError> {
        self.apply_saying(device, &mut |_| {})
    }

    /// [`Self::apply`], saying each step as [`Self::check`] does.
    ///
    /// Nothing is written unless the plan installs and passes
    /// [`Self::check`]. Then a trial write in the new partition is read
    /// back, and the table's blocks go in [`shared::encode`]'s order, the
    /// backup copy before the primary, each made durable and read back
    /// before the next, and the backup read whole before the primary is
    /// touched: until the primary's entries are written the disk reads as
    /// its earlier table, and from then on as the new one, from the backup
    /// while the primary header is still the earlier one's
    /// ([`shared::read`]). So a write that fails or reads back otherwise,
    /// or power lost, leaves one whole table or the other, never a mix, and
    /// a write that does not read back stops before the next. Then the
    /// ranges are zeroed, in pieces of [`ZERO_BYTES`], everything is made
    /// durable, both copies of the table are read back, and the zeroed
    /// ranges are read back whole.
    #[expect(
        clippy::result_large_err,
        reason = "the driver's error is kept whole for the stop's message, and one plan is applied a boot"
    )]
    pub fn apply_saying(
        &self,
        device: &mut dyn BlockDevice,
        say: &mut dyn FnMut(&str),
    ) -> Result<(), ApplyError> {
        if self.mode == Mode::Check {
            return Err(error(Step::Check, None, Cause::CheckOnly));
        }
        let failed = |step, lba| move |cause| error(step, lba, Cause::Device(cause));
        let checked = self.check(&mut ReadOnly::new(device), say)?;
        let geometry = self.geometry;
        let block = geometry.block_bytes;
        if let Some(planned) = &checked.table {
            self.write_back(
                device,
                Step::Probe,
                &self.probe(),
                "the trial write in the new partition",
                say,
            )?;
            for (index, write) in self.table.iter().enumerate() {
                self.write_back(device, Step::Table, write, self.names(write.lba), say)?;
                if index != 1 {
                    continue;
                }
                // The backup copy is whole before the primary is touched.
                let back = read_ends(geometry, Step::Table, |lba, buffer| {
                    device.read(lba, buffer)
                })?;
                if backup_copy(geometry, &back.tail).as_ref() != Some(planned) {
                    return Err(error(
                        Step::Table,
                        Some(geometry.last_usable() + 1),
                        Cause::ReadBack,
                    ));
                }
                // And the backup's writes landed nowhere in the primary's
                // blocks, from LBA 0: they read as the picker read them.
                if let Some(lba) = first_difference(&back.head, &self.ends.head, block) {
                    return Err(error(Step::Table, Some(lba), Cause::Changed));
                }
                say(
                    "PASS the backup GPT on the disk is the new table, whole, and the primary is \
                     as it was",
                );
            }
        }
        let piece = (ZERO_BYTES / block).max(1);
        for extent in &self.zero {
            let mut done = 0;
            while done < extent.blocks {
                let lba = extent.lba + done;
                let blocks = (extent.blocks - done).min(piece);
                Request::WriteZeroes {
                    lba,
                    blocks,
                    unmap: false,
                }
                .run(device)
                .map_err(failed(Step::Zero, Some(lba)))?;
                done += blocks;
            }
        }
        device.flush().map_err(failed(Step::Flush, None))?;
        let verify = |lba, cause| error(Step::Verify, lba, cause);
        let table = gpt::read(device).map_err(|cause| verify(None, Cause::Table(cause)))?;
        if let Some(planned) = &checked.table {
            let now = read_ends(geometry, Step::Verify, |lba, buffer| {
                device.read(lba, buffer)
            })?;
            let (found, _) =
                copies(geometry, &now).map_err(|found| verify(found.lba, found.cause))?;
            if found != *planned || table != *planned {
                return Err(verify(Some(1), Cause::Missing));
            }
            say("PASS both GPT copies read back whole, alike and as planned");
        }
        let mut matching = table
            .partitions
            .iter()
            .filter(|partition| partition.unique_guid == self.partition.guid);
        let planned = matching.next().is_some_and(|partition| {
            partition.first_lba == self.partition.first_lba
                && partition.blocks() == self.partition.blocks
                && partition.role() == Some(Role::AppleRegion)
        });
        if !planned || matching.next().is_some() {
            return Err(verify(None, Cause::Missing));
        }
        let mut read = vec![0; (piece * block) as usize];
        for extent in &self.zero {
            let mut done = 0;
            while done < extent.blocks {
                let lba = extent.lba + done;
                let blocks = (extent.blocks - done).min(piece);
                let part = &mut read[..(blocks * block) as usize];
                device
                    .read(lba, part)
                    .map_err(failed(Step::Verify, Some(lba)))?;
                if let Some(index) = part
                    .chunks(block as usize)
                    .position(|found| found.iter().any(|&byte| byte != 0))
                {
                    return Err(verify(Some(lba + index as u64), Cause::NotZero));
                }
                done += blocks;
            }
            say(&format!(
                "PASS zeroed {} and read it back as zeros",
                self.span(extent.lba, extent.blocks)
            ));
        }
        Ok(())
    }

    /// The plan as bytes, for the memory the loader hands the engine.
    pub fn encode(&self) -> Vec<u8> {
        fn number(bytes: &mut Vec<u8>, value: u64) {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC);
        number(
            &mut bytes,
            match self.mode {
                Mode::Install => 0,
                Mode::Check => 1,
            },
        );
        number(&mut bytes, self.geometry.block_bytes);
        number(&mut bytes, self.geometry.blocks);
        bytes.extend_from_slice(&self.seen);
        bytes.extend_from_slice(&self.partition.guid.0);
        number(&mut bytes, self.partition.first_lba);
        number(&mut bytes, self.partition.blocks);
        number(&mut bytes, self.serial.len() as u64);
        bytes.extend_from_slice(self.serial.as_bytes());
        for end in [&self.ends.head, &self.ends.tail] {
            number(&mut bytes, end.len() as u64);
            bytes.extend_from_slice(end);
        }
        number(&mut bytes, self.zero.len() as u64);
        for extent in &self.zero {
            number(&mut bytes, extent.lba);
            number(&mut bytes, extent.blocks);
        }
        number(&mut bytes, self.table.len() as u64);
        for write in &self.table {
            number(&mut bytes, write.lba);
            number(&mut bytes, write.bytes.len() as u64);
            bytes.extend_from_slice(&write.bytes);
        }
        bytes
    }

    /// The plan [`Self::encode`] made `bytes` from; `None` when they are
    /// not one whole plan.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let mut rest = Rest(bytes);
        if rest.take(MAGIC.len() as u64)? != MAGIC {
            return None;
        }
        let mode = match rest.number()? {
            0 => Mode::Install,
            1 => Mode::Check,
            _ => return None,
        };
        let geometry = Disk {
            block_bytes: rest.number()?,
            blocks: rest.number()?,
        };
        let seen = rest.take(32)?.try_into().ok()?;
        let partition = Planned {
            guid: Guid(rest.take(16)?.try_into().ok()?),
            first_lba: rest.number()?,
            blocks: rest.number()?,
        };
        let serial_bytes = rest.number()?;
        let serial = String::from(core::str::from_utf8(rest.take(serial_bytes)?).ok()?);
        let ends = Ends {
            head: rest.counted()?.to_vec(),
            tail: rest.counted()?.to_vec(),
        };
        let mut zero = Vec::new();
        for _ in 0..rest.number()? {
            zero.push(Extent {
                lba: rest.number()?,
                blocks: rest.number()?,
            });
        }
        let mut table = Vec::new();
        for _ in 0..rest.number()? {
            let lba = rest.number()?;
            let length = rest.number()?;
            table.push(Write {
                lba,
                bytes: rest.take(length)?.to_vec(),
            });
        }
        rest.0.is_empty().then_some(Self {
            serial,
            geometry,
            seen,
            ends,
            mode,
            table,
            zero,
            partition,
        })
    }
}

/// The bytes of an encoded plan, or of the pending work around it
/// (`disk_writes`), not decoded yet.
pub(super) struct Rest<'a>(pub(super) &'a [u8]);

impl<'a> Rest<'a> {
    pub(super) fn take(&mut self, count: u64) -> Option<&'a [u8]> {
        let (taken, rest) = self.0.split_at_checked(usize::try_from(count).ok()?)?;
        self.0 = rest;
        Some(taken)
    }

    pub(super) fn number(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }

    /// A length and that many bytes.
    pub(super) fn counted(&mut self) -> Option<&'a [u8]> {
        let count = self.number()?;
        self.take(count)
    }
}

#[cfg(test)]
pub(super) mod tests;
