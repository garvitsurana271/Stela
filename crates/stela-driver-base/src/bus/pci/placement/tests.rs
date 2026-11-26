#![expect(
    clippy::single_range_in_vec_init,
    reason = "a window or claim list of one range is a list, as `place` takes it"
)]
use super::*;
use alloc::vec;

const PAGE_BAR: Request = Request {
    bytes: 0x1000,
    wide: true,
    prefetchable: false,
};
const NARROW: Request = Request {
    bytes: 0x1000,
    wide: false,
    prefetchable: false,
};

fn overlaps(a: &Range<u64>, b: &Range<u64>) -> bool {
    a.start < b.end && b.start < a.end
}

#[test]
fn an_empty_window_gives_its_first_aligned_page() {
    assert_eq!(
        place(&[0x8000_0000..0x9000_0000], &[], PAGE_BAR),
        Ok(0x8000_0000..0x8000_1000)
    );
    assert_eq!(
        place(&[0x8000_0800..0x9000_0000], &[], PAGE_BAR),
        Ok(0x8000_1000..0x8000_2000)
    );
}

#[test]
fn claims_are_stepped_around_and_touching_ones_are_no_overlap() {
    let claims = [
        0x8000_0000..0x8000_1000,
        0x8000_1000..0x8000_3000,
        0x8000_4000..0x8000_5000,
    ];
    // The gap between the second and third claim fits a page exactly.
    assert_eq!(
        place(&[0x8000_0000..0x9000_0000], &claims, PAGE_BAR),
        Ok(0x8000_3000..0x8000_4000)
    );
    // Overlapping and unsorted claims are joined.
    let claims = [
        0x8000_2000..0x8000_6000,
        0x8000_0000..0x8000_3000,
        0x8000_5000..0x8000_8000,
    ];
    assert_eq!(
        place(&[0x8000_0000..0x9000_0000], &claims, PAGE_BAR),
        Ok(0x8000_8000..0x8000_9000)
    );
}

#[test]
fn a_bar_is_aligned_to_its_size_and_a_small_one_takes_a_page() {
    let big = Request {
        bytes: 0x10_0000,
        ..PAGE_BAR
    };
    assert_eq!(
        place(&[0x8000_1000..0x9000_0000], &[], big),
        Ok(0x8010_0000..0x8020_0000)
    );
    assert_eq!(
        place(
            &[0x8000_0000..0x9000_0000],
            &[0x8000_0000..0x8010_0001],
            big
        ),
        Ok(0x8020_0000..0x8030_0000)
    );
    let small = Request {
        bytes: 0x100,
        ..PAGE_BAR
    };
    assert_eq!(
        place(
            &[0x8000_0000..0x9000_0000],
            &[0x8000_0000..0x8000_0100],
            small
        ),
        Ok(0x8000_1000..0x8000_2000)
    );
}

#[test]
fn below_four_gib_comes_first_and_only_a_64_bit_bar_goes_above() {
    let windows = [0x40_0000_0000..0x60_0000_0000, 0x7200_0000..0x7e40_0000];
    assert_eq!(place(&windows, &[], PAGE_BAR), Ok(0x7200_0000..0x7200_1000));
    // The window below is full.
    let full = [0x7200_0000..0x7e40_0000];
    assert_eq!(
        place(&windows, &full, PAGE_BAR),
        Ok(0x40_0000_0000..0x40_0000_1000)
    );
    assert_eq!(place(&windows, &full, NARROW), Err(Refusal::NoRoom(0x1000)));
    // Only a window above 4 GiB: a 32-bit BAR has none.
    assert_eq!(
        place(&[0x40_0000_0000..0x60_0000_0000], &[], NARROW),
        Err(Refusal::NoWindow)
    );
}

#[test]
fn a_window_across_four_gib_is_cut_there_for_a_32_bit_bar() {
    let window = [0xffff_e000..0x1_0001_0000];
    let claims = [0xffff_e000..0xffff_f000];
    assert_eq!(
        place(&window, &claims, NARROW),
        Ok(0xffff_f000..0x1_0000_0000)
    );
    let claims = [0xffff_e000..0x1_0000_0000];
    assert_eq!(
        place(&window, &claims, NARROW),
        Err(Refusal::NoRoom(0x1000))
    );
    assert_eq!(
        place(&window, &claims, PAGE_BAR),
        Ok(0x1_0000_0000..0x1_0000_1000)
    );
}

#[test]
fn nothing_goes_below_one_mib() {
    // A root bridge's legacy VGA window and a window from zero.
    let windows = [0xa_0000..0xc_0000, 0..0x20_0000];
    assert_eq!(place(&windows, &[], PAGE_BAR), Ok(0x10_0000..0x10_1000));
    assert_eq!(
        place(&[0xa_0000..0xc_0000], &[], PAGE_BAR),
        Err(Refusal::NoWindow)
    );
}

#[test]
fn sizes_that_are_no_bar_are_refused_before_any_window_is_read() {
    for bytes in [0, 8, 0x1800, MAX_BYTES * 2] {
        let request = Request { bytes, ..PAGE_BAR };
        assert_eq!(
            place(&[0x8000_0000..0x9000_0000], &[], request),
            Err(Refusal::Size(bytes))
        );
    }
    assert_eq!(place(&[], &[], PAGE_BAR), Err(Refusal::NoWindow));
}

#[test]
fn behind_a_bridge_only_its_windows_hold_a_bar() {
    let root = [0x7200_0000..0xc000_0000];
    let bridge = BridgeWindows {
        memory: Some(0x7e00_0000..0x7e10_0000),
        prefetchable: Some(0x7800_0000..0x7c00_0000),
    };
    let allowed = intersect(&root, &bridge.admitting(NARROW));
    assert_eq!(allowed, vec![0x7e00_0000..0x7e10_0000]);
    let claims = [0x7e00_0000..0x7e0f_f000];
    assert_eq!(
        place(&allowed, &claims, NARROW),
        Ok(0x7e0f_f000..0x7e10_0000)
    );
    // The bridge's window is full: nothing outside it is given.
    let claims = [0x7e00_0000..0x7e10_0000];
    assert_eq!(
        place(&allowed, &claims, NARROW),
        Err(Refusal::NoRoom(0x1000))
    );
    // A prefetchable BAR may also use the prefetchable window.
    let prefetchable = Request {
        prefetchable: true,
        ..NARROW
    };
    let allowed = intersect(&root, &bridge.admitting(prefetchable));
    assert_eq!(
        place(&allowed, &claims, prefetchable),
        Ok(0x7800_0000..0x7800_1000)
    );
    // A bridge with its windows closed admits nothing.
    let closed = BridgeWindows::default();
    assert_eq!(
        place(&intersect(&root, &closed.admitting(NARROW)), &[], NARROW),
        Err(Refusal::NoWindow)
    );
}

#[test]
fn only_a_bar_at_zero_is_unassigned() {
    let bar = |address| Bar::Memory {
        address,
        size: 0x1000,
        prefetchable: false,
        wide: true,
    };
    assert!(unassigned(bar(0)));
    // An assigned BAR is the firmware's wherever it is, even outside every
    // window the placement would use.
    for address in [0x1000, 0x7e3f_f000, 0xfed0_0000, 0x40_0000_0000] {
        assert!(!unassigned(bar(address)));
    }
    assert!(!unassigned(Bar::Io {
        port: 0,
        size: 0x20
    }));
}

#[test]
fn an_assigned_bar_counts_as_large_as_its_alignment_allows() {
    assert_eq!(assigned_extent(0), None);
    assert_eq!(assigned_extent(0x7e3f_f000), Some(0x7e3f_f000..0x7e40_0000));
    assert_eq!(assigned_extent(0x7e00_0000), Some(0x7e00_0000..0x8000_0000));
    assert_eq!(
        assigned_extent(0x8000_0000_0000_0000),
        Some(0x8000_0000_0000_0000..u64::MAX)
    );
    // Whatever its real size, a BAR at that base decodes inside the bound,
    // so a placement clear of the bound is clear of the BAR.
    for size in [0x10u64, 0x1000, 0x10_0000, 0x200_0000] {
        let base = 0x7e00_0000;
        let bound = assigned_extent(base).unwrap();
        assert!(base + size <= bound.end);
    }
}

#[test]
fn a_type_0_header_claims_its_assigned_bars_and_rom() {
    let mut words: Words = [0; 12];
    // BAR0: 64-bit memory at 0x60_1c00_0000.
    words[0] = 0x1c00_0004;
    words[1] = 0x60;
    // BAR2: I/O.
    words[2] = 0x3001;
    // BAR3: 32-bit memory at 0x7e3f_0000.
    words[3] = 0x7e3f_0000;
    // BAR4: unassigned 64-bit memory, BAR5 its upper half.
    words[4] = 0x4;
    // Expansion ROM at 0x7e30_0000, disabled.
    words[8] = 0x7e30_0000;
    let (claims, windows) = decoded(&words, false);
    assert_eq!(windows, None);
    assert_eq!(
        claims,
        vec![
            0x60_1c00_0000..0x60_2000_0000,
            0x7e3f_0000..0x7e40_0000,
            0x7e30_0000..0x7e40_0000,
        ]
    );
}

#[test]
fn a_type_1_header_claims_its_windows_exactly() {
    let mut words: Words = [0; 12];
    // BAR0: 32-bit at 0x7e40_0000; BAR1 unused.
    words[0] = 0x7e40_0000;
    // Bus numbers at 0x18 (index 2) are no address.
    words[2] = 0x0001_0100;
    // Memory window 0x7e00_0000..=0x7e1f_ffff.
    words[4] = 0x7e10_7e00;
    // 64-bit prefetchable window 0x40_0000_0000..=0x40_0fff_ffff.
    words[5] = 0x0ff1_0001;
    words[6] = 0x40;
    words[7] = 0x40;
    // Expansion ROM at 0x38 (index 10).
    words[10] = 0x7e20_0001;
    let (claims, windows) = decoded(&words, true);
    let windows = windows.unwrap();
    assert_eq!(windows.memory, Some(0x7e00_0000..0x7e20_0000));
    assert_eq!(windows.prefetchable, Some(0x40_0000_0000..0x40_1000_0000));
    assert_eq!(
        claims,
        vec![0x7e40_0000..0x7e80_0000, 0x7e20_0000..0x7e40_0000]
    );
    // A base above its limit closes a window.
    let closed = BridgeWindows::from_words(0x0000_fff0, 0x0000_fff0, (0, 0));
    assert_eq!(closed, BridgeWindows::default());
}

/// A DWord address space descriptor.
fn dword(kind: u8, flags: u8, minimum: u32, maximum: u32, translation: u32, bytes: u32) -> Vec<u8> {
    let mut raw = vec![0x87, 23, 0, kind, flags, 0];
    for value in [0, minimum, maximum, translation, bytes] {
        raw.extend(value.to_le_bytes());
    }
    raw
}

/// A QWord address space descriptor.
fn qword(flags: u8, minimum: u64, maximum: u64, bytes: u64) -> Vec<u8> {
    let mut raw = vec![0x8a, 43, 0, 0, flags, 0];
    for value in [0, minimum, maximum, 0, bytes] {
        raw.extend(value.to_le_bytes());
    }
    raw
}

const END: [u8; 2] = [0x79, 0];

#[test]
fn a_root_bridge_crs_gives_its_producer_windows_and_nothing_else_as_one() {
    let mut buffer = Vec::new();
    // WordBusNumber, a small IO port descriptor.
    buffer.extend([0x88, 13, 0, 2, 0x0c, 0, 0, 0, 0, 0, 0xff, 0, 0, 0, 0, 1]);
    buffer.extend([0x47, 1, 0xf8, 0x0c, 0xf8, 0x0c, 1, 8]);
    // Legacy VGA window and the 32-bit window, producers.
    buffer.extend(dword(0, 0x0c, 0xa_0000, 0xb_ffff, 0, 0x2_0000));
    buffer.extend(dword(0, 0x0c, 0x7200_0000, 0xbfff_ffff, 0, 0x4e00_0000));
    // A window whose length is shorter than minimum..=maximum is cut.
    buffer.extend(qword(0x0c, 0x40_0000_0000, 0x7f_ffff_ffff, 0x20_0000_0000));
    // A zero-length producer opens nothing.
    buffer.extend(dword(0, 0x0c, 0xc000_0000, 0xc000_ffff, 0, 0));
    // An I/O-typed DWord is no memory.
    buffer.extend(dword(1, 0x0c, 0xd00, 0xffff, 0, 0xf300));
    buffer.extend(END);
    let memory = scan_memory(&buffer).unwrap();
    let windows: Vec<_> = memory.iter().filter_map(|m| m.window.clone()).collect();
    assert_eq!(
        windows,
        vec![
            0xa_0000..0xc_0000,
            0x7200_0000..0xc000_0000,
            0x40_0000_0000..0x60_0000_0000,
        ]
    );
    assert_eq!(memory.len(), 4);
    assert_eq!(memory[3].claim, vec![0xc000_0000..0xc001_0000]);
}

#[test]
fn a_device_crs_claims_every_memory_descriptor_widely() {
    let mut buffer = Vec::new();
    // Memory32Fixed 0xfed1_0000, 0x8000 bytes.
    buffer.extend([0x86, 9, 0, 1]);
    buffer.extend(0xfed1_0000u32.to_le_bytes());
    buffer.extend(0x8000u32.to_le_bytes());
    // Memory32: relocatable between 0xe000_0000 and 0xe100_0000, 0x1000 long.
    buffer.extend([0x85, 17, 0, 1]);
    for value in [0xe000_0000u32, 0xe100_0000, 0x1000, 0x1000] {
        buffer.extend(value.to_le_bytes());
    }
    // A consumer DWord, which opens no window.
    buffer.extend(dword(0, 0x0d, 0xfe00_0000, 0xfe00_ffff, 0, 0x1_0000));
    // Memory24: 0xd_0000..=0xd_ffff, base step and 0x100 length units.
    buffer.extend([0x81, 9, 0, 1, 0x00, 0x0d, 0x00, 0x0d, 0, 0, 0x00, 0x01]);
    // Dependent functions: both alternatives are scanned.
    buffer.push(0x30);
    buffer.extend([0x86, 9, 0, 1]);
    buffer.extend(0xfc00_0000u32.to_le_bytes());
    buffer.extend(0x1000u32.to_le_bytes());
    buffer.push(0x30);
    buffer.extend([0x86, 9, 0, 1]);
    buffer.extend(0xfc10_0000u32.to_le_bytes());
    buffer.extend(0x1000u32.to_le_bytes());
    buffer.push(0x38);
    buffer.extend(END);
    let memory = scan_memory(&buffer).unwrap();
    assert!(memory.iter().all(|m| m.window.is_none()));
    let claims: Vec<_> = memory.iter().flat_map(|m| m.claim.clone()).collect();
    assert_eq!(
        claims,
        vec![
            0xfed1_0000..0xfed1_8000,
            0xe000_0000..0xe100_1000,
            0xfe00_0000..0xfe01_0000,
            0xd_0000..0xe_0000,
            0xfc00_0000..0xfc00_1000,
            0xfc10_0000..0xfc10_1000,
        ]
    );
}

#[test]
fn a_translated_descriptor_claims_both_sides_and_a_cut_buffer_is_refused() {
    let mut buffer = dword(0, 0x0d, 0x1000_0000, 0x1000_0fff, 0x2000_0000, 0x1000);
    buffer.extend(END);
    let memory = scan_memory(&buffer).unwrap();
    assert_eq!(memory[0].translation, 0x2000_0000);
    assert_eq!(
        memory[0].claim,
        vec![0x1000_0000..0x1000_1000, 0x3000_0000..0x3000_1000]
    );
    // Without its end tag, or cut inside a descriptor.
    let cut = &buffer[..buffer.len() - 2];
    assert_eq!(scan_memory(cut), Err(ScanError::Truncated { offset: 26 }));
    assert!(matches!(
        scan_memory(&buffer[..10]),
        Err(ScanError::Truncated { offset: 0 })
    ));
}

fn table(signature: &[u8; 4], header: usize, body: &[u8]) -> Vec<u8> {
    let mut raw = vec![0; header];
    raw[..4].copy_from_slice(signature);
    raw.extend_from_slice(body);
    let length = raw.len() as u32;
    raw[4..8].copy_from_slice(&length.to_le_bytes());
    raw
}

#[test]
fn remapping_tables_claim_unit_registers_and_reserved_memory() {
    let mut body = Vec::new();
    // DRHD, size field 0: one page, taken as two.
    body.extend([0, 0, 16, 0, 0, 0, 0, 0]);
    body.extend(0xfed9_0000u64.to_le_bytes());
    // DRHD, size field 2: four pages.
    body.extend([0, 0, 16, 0, 0, 2, 0, 0]);
    body.extend(0xfed9_1000u64.to_le_bytes());
    // RMRR 0x6c00_0000..=0x6c7f_ffff.
    body.extend([1, 0, 24, 0, 0, 0, 0, 0]);
    body.extend(0x6c00_0000u64.to_le_bytes());
    body.extend(0x6c7f_ffffu64.to_le_bytes());
    let dmar = table(b"DMAR", 48, &body);
    assert_eq!(
        table_claims(&dmar),
        vec![
            0xfed9_0000..0xfed9_2000,
            0xfed9_1000..0xfed9_5000,
            0x6c00_0000..0x6c80_0000,
        ]
    );
    let mut body = Vec::new();
    // IVHD type 0x11 at 0xfd20_0000.
    body.extend([0x11, 0, 40, 0, 0, 0, 0, 0]);
    body.extend(0xfd20_0000u64.to_le_bytes());
    body.extend([0; 24]);
    // IVMD type 0x20: 0x9d00_0000, 0x10_0000 bytes.
    body.extend([0x20, 0, 32, 0, 0, 0, 0, 0]);
    body.extend([0; 8]);
    body.extend(0x9d00_0000u64.to_le_bytes());
    body.extend(0x10_0000u64.to_le_bytes());
    let ivrs = table(b"IVRS", 48, &body);
    assert_eq!(
        table_claims(&ivrs),
        vec![0xfd20_0000..0xfd28_0000, 0x9d00_0000..0x9d10_0000]
    );
}

#[test]
fn platform_tables_claim_ecam_interrupt_controllers_and_the_hpet() {
    let mut body = vec![0; 8];
    body.extend(0xc000_0000u64.to_le_bytes());
    body.extend([0, 0, 0, 0x2b, 0, 0, 0, 0]);
    let mcfg = table(b"MCFG", 36, &body);
    assert_eq!(table_claims(&mcfg), vec![0xc000_0000..0xc2c0_0000]);

    let mut madt = table(b"APIC", 44, &[1, 12, 2, 0, 0, 0, 0xc0, 0xfe, 0, 0, 0, 0]);
    madt[36..40].copy_from_slice(&0xfee0_0000u32.to_le_bytes());
    assert_eq!(
        table_claims(&madt),
        vec![0xfee0_0000..0xfee0_1000, 0xfec0_0000..0xfec0_1000]
    );

    let mut hpet = table(b"HPET", 56, &[]);
    hpet[44..52].copy_from_slice(&0xfed0_0000u64.to_le_bytes());
    assert_eq!(table_claims(&hpet), vec![0xfed0_0000..0xfed0_0400]);
    // A table it does not know claims nothing.
    assert_eq!(table_claims(&table(b"FACP", 36, &[1, 2, 3])), vec![]);
    assert_eq!(table_claims(&[0; 10]), vec![]);
}

/// The Dell of the report: the firmware's root bridge left the touchpad's
/// I2C controller at zero. Its 32-bit `_CRS` window reaches the ECAM base,
/// above what the firmware's own placement used.
#[test]
fn the_touchpad_controller_of_the_report_finds_a_page_past_the_firmware_placement() {
    let windows = [0x7200_0000..0xc000_0000, 0x40_0000_0000..0x60_1d20_0000];
    let claims = [
        // The Thunderbolt port's window and the NVMe port's.
        0x7200_0000..0x7e00_0000,
        0x7e00_0000..0x7e10_0000,
        // Root bus BARs, bounded by their alignment.
        assigned_extent(0x7e10_0000).unwrap(),
        assigned_extent(0x7e20_0000).unwrap(),
        assigned_extent(0x7e3f_f000).unwrap(),
        // The integrated GPU's BARs above 4 GiB.
        assigned_extent(0x60_1c00_0000).unwrap(),
        // ECAM and the fixed range.
        0xc000_0000..0xd000_0000,
        X86_FIXED,
    ];
    let placed = place(&windows, &claims, PAGE_BAR).unwrap();
    assert_eq!(placed, 0x7e40_0000..0x7e40_1000);
    assert!(claims.iter().all(|claim| !overlaps(claim, &placed)));
}

/// Whatever windows and claims, a placement lies in one window, is aligned
/// to its footprint and overlaps no claim.
#[test]
fn every_placement_is_inside_a_window_aligned_and_clear_of_every_claim() {
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let mut placed_any = 0;
    for _ in 0..2000 {
        let span = |next: &mut dyn FnMut() -> u64, unit: u64| {
            let start = (next() % 0x1_4000) * unit;
            start..start + (next() % 64 + 1) * unit
        };
        let windows: Vec<_> = (0..next() % 3 + 1)
            .map(|_| span(&mut next, 0x1_0000))
            .collect();
        let claims: Vec<_> = (0..next() % 24).map(|_| span(&mut next, 0x1000)).collect();
        let request = Request {
            bytes: 1 << (4 + next() % 17),
            wide: next() % 2 == 0,
            prefetchable: false,
        };
        let Ok(placed) = place(&windows, &claims, request) else {
            continue;
        };
        placed_any += 1;
        let footprint = request.footprint();
        assert_eq!(placed.end - placed.start, footprint);
        assert_eq!(placed.start % footprint, 0);
        assert!(placed.start >= FLOOR);
        assert!(request.wide || placed.end <= FOUR_GIB);
        assert!(
            windows
                .iter()
                .any(|window| window.start <= placed.start && placed.end <= window.end)
        );
        assert!(claims.iter().all(|claim| !overlaps(claim, &placed)));
    }
    assert!(placed_any > 500);
}
