//! Networks the Wi-Fi picker joined after the firmware exited, waiting to
//! be saved: `\EFI\stela\wifi.pending` ([`FILE`]), a file of exactly
//! [`FILE_BYTES`] the loader preallocates on the USB. With no firmware file
//! system left, the system engine writes only the blocks of its own
//! clusters, as it writes the boot log, so the volume's FAT and directories
//! are never written after the firmware exits. The next boot's loader folds
//! what it holds into `wifi.saved` ([`fold`]) before the startup menu reads
//! that, then empties it.
//!
//! The file is two slots of [`SLOT_BYTES`], each a [`HEADER_BYTES`] header
//! and up to [`MAX_PAYLOAD`] of `wifi.saved` lines (`wifi_saved::line`). A
//! commit writes the slot that is not the newest, its payload first, then
//! its header, flushing after each, and reads it back: a commit cut short
//! anywhere leaves the newest slot as it was, and the new one either whole
//! or failing its checksum, so a read finds the old record or the new one,
//! never a mix. The header, little endian:
//!
//! | Offset | Bytes | Field |
//! | --- | --- | --- |
//! | 0 | 8 | magic `STLWIFIP` |
//! | 8 | 4 | version, 1 |
//! | 12 | 4 | header bytes, 4096 |
//! | 16 | 8 | sequence: the newest slot's is the larger, never 0 |
//! | 24 | 8 | payload bytes, 0 to [`MAX_PAYLOAD`] |
//! | 32 | 32 | SHA-256 of the payload |
//! | 64 | 32 | SHA-256 of the header with this field zeroed |
//! | 96 | rest | zero |
//!
//! An empty payload is "nothing pending". A file of zeros was never used; a
//! file that is not zeros and holds no valid slot, or two slots of one
//! sequence, is damaged, and is never taken for an empty one. The payload
//! holds passphrases as `wifi.saved` does, as plain text on the USB; the
//! copies read from it are wiped once dropped.
use crate::wifi_saved;
use alloc::{vec, vec::Vec};
use core::fmt;
use stela_crypto::Sha256;
use stela_crypto::digest::Digest;
use stela_drivers::block::{BlockDevice, BlockError};

/// The file, beside `wifi.saved` on the USB's EFI partition.
pub const FILE: &str = "\\EFI\\stela\\wifi.pending";
pub const FILE_BYTES: usize = 32 << 10;
pub const SLOT_BYTES: usize = FILE_BYTES / 2;
pub const HEADER_BYTES: usize = 4096;
/// About a hundred networks' lines.
pub const MAX_PAYLOAD: usize = SLOT_BYTES - HEADER_BYTES;
const MAGIC: &[u8; 8] = b"STLWIFIP";
const VERSION: u32 = 1;
const HEADER_DIGEST: usize = 64;
const HEADER_RECORD: usize = HEADER_DIGEST + 32;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    ImageSize {
        bytes: usize,
    },
    BlockSize(u32),
    DeviceSize {
        blocks: u64,
        block_size: u32,
    },
    PayloadSize {
        bytes: usize,
    },
    /// Not zeros, with no valid slot or two of one sequence.
    Corrupt,
    SequenceExhausted,
    /// A commit failed after it may have reached the medium: open again.
    NeedsReopen,
    /// The medium no longer holds what this journal opened.
    Changed,
    Readback,
    Device(BlockError),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ImageSize { bytes } => {
                write!(f, "wifi.pending has {bytes} bytes; it needs {FILE_BYTES}")
            }
            Self::BlockSize(size) => write!(f, "wifi.pending does not take {size}-byte blocks"),
            Self::DeviceSize { blocks, block_size } => write!(
                f,
                "wifi.pending's view has {blocks} blocks of {block_size} bytes; it needs {FILE_BYTES} bytes"
            ),
            Self::PayloadSize { bytes } => {
                write!(
                    f,
                    "{bytes} bytes of networks do not fit wifi.pending's {MAX_PAYLOAD}"
                )
            }
            Self::Corrupt => f.write_str("wifi.pending is damaged"),
            Self::SequenceExhausted => f.write_str("wifi.pending's sequence is exhausted"),
            Self::NeedsReopen => {
                f.write_str("wifi.pending must be opened again after a failed write")
            }
            Self::Changed => f.write_str("wifi.pending changed since it was opened"),
            Self::Readback => f.write_str("wifi.pending did not read back as written"),
            Self::Device(error) => write!(f, "wifi.pending's device: {error}"),
        }
    }
}

impl core::error::Error for Error {}

impl From<BlockError> for Error {
    fn from(error: BlockError) -> Self {
        Self::Device(error)
    }
}

/// One valid slot. Its payload holds passphrases: wiped when dropped.
#[derive(PartialEq, Eq)]
struct Record {
    slot: usize,
    sequence: u64,
    payload: Vec<u8>,
}

impl fmt::Debug for Record {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Record")
            .field("slot", &self.slot)
            .field("sequence", &self.sequence)
            .field("payload", &self.payload.len())
            .finish()
    }
}

impl Drop for Record {
    fn drop(&mut self) {
        wifi_saved::wipe(&mut self.payload);
    }
}

/// One session over the file's blocks, which the caller gives as a device
/// of exactly [`FILE_BYTES`]. After a failed commit it must be opened again:
/// the new record may have reached the medium without its readback.
#[derive(Debug)]
pub struct Journal {
    record: Option<Record>,
    needs_reopen: bool,
}

impl Journal {
    pub fn open(device: &mut dyn BlockDevice) -> Result<Self, Error> {
        Ok(Self {
            record: read_device(device)?,
            needs_reopen: false,
        })
    }

    /// The pending lines; empty when none, or never written.
    pub fn pending(&self) -> &[u8] {
        self.record
            .as_ref()
            .map_or(&[][..], |record| record.payload.as_slice())
    }

    /// Make `bytes` the pending lines, empty for none: `Ok` only once both
    /// flushes are done and the slot read back as written.
    pub fn commit(&mut self, device: &mut dyn BlockDevice, bytes: &[u8]) -> Result<(), Error> {
        if self.needs_reopen {
            return Err(Error::NeedsReopen);
        }
        if bytes.len() > MAX_PAYLOAD {
            return Err(Error::PayloadSize { bytes: bytes.len() });
        }
        let block = geometry(device)?;
        if device.identity().read_only {
            return Err(BlockError::ReadOnly.into());
        }
        let sequence = self
            .record
            .as_ref()
            .map_or(Some(1), |record| record.sequence.checked_add(1))
            .ok_or(Error::SequenceExhausted)?;
        // A stale session or a file changed under it writes nothing.
        if read_device(device)? != self.record {
            return Err(Error::Changed);
        }
        let slot = self.record.as_ref().map_or(0, |record| 1 - record.slot);
        let offset = slot * SLOT_BYTES;
        let mut payload = vec![0; bytes.len().div_ceil(block).max(1) * block];
        payload[..bytes.len()].copy_from_slice(bytes);
        let header = encode_header(sequence, bytes);
        // Even a failed first write may have changed the medium.
        self.needs_reopen = true;
        let written = (|| {
            device.write(((offset + HEADER_BYTES) / block) as u64, &payload)?;
            device.flush()?;
            device.write((offset / block) as u64, &header)?;
            device.flush()
        })();
        wifi_saved::wipe(&mut payload);
        written?;
        let mut readback = vec![0; SLOT_BYTES];
        let read = device.read((offset / block) as u64, &mut readback);
        let record = read.map(|()| decode_slot(slot, &readback));
        wifi_saved::wipe(&mut readback);
        let record = record?.ok_or(Error::Readback)?;
        if record.sequence != sequence || record.payload != bytes {
            return Err(Error::Readback);
        }
        self.record = Some(record);
        self.needs_reopen = false;
        Ok(())
    }
}

/// The pending lines in the file's bytes as the firmware's file system
/// reads them: `None` for a file never written.
pub fn read_image(bytes: &[u8]) -> Result<Option<Vec<u8>>, Error> {
    Ok(select(bytes)?.map(|record| record.payload.clone()))
}

/// The file as the loader first writes it, through the firmware's file
/// system: an empty record in slot 0, so the system engine's first commit,
/// which goes to slot 1, always has an older slot to fall back on.
pub fn initial_image() -> Vec<u8> {
    let mut image = vec![0; FILE_BYTES];
    image[..HEADER_BYTES].copy_from_slice(&encode_header(1, b""));
    image
}

/// `pending` with the network `ssid` saved with `secret` in place of any
/// line of the same name.
pub fn add(pending: &[u8], ssid: &[u8], secret: &[u8]) -> Vec<u8> {
    let mut updated = wifi_saved::without(pending, ssid);
    let mut line = wifi_saved::line(ssid, secret);
    updated.extend_from_slice(&line);
    wifi_saved::wipe(&mut line);
    updated
}

/// `saved` (`wifi.saved`'s bytes) with each of `pending`'s networks in place
/// of any line of the same name; every other line, comments included, as
/// it was. Folding the same lines twice gives what folding them once gave.
pub fn fold(saved: &[u8], pending: &[u8]) -> Vec<u8> {
    let mut folded = saved.to_vec();
    for line in pending.split(|&byte| byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let Some(name) = wifi_saved::names(line).into_iter().next() else {
            continue;
        };
        let mut kept = wifi_saved::without(&folded, &name);
        wifi_saved::wipe(&mut folded);
        kept.extend_from_slice(line);
        kept.push(b'\n');
        folded = kept;
    }
    folded
}

fn geometry(device: &dyn BlockDevice) -> Result<usize, Error> {
    let identity = device.identity();
    if !matches!(identity.block_size, 512 | 4096) {
        return Err(Error::BlockSize(identity.block_size));
    }
    let block = identity.block_size as usize;
    if identity.blocks != (FILE_BYTES / block) as u64 {
        return Err(Error::DeviceSize {
            blocks: identity.blocks,
            block_size: identity.block_size,
        });
    }
    Ok(block)
}

fn read_device(device: &mut dyn BlockDevice) -> Result<Option<Record>, Error> {
    geometry(device)?;
    let mut image = vec![0; FILE_BYTES];
    let read = device.read(0, &mut image).map_err(Error::from);
    let selected = read.and_then(|()| select(&image));
    wifi_saved::wipe(&mut image);
    selected
}

fn select(bytes: &[u8]) -> Result<Option<Record>, Error> {
    if bytes.len() != FILE_BYTES {
        return Err(Error::ImageSize { bytes: bytes.len() });
    }
    match (
        decode_slot(0, &bytes[..SLOT_BYTES]),
        decode_slot(1, &bytes[SLOT_BYTES..]),
    ) {
        (Some(a), Some(b)) if a.sequence == b.sequence => Err(Error::Corrupt),
        (Some(a), Some(b)) => Ok(Some(if a.sequence > b.sequence { a } else { b })),
        (Some(record), None) | (None, Some(record)) => Ok(Some(record)),
        (None, None) if bytes.iter().all(|&byte| byte == 0) => Ok(None),
        (None, None) => Err(Error::Corrupt),
    }
}

fn encode_header(sequence: u64, payload: &[u8]) -> [u8; HEADER_BYTES] {
    let mut header = [0; HEADER_BYTES];
    header[..8].copy_from_slice(MAGIC);
    header[8..12].copy_from_slice(&VERSION.to_le_bytes());
    header[12..16].copy_from_slice(&(HEADER_BYTES as u32).to_le_bytes());
    header[16..24].copy_from_slice(&sequence.to_le_bytes());
    header[24..32].copy_from_slice(&(payload.len() as u64).to_le_bytes());
    header[32..64].copy_from_slice(&Sha256::digest(payload));
    let digest = header_hash(&header);
    header[HEADER_DIGEST..HEADER_RECORD].copy_from_slice(&digest);
    header
}

fn header_hash(header: &[u8]) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(&header[..HEADER_DIGEST]);
    hash.update([0; 32]);
    hash.update(&header[HEADER_RECORD..]);
    hash.finalize().into()
}

fn decode_slot(slot: usize, bytes: &[u8]) -> Option<Record> {
    let header = &bytes[..HEADER_BYTES];
    if &header[..8] != MAGIC
        || header[8..12] != VERSION.to_le_bytes()
        || header[12..16] != (HEADER_BYTES as u32).to_le_bytes()
        || header[HEADER_RECORD..].iter().any(|&byte| byte != 0)
        || header_hash(header) != header[HEADER_DIGEST..HEADER_RECORD]
    {
        return None;
    }
    let sequence = u64::from_le_bytes(header[16..24].try_into().ok()?);
    let length = u64::from_le_bytes(header[24..32].try_into().ok()?);
    if sequence == 0 || length > MAX_PAYLOAD as u64 {
        return None;
    }
    let payload = &bytes[HEADER_BYTES..HEADER_BYTES + length as usize];
    if Sha256::digest(payload)[..] != header[32..64] {
        return None;
    }
    Some(Record {
        slot,
        sequence,
        payload: payload.to_vec(),
    })
}

#[cfg(test)]
mod tests;
