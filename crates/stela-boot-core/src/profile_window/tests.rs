use super::*;
use stela_engine::machine::engine::step_timing::{HOT_BLOCKS, PcHit};

fn hit(pc: u64, level: Level, page: Option<u64>, cycles: u64) -> PcHit {
    PcHit {
        pc,
        level,
        page,
        offset: (pc & 0xfff) as u16,
        cycles,
    }
}

/// While the summary has room every weight is exact; once full, a new key
/// replaces the lightest and takes its weight as error, and the heavy keys
/// stay with their exact weights.
#[test]
fn space_saving_keeps_the_heavy_keys_exactly() {
    let mut summary = HeavyHitters::new(4);
    for (key, weight) in [(1u64, 100), (2, 50), (3, 10), (4, 5)] {
        summary.add(key, weight, 1);
    }
    assert!(summary.exact());
    assert_eq!(summary.floor(), 0);
    assert_eq!(summary.top_weight(2), 150);
    // A fifth key replaces key 4 (weight 5).
    summary.add(5, 1, 1);
    assert!(!summary.exact());
    assert_eq!(summary.evicted, 1);
    let top = summary.top(5);
    assert_eq!(top.len(), 4);
    assert_eq!(
        top[0],
        (
            1,
            Slot {
                weight: 100,
                error: 0,
                aux: 1
            }
        )
    );
    assert_eq!(top[1].0, 2);
    // Key 5 holds 5 + 1 with an error of 5: certainly 1.
    let five = top.iter().find(|(key, _)| *key == 5).unwrap().1;
    assert_eq!((five.weight, five.error), (6, 5));
    assert_eq!(summary.floor(), 6);
    assert_eq!(summary.total, 166);
    // The heaviest two are still exact.
    assert_eq!(summary.top_weight(2), 150);
    // A held key grows in place.
    summary.add(3, 200, 1);
    assert_eq!(
        summary.top(1)[0],
        (
            3,
            Slot {
                weight: 210,
                error: 0,
                aux: 2
            }
        )
    );
}

/// A stream with a few heavy keys and many light ones: the summary's top
/// keys are the true ones, with their true weights.
#[test]
fn heavy_keys_survive_a_long_light_tail() {
    let mut summary = HeavyHitters::new(64);
    for round in 0..50u64 {
        for heavy in 0..8u64 {
            summary.add(heavy, 10, 0);
        }
        for light in 0..200u64 {
            summary.add(1000 + round * 200 + light, 1, 0);
        }
    }
    let top = summary.top(8);
    let mut keys: Vec<u64> = top.iter().map(|(key, _)| *key).collect();
    keys.sort_unstable();
    assert_eq!(keys, (0..8).collect::<Vec<_>>());
    // Each heavy key's certain weight is within its error of the truth.
    for (_, slot) in &top {
        assert!(
            slot.weight >= 500 && slot.weight - slot.error <= 500,
            "{slot:?}"
        );
    }
}

/// Pages sum every block of a beat, the table's whole, at the page's PC
/// and key; the lines are the `PES PC` lines' form at offset 0.
#[test]
fn pages_sum_every_block_by_page() {
    let mut beat = PcSamples::default();
    beat.add(hit(0x1_8000_0040, Level::User, Some(0xaa), 30));
    beat.add(hit(0x1_8000_0400, Level::User, Some(0xaa), 20));
    beat.add(hit(0x1_8000_1010, Level::User, Some(0xbb), 5));
    beat.add(hit(0x2c4d0, Level::Kernel, None, 7));
    beat.add_gone(1, 9);
    let mut pages = PcPages::default();
    pages.add(&beat);
    pages.add(&beat);
    assert_eq!(pages.len(), 3);
    assert_eq!(pages.samples, [6, 2, 0]);
    assert_eq!(pages.gone, [2, 18]);
    let lines = pages.lines("PES PC PAGES", 3, 1);
    assert_eq!(
        lines,
        [
            "PES PC PAGES samples=8 cycles=124 gone=2/18 other=0/0 w=3 pages=3",
            "PES PC PAGES user samples=6 cycles=110 pages=2 part=1/2 \
             0x180000000:4:100@00000000000000aa+0",
            "PES PC PAGES user samples=6 cycles=110 pages=2 part=2/2 \
             0x180001000:2:10@00000000000000bb+0",
            "PES PC PAGES kernel samples=2 cycles=14 pages=1 part=1/1 v+0x2c000:2:14@?+0",
        ]
    );
    assert!(PcPages::default().lines("PES PC PAGES", 0, 8).is_empty());
}

#[test]
fn pc_shares_rank_blocks_by_cycles() {
    let mut beat = PcSamples::default();
    beat.add(hit(0x1000, Level::User, Some(1), 600));
    beat.add(hit(0x2000, Level::User, Some(2), 300));
    beat.add(hit(0x3000, Level::User, Some(3), 100));
    let mut shares = PcShares::new(16);
    shares.add(&beat);
    let lines = shares.lines("PES PC SHARE", Some(2));
    assert_eq!(
        lines,
        [
            "PES PC SHARE user w=2 samples=3 cycles=1000 blocks=3 exact=yes evicted=0 floor=0 \
          top24=1000 top100=1000 top1000=1000 top10000=1000"
        ]
    );
}

/// A drained table's blocks, all of them, whatever the engine's table
/// size: more blocks than one table holds, drained in parts, are counted
/// whole.
#[test]
fn hot_counts_take_more_blocks_than_one_table_holds() {
    let mut counts = HotCounts::default();
    let blocks = HOT_BLOCKS as u64 * 2;
    for part in 0..4u64 {
        let mut table = HotBlocks::default();
        for pc in (part * blocks / 4)..((part + 1) * blocks / 4) {
            table.add(pc * 4, Level::Kernel, 1 + u64::from(pc == 7));
        }
        assert_eq!(table.other, 0);
        counts.absorb(&table);
    }
    assert_eq!(counts.samples[Level::Kernel as usize], blocks + 1);
    assert_eq!(counts.other, 0);
    assert_eq!(counts.top(Level::Kernel, 1)[0].pc, 28);
    assert_eq!(counts.top(Level::Kernel, usize::MAX).len(), blocks as usize);
    let mut shares = HotShares::new(1 << 16);
    shares.add(&counts);
    assert_eq!(shares.top(Level::Kernel, 1)[0].count, 2);
    let line = shares.line("PES HOT SHARE", Level::Kernel, None).unwrap();
    assert!(
        line.starts_with("PES HOT SHARE kernel samples=16385 other=0 blocks=16384 exact=yes"),
        "{line}"
    );
    assert!(shares.line("PES HOT SHARE", Level::User, None).is_none());
}

#[test]
fn the_mark_is_control_option_shift_m_and_its_m_stays_from_the_guest() {
    let mut keys = MarkKeys::new();
    // M alone, and with fewer modifiers, reaches the guest.
    assert_eq!(keys.key(USAGE_M, true), KeyAction::Deliver);
    assert_eq!(keys.key(USAGE_M, false), KeyAction::Deliver);
    assert_eq!(keys.key(0xe0, true), KeyAction::Deliver);
    assert_eq!(keys.key(0xe1, true), KeyAction::Deliver);
    assert_eq!(keys.key(USAGE_M, true), KeyAction::Deliver);
    assert_eq!(keys.key(USAGE_M, false), KeyAction::Deliver);
    // Right Alt completes it.
    assert_eq!(keys.key(0xe6, true), KeyAction::Deliver);
    assert_eq!(keys.key(USAGE_M, true), KeyAction::Mark);
    // Its release stays from the guest even after a modifier's.
    assert_eq!(keys.key(0xe6, false), KeyAction::Deliver);
    assert_eq!(keys.key(USAGE_M, false), KeyAction::Swallow);
    assert_eq!(keys.key(USAGE_M, false), KeyAction::Deliver);
    // With Command held it is the guest's.
    assert_eq!(keys.key(0xe2, true), KeyAction::Deliver);
    assert_eq!(keys.key(0xe3, true), KeyAction::Deliver);
    assert_eq!(keys.key(USAGE_M, true), KeyAction::Deliver);
    assert_eq!(keys.key(0xe3, false), KeyAction::Deliver);
    assert_eq!(keys.key(USAGE_M, true), KeyAction::Mark);
    // Other keys always reach it.
    assert_eq!(keys.key(0x04, true), KeyAction::Deliver);
}

#[test]
fn a_dyld_header_names_its_uuid_and_unslid_base() {
    let mut header = [0u8; 0x200];
    header[..15].copy_from_slice(b"dyld_v1  arm64e");
    header[16..20].copy_from_slice(&0x1a0u32.to_le_bytes());
    for (at, byte) in header[88..104].iter_mut().enumerate() {
        *byte = at as u8 + 1;
    }
    header[0x1a0..0x1a8].copy_from_slice(&0x1_8000_0000u64.to_le_bytes());
    let parsed = dyld_header(&header).unwrap();
    assert_eq!(parsed.unslid, 0x1_8000_0000);
    assert_eq!(magic_text(&parsed.magic), "dyld_v1  arm64e");
    assert_eq!(
        uuid_text(&parsed.uuid),
        "01020304-0506-0708-090A-0B0C0D0E0F10"
    );
    assert!(dyld_header(&[0; 0x200]).is_none());
    // A mapping past the bytes read is no header.
    header[16..20].copy_from_slice(&0x400u32.to_le_bytes());
    assert!(dyld_header(&header).is_none());
}

/// A kernel collection's header and load commands: `__TEXT`, its UUID and
/// the kernel's fileset entry.
#[test]
fn a_kernel_collection_names_its_text_uuid_and_kernel() {
    let mut header = Vec::new();
    for word in [0xfeed_facfu32, 0x0100_000c, 2, MH_FILESET, 3, 0, 0, 0] {
        header.extend_from_slice(&word.to_le_bytes());
    }
    let mut commands = Vec::new();
    // LC_SEGMENT_64 __TEXT at 0xfffffe0007004000 (72 bytes, no sections).
    commands.extend_from_slice(&LC_SEGMENT_64.to_le_bytes());
    commands.extend_from_slice(&72u32.to_le_bytes());
    let mut name = [0u8; 16];
    name[..6].copy_from_slice(b"__TEXT");
    commands.extend_from_slice(&name);
    commands.extend_from_slice(&0xffff_fe00_0700_4000u64.to_le_bytes());
    commands.extend_from_slice(&[0; 72 - 32]);
    // LC_UUID.
    commands.extend_from_slice(&LC_UUID.to_le_bytes());
    commands.extend_from_slice(&24u32.to_le_bytes());
    commands.extend_from_slice(&[0xab; 16]);
    // LC_FILESET_ENTRY com.apple.kernel at 0xfffffe0007008000.
    let entry_name = b"com.apple.kernel\0\0\0\0\0\0\0\0";
    commands.extend_from_slice(&LC_FILESET_ENTRY.to_le_bytes());
    commands.extend_from_slice(&(32 + entry_name.len() as u32).to_le_bytes());
    commands.extend_from_slice(&0xffff_fe00_0700_8000u64.to_le_bytes());
    commands.extend_from_slice(&0u64.to_le_bytes());
    commands.extend_from_slice(&32u32.to_le_bytes());
    commands.extend_from_slice(&0u32.to_le_bytes());
    commands.extend_from_slice(entry_name);
    let parsed = mach_header(&header).unwrap();
    assert_eq!(parsed.filetype, MH_FILESET);
    assert_eq!(parsed.ncmds, 3);
    let info = load_info(&commands, parsed.ncmds);
    assert_eq!(info.text, Some(0xffff_fe00_0700_4000));
    assert_eq!(info.uuid, Some([0xab; 16]));
    assert_eq!(info.kernel, Some(0xffff_fe00_0700_8000));
    // A truncated command list stops where it breaks.
    assert_eq!(load_info(&commands[..40], 3), LoadInfo::default());
    // An x86 or 32-bit header is none.
    let mut other = header.clone();
    other[4] = 7;
    assert!(mach_header(&other).is_none());
}
