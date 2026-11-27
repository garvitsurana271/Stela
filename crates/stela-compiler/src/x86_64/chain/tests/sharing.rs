//! Probes, tails and frames for code whose aliases share translations
//! (`--share-aliases on`): every transfer into a block of another page
//! sets the state's page base to the target's page
//! ([`crate::GuestState::page_base`]), which position independent code
//! adds its offsets to; without sharing nothing writes it. A relative
//! tail ([`relative_lookup_tail`]) takes its target from the page base the
//! code runs at, so one copy serves every alias.
use super::super::*;
use super::Controlled;
use alloc::boxed::Box;
use core::ffi::c_void;
unsafe extern "C" {
    fn mmap(
        address: *mut c_void,
        bytes: usize,
        protection: i32,
        flags: i32,
        fd: i32,
        offset: i64,
    ) -> *mut c_void;
    fn munmap(address: *mut c_void, bytes: usize) -> i32;
}

const PAGE: usize = 4096;
/// Sites: the lookup tail, a relative tail, and the probes.
const LOOKUP: usize = 64;
const RELATIVE: usize = 512;
const PROBES: usize = 1024;
const REGION: usize = 4 * PAGE;
const RAN: u32 = 3;
const CONTEXTS: [u64; 2] = [5 << LOOKUP_SALT_BITS, 9 << LOOKUP_SALT_BITS];
/// The relative tail's target, from the page it runs at.
const OFFSET: i32 = 0x2040;

struct Pages(*mut u8, usize);
impl Drop for Pages {
    fn drop(&mut self) {
        // SAFETY: our mapping, nothing runs it any more.
        assert_eq!(unsafe { munmap(self.0.cast(), self.1) }, 0);
    }
}
fn pages(bytes: usize) -> Pages {
    // SAFETY: private anonymous memory, unmapped on drop.
    let base = unsafe { mmap(core::ptr::null_mut(), bytes, 7, 0x22, -1, 0) };
    assert_ne!(base as usize, usize::MAX);
    Pages(base.cast(), bytes)
}
fn put(pages: &Pages, offset: usize, bytes: &[u8]) {
    assert!(offset + bytes.len() <= pages.1);
    // SAFETY: inside the mapping; nothing runs it while it is written.
    unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), pages.0.add(offset), bytes.len()) };
}

/// A block that returns its marker in RAX and 99 in EDX.
fn block() -> Pages {
    let block = pages(PAGE);
    let mut code = vec![0x48, 0xb8];
    code.extend(0x7703_u64.to_le_bytes());
    code.extend([0xba, 99, 0, 0, 0, 0xc3]);
    put(&block, 0, &code);
    block
}

/// `mov rax, rsi` and a jump to the lookup tail at the region's start, a
/// jump to the relative tail at 16; the tails and the probes, with sharing
/// or without.
fn region(sharing: bool) -> Pages {
    let region = pages(REGION);
    let base = region.0 as u64;
    let mut code = vec![0x48, 0x89, 0xf0];
    code.extend(jump(base + 3, base + LOOKUP as u64).unwrap());
    put(&region, 0, &code);
    put(
        &region,
        16,
        &jump(base + 16, base + RELATIVE as u64).unwrap(),
    );
    let word = ExitWord::from(ExitKind::Return);
    let probe = base + (PROBES + PROBE_ENTRY) as u64;
    put(
        &region,
        LOOKUP,
        &lookup_tail_for(base + LOOKUP as u64, word, RAN, probe, 0, sharing).unwrap(),
    );
    let direct = ExitWord::from(ExitKind::Direct);
    put(
        &region,
        RELATIVE,
        &relative_lookup_tail(base + RELATIVE as u64, OFFSET, direct, RAN, probe).unwrap(),
    );
    put(&region, PROBES, &lookup_dispatcher_for(0, sharing).unwrap());
    region
}

struct World {
    table: LookupTable,
    itlb: LookupTable,
    control: Box<Controlled>,
    block: Pages,
    region: Pages,
}

impl World {
    fn new(sharing: bool) -> Self {
        let mut table = LookupTable::new();
        let itlb = LookupTable::itlb();
        let stamps = table.new_stamps();
        let control = Box::new(Controlled {
            control: ChainControl {
                table: table.address(),
                itlb: itlb.address(),
                contexts: CONTEXTS,
                salts: CONTEXTS.map(|context| itlb.salt(context)),
                masks: [table.mask(), itlb.mask()],
                stamps,
                ..ChainControl::default()
            },
            account: Account::new(MAX_BUDGET),
        });
        World {
            table,
            itlb,
            control,
            block: block(),
            region: region(sharing),
        }
    }

    /// The page of `pc` translating to `key`, the block entered at `pc`.
    fn map(&mut self, pc: u64, key: u64) {
        let context = self.control.contexts[half(pc)];
        self.itlb.insert(
            self.itlb.page_slot(pc, context),
            LookupEntry::page(pc & !0xfff, context, key),
        );
        self.table.insert(
            self.table.slot(pc, key),
            LookupEntry {
                pc,
                key,
                entry: self.block.0 as u64,
                block: 3,
                targets: 0b1111,
            },
        );
    }

    /// Run the stub at `stub` with the target in RSI and the page base
    /// `page`: RAX, RDX and the page base the run left.
    fn run(&mut self, stub: usize, target: u64, page: u64) -> (u64, u64, u64) {
        let region = &self.region;
        let mut left = 0;
        let result = self.control.run(|frame| {
            frame.page_base = crate::PageBase(page);
            // SAFETY: the region holds the stubs, tails and probes with the
            // block ABI; they touch the frame's control, the tables, the
            // stamps and the block, which outlive the call.
            let result = unsafe {
                crate::x86_64::native::enter_counted(region.0.add(stub), frame, target, 0)
            };
            left = frame.page_base.0;
            result
        });
        (result as u64, (result >> 64) as u64, left)
    }
}

/// A probe's hit and the jump cache's both set the page base to the
/// target's page; their miss leaves it, as does every transfer without
/// sharing.
#[test]
fn every_transfer_into_another_page_sets_the_page_base() {
    const PC: u64 = 0x1_8a34_5f40;
    const OTHER: u64 = 0x7000_1000;
    for sharing in [true, false] {
        let mut world = World::new(sharing);
        world.map(PC, block_key(0x4000_0000, 1));
        let entered = if sharing { PC & !0xfff } else { OTHER };
        // The probe, then the jump cache's hit.
        for _ in 0..3 {
            assert_eq!(world.run(0, PC, OTHER), (0x7703, 99, entered));
        }
        // An unmapped target exits as it was, the page base left.
        let missed = world.run(0, PC + 0x10_0000, OTHER);
        assert_eq!((missed.0, missed.2), (PC + 0x10_0000, OTHER));
    }
}

/// One relative tail serves every alias: its target is the page the code
/// runs at plus its offset, at the shared cache's two slides.
#[test]
fn a_relative_tail_takes_its_target_from_the_page_it_runs_at() {
    const SLIDE: u64 = 0xdf8_8000;
    const HOME: u64 = 0x1_8a34_5000;
    let mut world = World::new(true);
    for page in [HOME, HOME + SLIDE] {
        let target = page + OFFSET as u64;
        // Unmapped, the exit returns the target to the engine.
        let unmapped = world.run(16, 0, page);
        assert_eq!((unmapped.0, unmapped.2), (target, page));
        world.map(target, block_key(0x4000_0000 + page, 1));
        assert_eq!(world.run(16, 0, page), (0x7703, 99, target & !0xfff));
    }
}

/// The tails of shared code fit the room the engine gives each.
#[test]
fn sharing_tails_fit_their_room() {
    let word = ExitWord::from(ExitKind::Direct);
    for btype in 0..4 {
        assert!(lookup_dispatcher_for(btype, true).is_some());
        assert!(lookup_tail_for(0x10_0000, word, 7, 0x10_8000, btype, true).is_some());
        assert!(
            stamped_branch_tail_for(
                0x10_0000,
                0x20_0000,
                3,
                0x1_8a34_5f40,
                word,
                7,
                9,
                0x10_4000,
                0x10_8000,
                btype,
                true,
            )
            .is_some()
        );
    }
    assert!(paired_tail_for(0x10_0000, ExitKind::Return, 7, 0x10_8000, true).is_some());
    assert!(relative_lookup_tail(0x10_0000, -0x7000_0000, word, 7, 0x10_8000).is_some());
    // Off, every tail is the one before sharing existed.
    assert_eq!(
        lookup_tail_for(0x10_0000, word, 7, 0x10_8000, 0, false),
        lookup_tail(0x10_0000, word, 7, 0x10_8000, 0)
    );
    assert_eq!(lookup_dispatcher_for(2, false), lookup_dispatcher(2));
}
