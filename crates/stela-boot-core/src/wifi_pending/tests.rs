use super::*;
use alloc::{format, string::String};
use stela_drivers::block::{Features, Identity, Transport};

/// Where a commit stops, as power going away would stop it.
#[derive(Clone, Copy, Debug)]
enum Cut {
    /// Every operation goes through.
    Never,
    /// The write with this index (from 0) is refused before it lands.
    Before(usize),
    /// The write with this index lands its first half and garbage over the
    /// rest, as a torn write leaves its blocks, then fails.
    Halfway(usize),
    /// The flush with this index (from 0) fails, after its writes landed.
    Flush(usize),
    /// Every write says it landed and none does, as a stick that drops
    /// writes behind a cache says.
    Lost,
}

/// The file's blocks in memory, cut where a test says.
struct Disk {
    identity: Identity,
    bytes: Vec<u8>,
    writes: usize,
    flushes: usize,
    cut: Cut,
}

impl Disk {
    fn new(block: u32) -> Self {
        Self {
            identity: Identity {
                transport: Transport::Firmware,
                location: String::from("wifi.pending"),
                model: String::new(),
                serial: String::new(),
                blocks: (FILE_BYTES / block as usize) as u64,
                block_size: block,
                read_only: false,
                features: Features::default(),
            },
            bytes: vec![0; FILE_BYTES],
            writes: 0,
            flushes: 0,
            cut: Cut::Never,
        }
    }

    fn span(&self, lba: u64, bytes: usize) -> core::ops::Range<usize> {
        let start = lba as usize * self.identity.block_size as usize;
        start..start + bytes
    }
}

impl BlockDevice for Disk {
    fn identity(&self) -> &Identity {
        &self.identity
    }

    fn read(&mut self, lba: u64, buffer: &mut [u8]) -> Result<(), BlockError> {
        self.identity.check(lba, buffer.len())?;
        buffer.copy_from_slice(&self.bytes[self.span(lba, buffer.len())]);
        Ok(())
    }

    fn write(&mut self, lba: u64, data: &[u8]) -> Result<(), BlockError> {
        self.identity.check_write(lba, data.len())?;
        let index = self.writes;
        self.writes += 1;
        let span = self.span(lba, data.len());
        match self.cut {
            Cut::Before(at) if at <= index => Err(BlockError::Unsupported("cut")),
            Cut::Halfway(at) if at == index => {
                let half = data.len() / 2;
                self.bytes[span.start..span.start + half].copy_from_slice(&data[..half]);
                self.bytes[span.start + half..span.end].fill(0xa5);
                Err(BlockError::Unsupported("cut"))
            }
            Cut::Halfway(at) if at < index => Err(BlockError::Unsupported("cut")),
            Cut::Lost => Ok(()),
            _ => {
                self.bytes[span].copy_from_slice(data);
                Ok(())
            }
        }
    }

    fn flush(&mut self) -> Result<(), BlockError> {
        let index = self.flushes;
        self.flushes += 1;
        match self.cut {
            Cut::Flush(at) if at <= index => Err(BlockError::Unsupported("cut")),
            _ => Ok(()),
        }
    }
}

fn lines(networks: &[(&str, &str)]) -> Vec<u8> {
    networks.iter().fold(Vec::new(), |pending, (name, secret)| {
        add(&pending, name.as_bytes(), secret.as_bytes())
    })
}

#[test]
fn a_never_used_file_holds_nothing_and_commits_read_back_on_both_block_sizes() {
    for block in [512, 4096] {
        let mut disk = Disk::new(block);
        let mut journal = Journal::open(&mut disk).unwrap();
        assert_eq!(journal.pending(), b"");
        assert_eq!(read_image(&disk.bytes).unwrap(), None);
        let big = vec![b'x'; MAX_PAYLOAD];
        for payload in [lines(&[("home", "hunter2hunter2")]), Vec::new(), big] {
            journal.commit(&mut disk, &payload).unwrap();
            assert_eq!(journal.pending(), payload.as_slice());
            assert_eq!(Journal::open(&mut disk).unwrap().pending(), payload);
            assert_eq!(read_image(&disk.bytes).unwrap(), Some(payload));
        }
        // Payload, then header, each flushed.
        assert_eq!((disk.writes, disk.flushes), (6, 6));
    }
}

#[test]
fn the_seeded_file_holds_an_empty_record_a_first_commit_cannot_lose() {
    let image = initial_image();
    assert_eq!(image.len(), FILE_BYTES);
    assert_eq!(read_image(&image).unwrap(), Some(Vec::new()));
    // Unseeded, a first commit torn halfway leaves a damaged file, which is
    // why the loader seeds it.
    let mut disk = Disk::new(512);
    disk.cut = Cut::Halfway(0);
    let mut journal = Journal::open(&mut disk).unwrap();
    assert!(
        journal
            .commit(
                &mut disk,
                b"home hunter2hunter2
"
            )
            .is_err()
    );
    assert_eq!(read_image(&disk.bytes), Err(Error::Corrupt));
    // Seeded, the same cut leaves the empty record.
    let mut disk = Disk::new(512);
    disk.bytes = initial_image();
    disk.cut = Cut::Halfway(0);
    let mut journal = Journal::open(&mut disk).unwrap();
    assert!(
        journal
            .commit(
                &mut disk,
                b"home hunter2hunter2
"
            )
            .is_err()
    );
    assert_eq!(read_image(&disk.bytes).unwrap(), Some(Vec::new()));
}

#[test]
fn commits_alternate_slots_and_the_newest_sequence_wins() {
    let mut disk = Disk::new(512);
    let mut journal = Journal::open(&mut disk).unwrap();
    journal.commit(&mut disk, b"one\n").unwrap();
    assert_eq!(&disk.bytes[..8], MAGIC);
    assert!(disk.bytes[SLOT_BYTES..].iter().all(|&byte| byte == 0));
    journal.commit(&mut disk, b"two\n").unwrap();
    assert_eq!(&disk.bytes[SLOT_BYTES..SLOT_BYTES + 8], MAGIC);
    journal.commit(&mut disk, b"three\n").unwrap();
    // The third commit went back to slot 0; slot 1 still holds "two".
    assert_eq!(
        &disk.bytes[SLOT_BYTES + HEADER_BYTES..SLOT_BYTES + HEADER_BYTES + 4],
        b"two\n"
    );
    assert_eq!(read_image(&disk.bytes).unwrap(), Some(b"three\n".to_vec()));
}

/// The heart of the safety argument: a commit cut at any write or flush,
/// before a write or halfway through one, leaves the old record or the new
/// one, never a damaged file and never anything else.
#[test]
fn a_commit_cut_anywhere_leaves_the_old_record_or_the_new_one() {
    let old = lines(&[("home", "hunter2hunter2")]);
    // Longer than half of the largest block, and sharing no first bytes with
    // `old`, so a torn write tears what either holds.
    let names: Vec<String> = (0..150).map(|n| format!("net-{n:03}")).collect();
    let networks: Vec<(&str, &str)> = names
        .iter()
        .map(|name| (name.as_str(), "password-1"))
        .collect();
    let new = lines(&networks);
    assert!(new.len() > 4096 / 2 && new.len() <= MAX_PAYLOAD);
    for block in [512, 4096] {
        // The loader seeds a new file with an empty record
        // ([`initial_image`]), so a first commit has an old slot too.
        for previous in [Vec::new(), old.clone()] {
            let mut cuts = vec![];
            for at in 0..3 {
                cuts.extend([Cut::Before(at), Cut::Halfway(at), Cut::Flush(at)]);
            }
            for cut in cuts {
                let mut disk = Disk::new(block);
                disk.bytes = initial_image();
                if !previous.is_empty() {
                    Journal::open(&mut disk)
                        .unwrap()
                        .commit(&mut disk, &previous)
                        .unwrap();
                }
                let (writes, flushes) = (disk.writes, disk.flushes);
                disk.cut = match cut {
                    Cut::Before(at) => Cut::Before(writes + at),
                    Cut::Halfway(at) => Cut::Halfway(writes + at),
                    Cut::Flush(at) => Cut::Flush(flushes + at),
                    cut => cut,
                };
                let mut journal = Journal::open(&mut disk).unwrap();
                let committed = journal.commit(&mut disk, &new);
                let read = read_image(&disk.bytes);
                let read = read.unwrap_or_else(|error| panic!("{cut:?}: {error}"));
                let before = Some(previous.clone());
                assert!(
                    read == before || read.as_ref() == Some(&new),
                    "{cut:?} after {previous:?} left {read:?}"
                );
                if committed.is_ok() {
                    assert_eq!(read.as_ref(), Some(&new), "{cut:?}");
                } else {
                    // A failed commit asks for a reopen, which finds what
                    // the medium holds.
                    assert_eq!(journal.commit(&mut disk, &new), Err(Error::NeedsReopen));
                    disk.cut = Cut::Never;
                    let reopened = Journal::open(&mut disk).unwrap();
                    assert_eq!(Some(reopened.pending().to_vec()), read);
                }
            }
        }
    }
}

#[test]
fn a_commit_is_acknowledged_only_once_it_reads_back() {
    let mut disk = Disk::new(512);
    disk.bytes = initial_image();
    let mut journal = Journal::open(&mut disk).unwrap();
    journal
        .commit(
            &mut disk, b"older
",
        )
        .unwrap();
    journal
        .commit(
            &mut disk, b"newer
",
        )
        .unwrap();
    // The next commit goes to the slot holding "older", which a stick that
    // drops writes still holds: it reads back, but not as written.
    disk.cut = Cut::Lost;
    assert_eq!(
        journal.commit(
            &mut disk, b"newest
"
        ),
        Err(Error::Readback)
    );
    assert_eq!(
        journal.commit(
            &mut disk, b"newest
"
        ),
        Err(Error::NeedsReopen)
    );
    disk.cut = Cut::Never;
    assert_eq!(
        Journal::open(&mut disk).unwrap().pending(),
        b"newer
"
    );
}

#[test]
fn damage_falls_back_to_the_older_slot_but_never_reads_as_empty() {
    let mut disk = Disk::new(512);
    let mut journal = Journal::open(&mut disk).unwrap();
    journal.commit(&mut disk, b"older\n").unwrap();
    journal.commit(&mut disk, b"newer\n").unwrap();
    let valid = disk.bytes.clone();
    // Every header field, the reserved bytes, both digests and the payload.
    for byte in [0, 8, 12, 16, 24, 32, 64, 96, HEADER_BYTES - 1, HEADER_BYTES] {
        let mut damaged = valid.clone();
        damaged[SLOT_BYTES + byte] ^= 1;
        assert_eq!(read_image(&damaged).unwrap(), Some(b"older\n".to_vec()));
        damaged[byte] ^= 1;
        assert_eq!(read_image(&damaged), Err(Error::Corrupt));
    }
    let mut stray = vec![0; FILE_BYTES];
    stray[FILE_BYTES - 1] = 1;
    assert_eq!(read_image(&stray), Err(Error::Corrupt));
    // Two slots of one sequence are not told apart.
    let mut twins = valid;
    let first = twins[..SLOT_BYTES].to_vec();
    twins[SLOT_BYTES..].copy_from_slice(&first);
    assert_eq!(read_image(&twins), Err(Error::Corrupt));
    // A damaged file is not written over by a journal: it does not open.
    disk.bytes = stray;
    assert!(matches!(Journal::open(&mut disk), Err(Error::Corrupt)));
}

#[test]
fn a_valid_checksum_does_not_make_a_zero_sequence_or_oversized_record() {
    for (sequence, length) in [(0_u64, 4_u64), (1, MAX_PAYLOAD as u64 + 1)] {
        let mut image = vec![0; FILE_BYTES];
        image[..8].copy_from_slice(MAGIC);
        image[8..12].copy_from_slice(&VERSION.to_le_bytes());
        image[12..16].copy_from_slice(&(HEADER_BYTES as u32).to_le_bytes());
        image[16..24].copy_from_slice(&sequence.to_le_bytes());
        image[24..32].copy_from_slice(&length.to_le_bytes());
        let digest = header_hash(&image[..HEADER_BYTES]);
        image[HEADER_DIGEST..HEADER_RECORD].copy_from_slice(&digest);
        assert_eq!(read_image(&image), Err(Error::Corrupt));
    }
}

#[test]
fn refusals_write_nothing() {
    let mut disk = Disk::new(512);
    let mut journal = Journal::open(&mut disk).unwrap();
    journal.commit(&mut disk, b"kept\n").unwrap();
    let before = disk.bytes.clone();
    assert_eq!(
        journal.commit(&mut disk, &vec![0; MAX_PAYLOAD + 1]),
        Err(Error::PayloadSize {
            bytes: MAX_PAYLOAD + 1
        })
    );
    disk.identity.read_only = true;
    assert_eq!(
        journal.commit(&mut disk, b"x"),
        Err(Error::Device(BlockError::ReadOnly))
    );
    disk.identity.read_only = false;
    // Another session committed since this one opened.
    Journal::open(&mut disk)
        .unwrap()
        .commit(&mut disk, b"other\n")
        .unwrap();
    let other = disk.bytes.clone();
    assert_eq!(journal.commit(&mut disk, b"stale\n"), Err(Error::Changed));
    assert_eq!(disk.bytes, other);
    assert_ne!(disk.bytes, before);
    // A view that is not exactly the file, or of blocks it does not take.
    disk.identity.blocks += 1;
    assert!(matches!(
        Journal::open(&mut disk),
        Err(Error::DeviceSize { .. })
    ));
    disk.identity.blocks -= 2;
    assert!(matches!(
        Journal::open(&mut disk),
        Err(Error::DeviceSize { .. })
    ));
    for size in [0, 1024, 8192] {
        disk.identity.block_size = size;
        assert!(matches!(Journal::open(&mut disk), Err(Error::BlockSize(s)) if s == size));
    }
    assert_eq!(read_image(&[0; 512]), Err(Error::ImageSize { bytes: 512 }));
}

#[test]
fn an_exhausted_sequence_refuses_without_touching_the_medium() {
    let mut disk = Disk::new(512);
    disk.bytes[..HEADER_BYTES].copy_from_slice(&encode_header(u64::MAX, b"last"));
    disk.bytes[HEADER_BYTES..HEADER_BYTES + 4].copy_from_slice(b"last");
    let before = disk.bytes.clone();
    let mut journal = Journal::open(&mut disk).unwrap();
    assert_eq!(
        journal.commit(&mut disk, b"next"),
        Err(Error::SequenceExhausted)
    );
    assert_eq!(disk.bytes, before);
}

#[test]
fn adding_a_network_again_replaces_its_line() {
    let pending = lines(&[("home", "first-pass"), ("cafe", "coffee-time")]);
    let pending = add(&pending, b"home", b"second-pass");
    assert_eq!(pending, b"cafe coffee-time\nhome second-pass\n".to_vec());
    // Names and secrets are written as wifi.saved writes them.
    assert_eq!(
        add(b"", b"Mom's phone", b"pass word%12"),
        b"Mom's%20phone pass%20word%2512\n".to_vec()
    );
}

#[test]
fn folding_replaces_saved_lines_of_the_same_name_and_keeps_the_rest() {
    let saved = b"# mine\nhome old-secret\ncafe coffee-time\n";
    let pending = lines(&[("home", "new-secret1"), ("lab", "lab-secret")]);
    let folded = fold(saved, &pending);
    assert_eq!(
        folded,
        b"# mine\ncafe coffee-time\nhome new-secret1\nlab lab-secret\n".to_vec()
    );
    let parsed = wifi_saved::parse(&folded);
    assert_eq!(parsed.unusable, 0);
    assert_eq!(parsed.networks.len(), 3);
    // Nothing pending changes nothing; a line with no name is not folded.
    assert_eq!(fold(saved, b""), saved.to_vec());
    assert_eq!(fold(saved, b"\n# a note\n"), saved.to_vec());
}

#[test]
fn folding_twice_is_folding_once() {
    let saved = b"home old-secret\r\n\ncafe coffee-time\n";
    let pending = lines(&[("cafe", "new-coffee1"), ("home", "new-home11")]);
    let once = fold(saved, &pending);
    assert_eq!(fold(&once, &pending), once);
    assert_eq!(fold(b"", &pending), pending);
}

#[test]
fn nothing_secret_reaches_debug_or_the_errors() {
    let mut disk = Disk::new(512);
    let mut journal = Journal::open(&mut disk).unwrap();
    journal
        .commit(&mut disk, &lines(&[("home", "hunter2hunter2")]))
        .unwrap();
    let shown = format!("{journal:?}");
    assert!(
        !shown.contains("hunter2") && !shown.contains("home"),
        "{shown}"
    );
    for error in [
        Error::Corrupt,
        Error::Changed,
        Error::Readback,
        Error::PayloadSize { bytes: 1 },
    ] {
        assert!(
            format!("{error}").starts_with("wifi.pending")
                || format!("{error}").contains("wifi.pending")
        );
    }
}
