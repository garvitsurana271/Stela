//! Host model of process switching through one PE's ITLB: address spaces
//! take turns, each fetching its hot user pages at the same VAs (every
//! process's main binary and stack sit at one address), and every turn
//! fetches the kernel's hot pages in the upper half, global as XNU maps its
//! text. The PE fetches through the engine's own context, ITLB and TLBI
//! paths ([`Machine::contexts`], [`Machine::current_key`],
//! [`Machine::translate_page`], [`Machine::tlb_maintained`]); only the walk
//! is left out, as its result is the block key the model names.
//!
//! Every turn also fetches hot pages of a shared library at the same VAs and
//! frames in every process, global as XNU maps the dyld shared cache, and
//! every page is in a 16 KiB leaf, as XNU maps code.
//!
//! `cargo test -p stela-engine --release --lib process_switching_miss_rates
//! -- --ignored --nocapture` prints ITLB misses per switch for each count of
//! processes, with per-page TLBIs of single ASIDs between turns, and with a
//! TLBI of everything every so often (which the architecture requires to
//! drop everything of the regime).

use super::*;
use mmu::tlbi::{Everywhere, Reach};

const MSR_TTBR0_EL1_X0: u32 = 0xd518_2000;
/// Hot pages of each process, at the same VAs in every one.
const USER_PAGES: u64 = 48;
/// Hot kernel pages each turn runs (system call, interrupt, switch paths).
const KERNEL_PAGES: u64 = 64;
const USER_BASE: u64 = 0x1_0000_0000;
const KERNEL_BASE: u64 = 0xffff_fe00_1000_0000;
/// Hot pages of the shared library each turn runs, at the same VAs and
/// frames in every process.
const SHARED_PAGES: u64 = 64;
const SHARED_BASE: u64 = 0x1_8000_0000;
/// The leaf every page is mapped in.
const LEAF: u64 = 0x4000;

/// What one run measured.
#[derive(Debug, Clone, Copy, Default)]
struct Rates {
    switches: u64,
    user_misses: u64,
    shared_misses: u64,
    kernel_misses: u64,
}

impl Rates {
    fn per_switch(self) -> (f64, f64, f64) {
        let switches = self.switches.max(1) as f64;
        (
            self.user_misses as f64 / switches,
            self.shared_misses as f64 / switches,
            self.kernel_misses as f64 / switches,
        )
    }
}

/// A deterministic choice of the next process: mostly the recent ones, as
/// a desktop's few busy processes run while hundreds wake now and then.
struct Picks(u64);

impl Picks {
    fn next(&mut self, processes: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        let draw = self.0 >> 33;
        // Three in four turns go to the busiest eighth.
        let busy = (processes / 8).max(1);
        if !draw.is_multiple_of(4) {
            (draw >> 2) % busy
        } else {
            (draw >> 2) % processes
        }
    }
}

/// Fetch `pc` at `el` in the current contexts, whose page's block key is
/// `key` through a [`LEAF`] leaf, `global` or not: whether the ITLB held it.
fn touch(
    machine: &mut Machine<FakeHost, NoDevices>,
    el: u8,
    pc: u64,
    key: u64,
    global: bool,
) -> bool {
    let variant = machine.current_variant(false, false);
    let contexts = machine.contexts(el, variant);
    machine.chain.set_contexts(contexts);
    if machine.current_key(pc) == Some(key) {
        return true;
    }
    let half = chain::half(pc);
    machine.fetched_through(half, pc, LEAF, global);
    // The walk may have forgotten contexts; the fetch records the page in
    // the ones it runs in after, as its walk left it in the fetch cache.
    let contexts = machine.contexts(el, variant);
    machine.chain.set_contexts(contexts);
    // The fetch runs at `el`, which its cache entry records.
    let running = core::mem::replace(&mut machine.cpu.pstate.el, el);
    let page = pc & !(FETCH_PAGE - 1);
    let physical = chain::key_frame(key) << 12;
    machine.fetch[(page / FETCH_PAGE) as usize % FETCH_CACHE] = Some(FetchEntry {
        page,
        physical,
        guarded: false,
        global,
        size: LEAF,
        el: el | u8::from(machine.cpu.pstate.guarded) << 4,
        epoch: machine.fetch_epoch(half),
    });
    machine.translate_leaf(pc, key, physical, variant);
    machine.cpu.pstate.el = running;
    false
}

/// The 8-bit ASID of process `process` (XNU's on an 8-bit chip shares one
/// between processes past 255).
fn asid(process: u64) -> u64 {
    process % 255 + 1
}

/// Run `switches` turns among `processes`: a TLBI of one page of one ASID
/// after each turn with `page_tlbis`, and a TLBI of everything every
/// `all_every` turns when nonzero.
fn run(processes: u64, switches: u64, page_tlbis: bool, all_every: u64) -> Rates {
    let mut machine = machine(&[]);
    let kernel_el = machine.cpu.pstate.el;
    let ttbr0 = machine.cpu.read(SystemRegister::TTBR0_EL1);
    let mut picks = Picks(processes ^ 0x5eed);
    let mut rates = Rates::default();
    // Blocks of the one variant the model runs in have key state 2.
    let variant = machine.current_variant(false, false);
    assert_eq!(machine.variant_id(variant).0, 0);
    // Process `p`'s user page `n` maps frame `p * USER_PAGES + n`.
    let user_key =
        |process: u64, page: u64| chain::block_key((process * USER_PAGES + page) << 12, 2);
    let kernel_key = |page: u64| chain::block_key((1 << 30 | page) << 12, 2);
    let shared_key = |page: u64| chain::block_key((1 << 29 | page) << 12, 2);
    for turn in 0..switches {
        let process = picks.next(processes);
        let table = ttbr0 & 0xffff_ffff_ffff & !0xfff_ffff | (process + 1) << 16;
        machine.cpu.state.x[0] = table | asid(process) << 48;
        machine.system_instruction(MSR_TTBR0_EL1_X0).unwrap();
        rates.switches += 1;
        for page in 0..KERNEL_PAGES / 2 {
            let pc = KERNEL_BASE + page * FETCH_PAGE;
            if !touch(&mut machine, kernel_el, pc, kernel_key(page), true) {
                rates.kernel_misses += 1;
            }
        }
        for page in 0..USER_PAGES {
            let pc = USER_BASE + page * FETCH_PAGE;
            if !touch(&mut machine, 0, pc, user_key(process, page), false) {
                rates.user_misses += 1;
            }
        }
        for page in 0..SHARED_PAGES {
            let pc = SHARED_BASE + page * FETCH_PAGE;
            if !touch(&mut machine, 0, pc, shared_key(page), true) {
                rates.shared_misses += 1;
            }
        }
        for page in KERNEL_PAGES / 2..KERNEL_PAGES {
            let pc = KERNEL_BASE + page * FETCH_PAGE;
            if !touch(&mut machine, kernel_el, pc, kernel_key(page), true) {
                rates.kernel_misses += 1;
            }
        }
        if page_tlbis {
            // A page of a heap or a stack (not a hot code page), of one
            // process, as `vm_map` changes make thousands of times a second.
            let victim = picks.next(processes);
            let reach = Reach::Containing(USER_BASE + (USER_PAGES + turn % 64) * FETCH_PAGE);
            machine.tlb_maintained(
                reach,
                Everywhere::Addresses(reach),
                Some(asid(victim) as u16),
            );
            // And one hot page of one process, which that process refills.
            let reach = Reach::Containing(USER_BASE + (turn % USER_PAGES) * FETCH_PAGE);
            machine.tlb_maintained(
                reach,
                Everywhere::Addresses(reach),
                Some(asid(victim) as u16),
            );
        }
        if all_every != 0 && turn % all_every == all_every - 1 {
            machine.tlb_maintained(Reach::All, Everywhere::All, None);
        }
    }
    rates
}

/// Prints misses per switch; the assertions only check the model runs.
#[test]
#[ignore = "manual: opt-in"]
fn process_switching_miss_rates() {
    const SWITCHES: u64 = 20_000;
    println!("processes  tlbis  all_every  user/switch  shared/switch  kernel/switch");
    for &processes in &[16, 48, 96, 192, 384] {
        for (page_tlbis, all_every) in [(false, 0), (true, 0), (true, 22), (true, 60)] {
            let rates = run(processes, SWITCHES, page_tlbis, all_every);
            let (user, shared, kernel) = rates.per_switch();
            println!(
                "{processes:9}  {:5}  {all_every:9}  {user:11.2}  {shared:13.2}  {kernel:13.2}",
                page_tlbis
            );
            assert_eq!(rates.switches, SWITCHES);
        }
    }
}

/// The context of TTBR0 value `table` with ASID `asid` at `el`.
fn context_of(
    machine: &mut Machine<FakeHost, NoDevices>,
    el: u8,
    table: u64,
    asid: u64,
) -> [u64; 2] {
    let variant = machine.current_variant(false, false);
    machine.cpu.state.x[0] = table & !(0xffff << 48) | asid << 48;
    machine.system_instruction(MSR_TTBR0_EL1_X0).unwrap();
    machine.contexts(el, variant)
}

/// Put an ITLB entry of `page` in `context`, the set's other entry kept.
fn put(machine: &mut Machine<FakeHost, NoDevices>, page: u64, context: u64) {
    let slot = machine.itlb.page_slot(page, context);
    machine
        .itlb
        .insert(slot, chain::LookupEntry::page(page, context, 1));
}

/// Whether the ITLB holds `page` in `context`.
fn held(machine: &Machine<FakeHost, NoDevices>, page: u64, context: u64) -> bool {
    let slot = machine.itlb.page_slot(page, context);
    machine.itlb.find(slot, page, context).key == context
}

/// A TLBI by VA and ASID drops the page in that ASID's contexts alone,
/// though other ASIDs' contexts share their salt: with more address spaces
/// than salts, a salt is never what decides. An ASID an 8-bit TCR.AS gave
/// two address spaces drops the page in both.
#[test]
fn a_tlbi_by_va_and_asid_spares_other_asids_that_share_its_salt() {
    const PAGE: u64 = 0x8000;
    let mut machine = machine(&[]);
    let el = machine.cpu.pstate.el;
    let ttbr0 = machine.cpu.read(SystemRegister::TTBR0_EL1);
    let contexts: Vec<(u64, u64)> = (1..=70u64)
        .map(|asid| {
            (
                asid,
                context_of(&mut machine, el, ttbr0 + asid * 0x10_0000, asid)[0],
            )
        })
        .collect();
    let salt = |context: u64| context % chain::LOOKUP_SALTS as u64;
    let (&(asid, mine), &(other_asid, other)) = contexts
        .iter()
        .flat_map(|a| contexts.iter().map(move |b| (a, b)))
        .find(|(a, b)| a.0 != b.0 && salt(a.1) == salt(b.1))
        .expect("70 address spaces share a salt among 64");
    assert_ne!(asid, other_asid);
    // A third address space reusing `asid` (an 8-bit ASID's wrap).
    let reused = context_of(&mut machine, el, ttbr0 + 0x7f0_0000, asid + 256)[0];
    machine.fetched_through(0, PAGE, 0x4000, false);
    for context in [mine, other, reused] {
        put(&mut machine, PAGE, context);
    }
    assert!(held(&machine, PAGE, mine) && held(&machine, PAGE, other));
    let reach = Reach::Containing(PAGE);
    machine.tlb_maintained(reach, Everywhere::Addresses(reach), Some(asid as u16));
    assert!(!held(&machine, PAGE, mine));
    assert!(!held(&machine, PAGE, reused));
    assert!(held(&machine, PAGE, other));
    assert_eq!(
        machine.statistics.itlb_sources[ItlbSource::DropAsid as usize],
        2
    );
}

/// A process's context outlives a thousand others' and finds its pages
/// again; past the half's bound the least recently used goes with its
/// entries, and its key takes a new number whose ITLB entries it lacks.
#[test]
fn contexts_outlive_a_thousand_address_spaces_then_go_least_recently_used() {
    const PAGE: u64 = 0x8000;
    let mut machine = machine(&[]);
    let el = machine.cpu.pstate.el;
    let ttbr0 = machine.cpu.read(SystemRegister::TTBR0_EL1);
    let table = |process: u64| ttbr0 + (process + 1) * 0x10_0000;
    let first = context_of(&mut machine, el, table(0), 1);
    put(&mut machine, PAGE, first[0]);
    for process in 1..=1000 {
        context_of(&mut machine, el, table(process), process % 250 + 1);
    }
    assert_eq!(context_of(&mut machine, el, table(0), 1), first);
    assert!(held(&machine, PAGE, first[0]));
    assert_eq!(machine.statistics.itlb_drops[2], 0);
    for process in 1001..=1000 + contexts::LOWER_CONTEXTS as u64 {
        context_of(&mut machine, el, table(process), process % 250 + 1);
    }
    assert!(machine.statistics.itlb_drops[2] > 0);
    assert!(machine.spaces[0].live(first[0]).is_none());
    let renewed = context_of(&mut machine, el, table(0), 1);
    assert_ne!(renewed[0], first[0]);
    assert_eq!(renewed[1], first[1]);
    assert!(machine.spaces[0].len() <= contexts::LOWER_CONTEXTS);
    // The entry of the context that went is never found for the new one.
    machine.chain.set_contexts(renewed);
    assert_eq!(machine.current_key(PAGE), None);
}

/// Forms without an address remove what [`mmu::tlbi::Scope::covers`]
/// says: in the host (EL2&0), ALLE1, VMALLS12E1 and IPAS2E1 reach no entry
/// of its regime and leave its contexts; ALLE2 and VMALLE1 (EL2&0 in the
/// host) forget them. A TLBI of everything a PE's mail kept forgets every
/// context.
#[test]
fn a_tlbi_of_everything_forgets_only_the_regimes_it_reaches() {
    const HCR_E2H: u64 = 1 << 34;
    const HCR_TGE: u64 = 1 << 27;
    const TLBI_ALLE1: u32 = 0xd50c_879f;
    const TLBI_VMALLS12E1: u32 = 0xd50c_87df;
    const TLBI_IPAS2E1_X0: u32 = 0xd50c_8420;
    const TLBI_ALLE2: u32 = 0xd50c_871f;
    const TLBI_VMALLE1: u32 = 0xd508_871f;
    let mut machine = machine(&[]);
    machine.cpu.pstate.el = 2;
    let hcr = machine.cpu.read(SystemRegister::HCR_EL2) | HCR_E2H | HCR_TGE;
    machine.cpu.registers.set(SystemRegister::HCR_EL2, 0, hcr);
    machine.controls_changed();
    assert_eq!(machine.cpu.tlbi_current().regime, mmu::Regime::El20);
    let variant = machine.current_variant(false, false);
    let contexts = |machine: &mut Machine<FakeHost, NoDevices>| {
        [machine.contexts(2, variant), machine.contexts(0, variant)]
    };
    let before = contexts(&mut machine);
    for word in [TLBI_ALLE1, TLBI_VMALLS12E1, TLBI_IPAS2E1_X0] {
        machine.cpu.state.x[0] = 0x1234;
        machine.system_instruction(word).unwrap();
        assert_eq!(contexts(&mut machine), before, "{word:#x}");
    }
    assert_eq!(
        machine.statistics.itlb_sources[ItlbSource::AllKept as usize],
        3
    );
    let mut last = before;
    for word in [TLBI_ALLE2, TLBI_VMALLE1] {
        machine.system_instruction(word).unwrap();
        let after = contexts(&mut machine);
        for (was, now) in last.iter().flatten().zip(after.iter().flatten()) {
            assert_ne!(was, now, "{word:#x}");
        }
        last = after;
    }
    assert_eq!(
        machine.statistics.itlb_sources[ItlbSource::AllLocal as usize],
        2
    );
    // The contexts of another regime (EL1&0, made outside the host) go
    // with a TLBI that reaches nothing of the current one; the current
    // regime's stay. One that reaches it forgets every context.
    let host = |machine: &mut Machine<FakeHost, NoDevices>, on: bool| {
        machine.cpu.pstate.el = if on { 2 } else { 1 };
        let hcr = machine.cpu.read(SystemRegister::HCR_EL2) & !HCR_TGE;
        let hcr = if on { hcr | HCR_TGE } else { hcr };
        machine.cpu.registers.set(SystemRegister::HCR_EL2, 0, hcr);
        machine.controls_changed();
    };
    host(&mut machine, false);
    assert_eq!(machine.cpu.tlbi_current().regime, mmu::Regime::El10);
    let guest = machine.contexts(1, variant);
    host(&mut machine, true);
    let kept = machine.contexts(2, variant);
    machine.tlb_maintained(Reach::Nothing, Everywhere::All, None);
    assert_eq!(
        machine.statistics.itlb_sources[ItlbSource::AllForgot as usize],
        1
    );
    assert_eq!(machine.contexts(2, variant), kept);
    host(&mut machine, false);
    let renewed = machine.contexts(1, variant);
    assert!(renewed[0] != guest[0] && renewed[1] != guest[1]);
    host(&mut machine, true);
    assert_eq!(machine.contexts(2, variant), kept);
    machine.tlb_maintained(Reach::All, Everywhere::All, None);
    let after = machine.contexts(2, variant);
    assert!(after[0] != kept[0] && after[1] != kept[1]);
}

/// More translation input values than [`CONTEXT_FRAMES`] forget every
/// context, and both halves' contexts are then of the same generation:
/// neither names a context the other's frame forgot.
#[test]
fn a_full_frame_table_forgets_every_context_and_names_live_ones() {
    const MSR_MAIR_EL1_X0: u32 = 0xd518_a200;
    let mut machine = machine(&[]);
    let el = machine.cpu.pstate.el;
    let variant = machine.current_variant(false, false);
    let first = machine.contexts(el, variant);
    let others = machine.statistics.lookup_forgets[3];
    for mair in 1..=CONTEXT_FRAMES as u64 + 1 {
        machine.cpu.state.x[0] = mair;
        machine.system_instruction(MSR_MAIR_EL1_X0).unwrap();
        let contexts = machine.contexts(el, variant);
        for (spaces, context) in machine.spaces.iter().zip(contexts) {
            assert!(spaces.live(context).is_some());
        }
    }
    assert!(machine.statistics.lookup_forgets[3] > others);
    assert!(machine.spaces[0].live(first[0]).is_none());
    assert!(machine.context_frames.len() <= CONTEXT_FRAMES);
}

/// Host nanoseconds of an engine fetch that refills the ITLB after a TLBI
/// of everything, against one that finds its page, and of the walk alone.
#[test]
#[ignore = "manual: opt-in"]
fn itlb_refill_cost() {
    use std::time::Instant;
    const RET: u32 = 0xd65f_03c0;
    const PAGES: u64 = 8;
    let mut words = vec![0u32; (PAGES * 0x1000 / 4) as usize];
    for page in 0..PAGES {
        words[(page * 0x400) as usize] = RET;
    }
    let mut machine = machine(&words);
    let frames: Vec<u64> = (0..PAGES).map(|page| BASE + page * 0x1000).collect();
    paged_space(&mut machine, 0, &frames);
    let fetch = |machine: &mut Machine<FakeHost, NoDevices>, page: u64| {
        assert!(matches!(
            machine.fetch_block(BASE + page * 0x1000, false),
            Ok(Fetched::Block(_))
        ));
    };
    for page in 0..PAGES {
        fetch(&mut machine, page);
    }
    const ROUNDS: u32 = 20_000;
    let time = |machine: &mut Machine<FakeHost, NoDevices>, forget: bool| {
        let started = Instant::now();
        for _ in 0..ROUNDS {
            if forget {
                machine.tlb_maintained(Reach::All, Everywhere::All, None);
            }
            for page in 0..PAGES {
                fetch(machine, page);
            }
        }
        started.elapsed().as_nanos() as f64 / f64::from(ROUNDS) / PAGES as f64
    };
    let hit = time(&mut machine, false);
    let refill = time(&mut machine, true);
    let walk = {
        let started = Instant::now();
        for _ in 0..ROUNDS {
            machine.advance_translation_generation();
            for page in 0..PAGES {
                assert!(machine.fetch_physical(BASE + page * 0x1000).is_ok());
            }
        }
        started.elapsed().as_nanos() as f64 / f64::from(ROUNDS) / PAGES as f64
    };
    let forget = {
        let started = Instant::now();
        for _ in 0..ROUNDS * 8 {
            machine.tlb_maintained(Reach::All, Everywhere::All, None);
        }
        started.elapsed().as_nanos() as f64 / f64::from(ROUNDS * 8)
    };
    let variant = machine.current_variant(false, false);
    let el = machine.cpu.pstate.el;
    let keys: Vec<u64> = (0..PAGES)
        .map(|page| machine.current_key(BASE + page * 0x1000).expect("held"))
        .collect();
    let contexts = {
        let started = Instant::now();
        for _ in 0..ROUNDS {
            machine.tlb_maintained(Reach::All, Everywhere::All, None);
            let contexts = machine.contexts(el, variant);
            machine.chain.set_contexts(contexts);
        }
        started.elapsed().as_nanos() as f64 / f64::from(ROUNDS) - forget
    };
    let entries = {
        let started = Instant::now();
        for _ in 0..ROUNDS {
            machine.tlb_maintained(Reach::All, Everywhere::All, None);
            let contexts = machine.contexts(el, variant);
            machine.chain.set_contexts(contexts);
            for page in 0..PAGES {
                machine.translate_page(BASE + page * 0x1000, keys[page as usize]);
            }
        }
        (started.elapsed().as_nanos() as f64 / f64::from(ROUNDS) - forget - contexts) / PAGES as f64
    };
    println!(
        "fetch found {hit:.0} ns, refill {refill:.0} ns per page (incl. 1/{PAGES} of a forget of {forget:.0} ns and of contexts {contexts:.0} ns), walk {walk:.0} ns, ITLB fill {entries:.0} ns"
    );
}

/// After a TLBI of everything, the first fetch in a context made anew for
/// a forgotten context's key walks again the pages that context took and
/// gives the ITLB those that translate, now: a page unmapped since is left
/// to its fetch (no fault, no state change), and a page remapped since
/// takes its new frame's key, never the old one.
#[test]
fn contexts_made_anew_walk_again_their_predecessors_pages() {
    const RET: u32 = 0xd65f_03c0;
    const PAGES: u64 = 6;
    let mut words = vec![0u32; (PAGES * 0x1000 / 4) as usize];
    for page in 0..PAGES {
        words[(page * 0x400) as usize] = RET;
    }
    let mut machine = machine(&words);
    let frames: Vec<u64> = (0..PAGES).map(|page| BASE + page * 0x1000).collect();
    paged_space(&mut machine, 0, &frames);
    let pc = |page: u64| BASE + page * 0x1000;
    let fetch = |machine: &mut Machine<FakeHost, NoDevices>, page: u64| {
        assert!(matches!(
            machine.fetch_block(pc(page), false),
            Ok(Fetched::Block(_))
        ));
    };
    for page in 0..PAGES {
        fetch(&mut machine, page);
    }
    let keys: Vec<u64> = (0..PAGES)
        .map(|page| machine.current_key(pc(page)).expect("held"))
        .collect();
    let before = machine.chain.contexts();
    machine.tlb_maintained(Reach::All, Everywhere::All, None);
    // Page 4 is unmapped and page 5 maps page 3's frame since the TLBI.
    let [_, _, l3] = SPACES_TABLES[0];
    write_entry(&mut machine, l3 + 8 * 4, 0);
    write_entry(&mut machine, l3 + 8 * 5, page_descriptor(frames[3], false));
    let (esr, far, elr) = (
        machine.cpu.read(SystemRegister::ESR_EL1),
        machine.cpu.read(SystemRegister::FAR_EL1),
        machine.cpu.read(SystemRegister::ELR_EL1),
    );
    let misses = machine.statistics.itlb[0];
    fetch(&mut machine, 0);
    assert_ne!(machine.chain.contexts(), before);
    assert_eq!(machine.statistics.itlb[0], misses);
    assert_eq!(
        machine.statistics.itlb_sources[ItlbSource::Refilled as usize],
        5
    );
    for page in 0..4 {
        assert_eq!(machine.current_key(pc(page)), Some(keys[page as usize]));
    }
    assert_eq!(machine.current_key(pc(4)), None);
    assert_eq!(machine.current_key(pc(5)), Some(keys[3]));
    assert_ne!(keys[5], keys[3]);
    assert_eq!(
        (
            machine.cpu.read(SystemRegister::ESR_EL1),
            machine.cpu.read(SystemRegister::FAR_EL1),
            machine.cpu.read(SystemRegister::ELR_EL1),
            machine.cpu.pc,
        ),
        (esr, far, elr, machine.cpu.pc)
    );
    // The pages it found need no walk of their own.
    for page in 1..4 {
        fetch(&mut machine, page);
    }
    assert_eq!(machine.statistics.itlb[0], misses);
}

/// A TLBI by VA and ASID of a lower-half page reaches that ASID's entries
/// and, in every context, the entries whose walk ended in a global leaf,
/// as a global entry matches every ASID (Arm `TLBIMatch`). Another ASID's
/// non-global entry of the same page stays, even inside the span of global
/// pages (a process's own copy of a shared library page).
#[test]
fn a_tlbi_by_va_and_asid_reaches_other_asids_only_at_their_global_entries() {
    const LOW: u64 = 0x1_8000_0000;
    const HIGH: u64 = 0x2_8000_0000;
    const OWN: u64 = 0x2_0000_0000;
    let mut machine = machine(&[]);
    let el = machine.cpu.pstate.el;
    let ttbr0 = machine.cpu.read(SystemRegister::TTBR0_EL1);
    let mine = context_of(&mut machine, el, ttbr0 + 0x10_0000, 5)[0];
    let other = context_of(&mut machine, el, ttbr0 + 0x20_0000, 6)[0];
    let third = context_of(&mut machine, el, ttbr0 + 0x30_0000, 7)[0];
    machine.fetched_through(0, LOW, 0x4000, true);
    machine.fetched_through(0, HIGH, 0x4000, true);
    machine.fetched_through(0, OWN, 0x4000, false);
    let put_global = |machine: &mut Machine<FakeHost, NoDevices>, page: u64, context: u64| {
        let slot = machine.itlb.page_slot(page, context);
        let entry = chain::LookupEntry::global_page(page, context, 1);
        machine.itlb.insert(slot, entry);
    };
    for context in [mine, other] {
        put(&mut machine, OWN, context);
        put_global(&mut machine, LOW, context);
    }
    // The third process maps LOW privately (nG=1).
    put(&mut machine, LOW, third);
    let tlbi = |machine: &mut Machine<FakeHost, NoDevices>, page: u64, asid: Option<u16>| {
        let reach = Reach::Containing(page);
        machine.tlb_maintained(reach, Everywhere::Addresses(reach), asid);
    };
    tlbi(&mut machine, OWN, Some(5));
    assert!(!held(&machine, OWN, mine));
    assert!(held(&machine, OWN, other));
    tlbi(&mut machine, LOW, Some(5));
    assert!(!held(&machine, LOW, mine));
    assert!(!held(&machine, LOW, other));
    assert!(held(&machine, LOW, third));
    // Its own ASID reaches its private entry.
    tlbi(&mut machine, LOW, Some(7));
    assert!(!held(&machine, LOW, third));
    // A TLBI by VA of all ASIDs reaches every context's entry.
    put(&mut machine, OWN, mine);
    tlbi(&mut machine, OWN, None);
    assert!(!held(&machine, OWN, mine));
    assert!(!held(&machine, OWN, other));
    // The drops in every context, by why: two global entries, then two
    // entries a TLBI without an ASID reached; none of the upper half.
    let sources = |source: ItlbSource| machine.statistics.itlb_sources[source as usize];
    assert_eq!(sources(ItlbSource::DropEvery), 4);
    assert_eq!(sources(ItlbSource::DropGlobal), 2);
    assert_eq!(sources(ItlbSource::DropNoAsid), 2);
    assert_eq!(sources(ItlbSource::DropUpper), 0);
}

/// One walk of a page in a leaf larger than a page gives the ITLB every
/// page of its 16 KiB granule, as a fetch of each would take it, and no
/// page past the granule; a TLBI by VA of any of them reaches them all,
/// as the fetch recorded the whole leaf. A 4 KiB leaf gives only its page.
#[test]
fn a_fetch_takes_every_page_of_its_leaf_granule() {
    const RET: u32 = 0xd65f_03c0;
    const PAGES: u64 = 5;
    let mut words = vec![0u32; (PAGES * 0x1000 / 4) as usize];
    for page in 0..PAGES {
        words[(page * 0x400) as usize] = RET;
    }
    let pc = |page: u64| BASE + page * 0x1000;
    // BASE's GiB as one block.
    let mut machine = machine(&words);
    normal_memory(&mut machine, false);
    assert!(matches!(
        machine.fetch_block(pc(1), false),
        Ok(Fetched::Block(_))
    ));
    let key = machine.current_key(pc(1)).expect("the fetched page");
    for page in [0, 2, 3] {
        assert_eq!(
            machine.current_key(pc(page)),
            Some(key - 1 + page),
            "page {page}"
        );
    }
    assert_eq!(machine.current_key(pc(4)), None);
    let sources = |machine: &Machine<FakeHost, NoDevices>, source: ItlbSource| {
        machine.statistics.itlb_sources[source as usize]
    };
    assert_eq!(sources(&machine, ItlbSource::LeafFills), 3);
    // Their fetches need no walk: no ITLB miss.
    let misses = machine.statistics.itlb[0];
    for page in [0, 2, 3] {
        assert!(matches!(
            machine.fetch_block(pc(page), false),
            Ok(Fetched::Block(_))
        ));
    }
    assert_eq!(machine.statistics.itlb[0], misses);
    tlbi_page(&mut machine, pc(3));
    for page in 0..4 {
        assert_eq!(machine.current_key(pc(page)), None, "page {page}");
    }
    // 4 KiB pages: the fetched page alone.
    let mut machine = machine_with_pages(&words);
    assert!(matches!(
        machine.fetch_block(pc(1), false),
        Ok(Fetched::Block(_))
    ));
    assert!(machine.current_key(pc(1)).is_some());
    assert_eq!(machine.current_key(pc(0)), None);
    assert_eq!(machine.current_key(pc(2)), None);
    assert_eq!(sources(&machine, ItlbSource::LeafFills), 0);
}

/// A leaf's other pages are not logged as fills; after a TLBI of
/// everything, the refill of the page whose fetch took them takes them
/// again, so the next fetches of the leaf miss no more than before.
#[test]
fn a_whole_tlbi_refills_a_leaf_through_the_page_its_fetch_took() {
    use mmu::tlbi::{Everywhere, Reach};
    const RET: u32 = 0xd65f_03c0;
    let mut words = vec![0u32; (4 * 0x1000 / 4) as usize];
    for page in 0..4 {
        words[page * 0x400] = RET;
    }
    let pc = |page: u64| BASE + page * 0x1000;
    let mut machine = machine(&words);
    normal_memory(&mut machine, false);
    assert!(matches!(
        machine.fetch_block(pc(1), false),
        Ok(Fetched::Block(_))
    ));
    let logged = |machine: &Machine<FakeHost, NoDevices>| {
        machine
            .fill_log
            .iter()
            .filter(|&&(page, _)| page >> 14 == pc(0) >> 14)
            .count()
    };
    assert_eq!(logged(&machine), 1);
    machine.tlb_maintained(Reach::All, Everywhere::All, None);
    // The fetch of another page of the leaf finds it refilled: no miss.
    let misses = machine.statistics.itlb[0];
    assert!(matches!(
        machine.fetch_block(pc(2), false),
        Ok(Fetched::Block(_))
    ));
    for page in 0..4 {
        assert!(machine.current_key(pc(page)).is_some(), "page {page}");
    }
    assert_eq!(machine.statistics.itlb[0], misses);
}

/// After a TLBI of everything, the refill of the pages the forgotten context
/// took walks each from the leaf table the walk before it read, not from
/// the table base: one descriptor a page past the first. It takes what a
/// full walk of each now gives, as before the memo: the pages that still
/// translate come back with their keys, a page moved to another frame comes
/// back with that frame's, and a page unmapped or made non-executable
/// between the fills and the TLBI is not taken.
#[test]
fn a_whole_tlbi_refill_walks_from_the_leaf_table_and_takes_what_full_walks_give() {
    use mmu::tlbi::{Everywhere, Reach};
    const RET: u32 = 0xd65f_03c0;
    const PAGES: u64 = 48;
    const FRAMES: u64 = 8;
    let mut words = vec![0u32; (FRAMES * 0x1000 / 4) as usize];
    for frame in 0..FRAMES as usize {
        words[frame * 0x400] = RET;
    }
    let pc = |page: u64| BASE + page * 0x1000;
    let frame = |page: u64| BASE + (page % FRAMES) * 0x1000;
    let mut machine = machine(&words);
    let frames: Vec<u64> = (0..PAGES).map(frame).collect();
    paged_space(&mut machine, 0, &frames);
    let fetch = |machine: &mut Machine<FakeHost, NoDevices>, page| {
        assert!(matches!(
            machine.fetch_block(pc(page), false),
            Ok(Fetched::Block(_))
        ));
    };
    for page in 0..PAGES {
        fetch(&mut machine, page);
    }
    let keys: Vec<u64> = (0..PAGES)
        .map(|page| machine.current_key(pc(page)).expect("taken"))
        .collect();
    // Between the fills and the TLBI: page 5 unmapped, page 6 moved to the
    // frame page 7 maps, page 7 made privileged-execute-never.
    let [_, _, l3] = SPACES_TABLES[0];
    write_entry(&mut machine, l3 + 5 * 8, 0);
    write_entry(&mut machine, l3 + 6 * 8, page_descriptor(frame(7), false));
    write_entry(&mut machine, l3 + 7 * 8, page_descriptor(frame(7), true));
    machine.tlb_maintained(Reach::All, Everywhere::All, None);
    let context = machine.chain.contexts()[1];
    let reads = machine.host.descriptor_reads;
    fetch(&mut machine, 0);
    let reads = machine.host.descriptor_reads - reads;
    // The fetch runs in a context made anew.
    assert_ne!(machine.chain.contexts()[1], context);
    for page in 0..PAGES {
        let expected = match page {
            5 | 7 => None,
            6 => Some(keys[7]),
            _ => Some(keys[page as usize]),
        };
        assert_eq!(machine.current_key(pc(page)), expected, "page {page}");
    }
    // A full walk reads three descriptors (levels 1 to 3); the batch reads
    // them for its first page and the leaf alone for each one after.
    assert!(reads < 2 * PAGES as usize, "{reads} descriptor reads");
    assert!(reads >= PAGES as usize - 1, "{reads} descriptor reads");
}

/// The acceptance set for any change to what a TLBI of everything keeps
/// (the deep dive's "verify instead of forget", #1(c)): after the guest
/// rewrites its tables and issues one, every fetch runs what its walk now
/// gives, whatever the ITLB, the block table and the contexts held before.
/// Each scenario runs on a machine of its own, whose space 0 maps pages 0
/// to 3 at frames 0 to 3, private (nG) to ASID 1, each page fetched once:
///
/// - a page remapped to another frame runs that frame;
/// - a page unmapped faults;
/// - a page made privileged-execute-never faults;
/// - an address space whose TTBR0 value (table base and ASID) a new
///   process reuses, with other mappings, runs the new process's frame;
/// - of one VA in two address spaces, the one whose mapping changed runs
///   its new frame and the other keeps its own;
///
/// and in each the pages it did not change still run their frames. Every
/// scenario fails where the TLBI keeps a context's entries without checking
/// them, as removing its forgetting from `tlb_maintained` shows (all five).
/// With `--tlbi-keep on` (`tlbi_keep.rs`) they run in `tests/tlbi_keep.rs`,
/// with the check and under the mutation that keeps without verifying.
#[test]
fn after_a_whole_tlbi_every_fetch_runs_what_its_walk_now_gives() {
    let (failed, _) = whole_tlbi_acceptance(&|_| {});
    assert!(failed.is_empty(), "{failed:#?}");
}

/// The scenarios of [`after_a_whole_tlbi_every_fetch_runs_what_its_walk_now_gives`],
/// each on a machine `configure` sets up before its first fetch: what failed,
/// and what keeping the contexts did on all of them.
pub(super) fn whole_tlbi_acceptance(
    configure: &dyn Fn(&mut Machine<FakeHost, NoDevices>),
) -> (Vec<String>, TlbiKeepCounts) {
    use mmu::tlbi::{Everywhere, Reach};
    const RET: u32 = 0xd65f_03c0;
    const NG: u64 = 1 << 11;
    const MSR_TTBR0_EL1_X0: u32 = 0xd518_2000;
    const FRAMES: u64 = 4;
    type M = Machine<FakeHost, NoDevices>;
    let frame = |n: u64| BASE + n * 0x1000;
    let pc = |page: u64| BASE + page * 0x1000;
    let [_, _, l3] = SPACES_TABLES[0];
    let [_, _, l3_other] = SPACES_TABLES[1];
    let map = |machine: &mut M, l3: u64, page: u64, frame: u64| {
        write_entry(machine, l3 + page * 8, page_descriptor(frame, false) | NG);
    };
    let select = |machine: &mut M, space: usize, asid: u64| {
        let [l1, _, _] = SPACES_TABLES[space];
        machine.cpu.state.x[0] = (BASE + l1) | asid << 48;
        machine.system_instruction(MSR_TTBR0_EL1_X0).unwrap();
    };
    // What a fetch of `pc` runs: its page's key, or None for a fault.
    let run = |machine: &mut M, pc: u64| -> Option<u64> {
        match machine.fetch_block(pc, false) {
            Ok(Fetched::Block(_)) => Some(machine.current_key(pc).expect("taken")),
            Ok(Fetched::Exception(_)) => None,
            _ => panic!("{pc:#x}: neither a block nor an exception"),
        }
    };
    let all = |machine: &mut M| machine.tlb_maintained(Reach::All, Everywhere::All, None);
    // A machine with space 0 set up and run, and each frame's key.
    let start = || -> (M, Vec<u64>) {
        let mut words = vec![0u32; (FRAMES * 0x1000 / 4) as usize];
        for frame in 0..FRAMES as usize {
            words[frame * 0x400] = RET;
        }
        let mut machine = machine(&words);
        configure(&mut machine);
        paged_space(&mut machine, 0, &(0..FRAMES).map(frame).collect::<Vec<_>>());
        for page in 0..FRAMES {
            map(&mut machine, l3, page, frame(page));
        }
        select(&mut machine, 0, 1);
        let keys: Vec<u64> = (0..FRAMES)
            .map(|page| run(&mut machine, pc(page)).expect("mapped"))
            .collect();
        assert_eq!(
            keys.iter()
                .collect::<alloc::collections::BTreeSet<_>>()
                .len(),
            FRAMES as usize,
            "one key a frame"
        );
        (machine, keys)
    };
    // Each scenario: what each fetch it makes after the TLBI ran, and what
    // the walk then gives.
    type Check = (&'static str, Option<u64>, Option<u64>);
    type Scenario<'a> = &'a dyn Fn(&mut M, &[u64]) -> Vec<Check>;
    let scenarios: [(&str, Scenario); 5] = [
        ("remapped page", &|machine, keys| {
            map(machine, l3, 0, frame(1));
            all(machine);
            vec![
                ("page 0", run(machine, pc(0)), Some(keys[1])),
                ("page 3", run(machine, pc(3)), Some(keys[3])),
            ]
        }),
        ("unmapped page", &|machine, keys| {
            write_entry(machine, l3 + 8, 0);
            all(machine);
            vec![
                ("page 1", run(machine, pc(1)), None),
                ("page 3", run(machine, pc(3)), Some(keys[3])),
            ]
        }),
        ("execute-never page", &|machine, keys| {
            write_entry(machine, l3 + 2 * 8, page_descriptor(frame(2), true) | NG);
            all(machine);
            vec![
                ("page 2", run(machine, pc(2)), None),
                ("page 3", run(machine, pc(3)), Some(keys[3])),
            ]
        }),
        ("reused TTBR0 and ASID", &|machine, keys| {
            // The process goes; another runs (ASID 2); a new process takes
            // the first one's table base and ASID with page 3 at frame 0,
            // and the kernel flushes before it runs.
            select(machine, 0, 2);
            map(machine, l3, 3, frame(0));
            all(machine);
            select(machine, 0, 1);
            vec![
                ("page 3", run(machine, pc(3)), Some(keys[0])),
                ("page 1", run(machine, pc(1)), Some(keys[1])),
            ]
        }),
        ("one VA in two spaces", &|machine, keys| {
            // Space 1 (ASID 3) maps page 0 at frame 2, and runs it.
            paged_space(machine, 1, &[frame(2)]);
            map(machine, l3_other, 0, frame(2));
            select(machine, 1, 3);
            let before = run(machine, pc(0));
            // Then maps it at frame 3, and flushes.
            map(machine, l3_other, 0, frame(3));
            all(machine);
            let changed = run(machine, pc(0));
            select(machine, 0, 1);
            vec![
                ("space 1 before", before, Some(keys[2])),
                ("space 1 after", changed, Some(keys[3])),
                ("space 0", run(machine, pc(0)), Some(keys[0])),
            ]
        }),
    ];
    let mut failed = Vec::new();
    let mut kept = TlbiKeepCounts::default();
    for (name, scenario) in scenarios {
        let (mut machine, keys) = start();
        for (what, ran, expected) in scenario(&mut machine, &keys) {
            if ran != expected {
                failed.push(alloc::format!(
                    "{name}, {what}: ran {ran:x?}, its walk gives {expected:x?}"
                ));
            }
        }
        kept.add(&machine.statistics.tlbi_keep);
    }
    (failed, kept)
}

/// A machine running `words` at BASE through 4 KiB pages of address
/// space 0, page `n` at frame `BASE + n * 4 KiB`.
fn machine_with_pages(words: &[u32]) -> Machine<FakeHost, NoDevices> {
    let mut machine = machine(words);
    let frames: Vec<u64> = (0..words.len() as u64 / 0x400)
        .map(|page| BASE + page * 0x1000)
        .collect();
    paged_space(&mut machine, 0, &frames);
    machine
}

/// A lower-half page whose walk ended in a global leaf is kept in the
/// context every address space shares: another process finds it without a
/// walk, while a page another process maps privately (nG=1) stays its own.
/// A TLBI by VA and any ASID reaches the shared entry; a TLBI of an ASID
/// leaves it, as it leaves every global entry (Arm `TLBIMatch`).
#[test]
fn address_spaces_share_their_global_pages_entries() {
    const RET: u32 = 0xd65f_03c0;
    const PAGES: u64 = 2;
    const NG: u64 = 1 << 11;
    let mut words = vec![0u32; (PAGES * 0x1000 / 4) as usize];
    for page in 0..PAGES {
        words[(page * 0x400) as usize] = RET;
    }
    let pc = |page: u64| BASE + page * 0x1000;
    let frames: Vec<u64> = (0..PAGES).map(pc).collect();
    let mut machine = machine(&words);
    paged_space(&mut machine, 1, &frames);
    paged_space(&mut machine, 0, &frames);
    // Space 1 maps page 1 privately.
    let [_, _, l3] = SPACES_TABLES[1];
    write_entry(&mut machine, l3 + 8, page_descriptor(frames[1], false) | NG);
    let switch = |machine: &mut Machine<FakeHost, NoDevices>, space: usize, asid: u64| {
        let [l1, _, _] = SPACES_TABLES[space];
        machine.cpu.state.x[0] = (BASE + l1) | asid << 48;
        machine.system_instruction(MSR_TTBR0_EL1_X0).unwrap();
        let variant = machine.current_variant(false, false);
        let contexts = machine.contexts(machine.cpu.pstate.el, variant);
        machine.chain.set_contexts(contexts);
    };
    let fetch = |machine: &mut Machine<FakeHost, NoDevices>, page: u64| {
        assert!(matches!(
            machine.fetch_block(pc(page), false),
            Ok(Fetched::Block(_))
        ));
    };
    switch(&mut machine, 0, 5);
    fetch(&mut machine, 0);
    let shared = machine.current_key(pc(0)).expect("fetched");
    let sources = |machine: &Machine<FakeHost, NoDevices>, source: ItlbSource| {
        machine.statistics.itlb_sources[source as usize]
    };
    assert_eq!(sources(&machine, ItlbSource::SharedFills), 1);
    switch(&mut machine, 1, 6);
    assert_eq!(machine.current_key(pc(0)), Some(shared));
    // Process 1's private page is its own.
    fetch(&mut machine, 1);
    let private = machine.current_key(pc(1)).expect("fetched");
    assert_eq!(sources(&machine, ItlbSource::SharedFills), 1);
    switch(&mut machine, 0, 5);
    assert_eq!(machine.current_key(pc(1)), None);
    // A TLBI of ASID 5 leaves the shared entry; one by VA and ASID 6 takes
    // it in every space.
    machine.tlb_maintained(Reach::All, Everywhere::NonGlobal(5), Some(5));
    assert_eq!(machine.current_key(pc(0)), Some(shared));
    let reach = Reach::Containing(pc(0));
    machine.tlb_maintained(reach, Everywhere::Addresses(reach), Some(6));
    assert_eq!(machine.current_key(pc(0)), None);
    switch(&mut machine, 1, 6);
    assert_eq!(machine.current_key(pc(0)), None);
    assert_eq!(machine.current_key(pc(1)), Some(private));
    // A TLBI of everything takes both.
    fetch(&mut machine, 0);
    machine.tlb_maintained(Reach::All, Everywhere::All, None);
    let variant = machine.current_variant(false, false);
    let contexts = machine.contexts(machine.cpu.pstate.el, variant);
    machine.chain.set_contexts(contexts);
    machine.pending_refills.clear();
    assert_eq!(machine.current_key(pc(0)), None);
    assert_eq!(machine.current_key(pc(1)), None);
}

/// A machine of `pages` pages of RET at BASE, private (nG) pages of address
/// space 0 at their own frames, selected with ASID 1.
fn private_pages(pages: u64) -> Machine<FakeHost, NoDevices> {
    const RET: u32 = 0xd65f_03c0;
    const NG: u64 = 1 << 11;
    let mut words = vec![0u32; (pages * 0x1000 / 4) as usize];
    for page in 0..pages as usize {
        words[page * 0x400] = RET;
    }
    let mut machine = machine(&words);
    let frames: Vec<u64> = (0..pages).map(|page| BASE + page * 0x1000).collect();
    paged_space(&mut machine, 0, &frames);
    let [_, _, l3] = SPACES_TABLES[0];
    for (page, &frame) in frames.iter().enumerate() {
        write_entry(
            &mut machine,
            l3 + 8 * page as u64,
            page_descriptor(frame, false) | NG,
        );
    }
    select_space(&mut machine, 0, 1);
    machine
}

/// Select address space `space` with ASID `asid`.
fn select_space(machine: &mut Machine<FakeHost, NoDevices>, space: usize, asid: u64) {
    let [l1, _, _] = SPACES_TABLES[space];
    machine.cpu.state.x[0] = (BASE + l1) | asid << 48;
    machine.system_instruction(MSR_TTBR0_EL1_X0).unwrap();
}

/// Fetch the block of page `page` of BASE.
fn fetch_page(machine: &mut Machine<FakeHost, NoDevices>, page: u64) {
    assert!(matches!(
        machine.fetch_block(BASE + page * 0x1000, false),
        Ok(Fetched::Block(_))
    ));
}

/// Which of the first `pages` pages of BASE the ITLB holds now.
fn held_pages(machine: &Machine<FakeHost, NoDevices>, pages: u64) -> Vec<bool> {
    (0..pages)
        .map(|page| machine.current_key(BASE + page * 0x1000).is_some())
        .collect()
}

/// Only fetches' takes go in the fill log: a page a refill took after a
/// TLBI of everything is walked again after the next only when an engine
/// fetch reached it since, or while it was refilled fewer times in a row
/// unreached than the limit (one unless set). Each refilled entry a fetch
/// reaches counts as hit, each that none reached before its context went
/// as unhit, and each refill that walked pages as a batch. Logged instead,
/// every refilled page was walked again after every TLBI, used or not.
#[test]
fn a_refilled_page_no_fetch_reaches_is_walked_again_only_within_its_limit() {
    use mmu::tlbi::{Everywhere, Reach};
    const PAGES: u64 = 4;
    let sources = |machine: &Machine<FakeHost, NoDevices>, source: ItlbSource| {
        machine.statistics.itlb_sources[source as usize]
    };
    let all = |machine: &mut Machine<FakeHost, NoDevices>| {
        machine.tlb_maintained(Reach::All, Everywhere::All, None)
    };
    for unhit in [1, 2] {
        let mut machine = private_pages(PAGES);
        machine.set_refill_limits(REFILLS_PER_CONTEXT, unhit);
        for page in 0..PAGES {
            fetch_page(&mut machine, page);
        }
        all(&mut machine);
        // Page 0's fetch walks every page again and reaches its own; the
        // fetch of page 1 reaches page 1's.
        fetch_page(&mut machine, 0);
        assert_eq!(held_pages(&machine, PAGES), [true; 4], "unhit {unhit}");
        assert_eq!(sources(&machine, ItlbSource::Refilled), PAGES);
        assert_eq!(sources(&machine, ItlbSource::RefillBatches), 1);
        fetch_page(&mut machine, 1);
        assert_eq!(sources(&machine, ItlbSource::RefillHits), 2);
        assert_eq!(machine.refill_marks.len(), 2, "pages 2 and 3 unreached");
        all(&mut machine);
        assert_eq!(sources(&machine, ItlbSource::RefillUnhit), 2);
        assert!(machine.refill_marks.is_empty());
        fetch_page(&mut machine, 0);
        fetch_page(&mut machine, 1);
        let again = match unhit {
            1 => [true, true, false, false],
            _ => [true; 4],
        };
        assert_eq!(held_pages(&machine, PAGES), again, "unhit {unhit}");
        // Refilled twice in a row unreached, pages 2 and 3 wait for a fetch
        // at any limit of two.
        all(&mut machine);
        fetch_page(&mut machine, 0);
        assert_eq!(
            held_pages(&machine, PAGES),
            [true, true, false, false],
            "unhit {unhit}"
        );
        // A fetch takes page 3 again, and the next TLBI's refill walks it.
        fetch_page(&mut machine, 3);
        all(&mut machine);
        fetch_page(&mut machine, 0);
        assert!(held_pages(&machine, PAGES)[3]);
    }
}

/// XNU flushes everything at a switch between processes of different shared
/// regions. The list the switch away from a process left stays through the
/// flush of the next switch, so the switch back walks that process's pages
/// again at once (and counts the list as kept); each page is walked in the
/// tables as they are then, so one unmapped meanwhile is not taken and its
/// fetch faults. A list held through more TLBIs than [`PENDING_FLUSHES`]
/// goes, and a half keeps at most [`PENDING_LISTS`].
#[test]
fn a_process_finds_its_refills_after_another_process_flushed() {
    use mmu::tlbi::{Everywhere, Reach};
    const NG: u64 = 1 << 11;
    const PAGES: u64 = 4;
    let all = |machine: &mut Machine<FakeHost, NoDevices>| {
        machine.tlb_maintained(Reach::All, Everywhere::All, None)
    };
    let lists = |machine: &Machine<FakeHost, NoDevices>| {
        machine
            .pending_refills
            .keys()
            .filter(|(half, _)| *half == 0)
            .count()
    };
    let mut machine = private_pages(PAGES);
    for page in 0..PAGES {
        fetch_page(&mut machine, page);
    }
    // To process B (space 1, ASID 2), flushing; B runs its page 0.
    all(&mut machine);
    paged_space(&mut machine, 1, &[BASE]);
    let [_, _, l3_other] = SPACES_TABLES[1];
    write_entry(&mut machine, l3_other, page_descriptor(BASE, false) | NG);
    select_space(&mut machine, 1, 2);
    fetch_page(&mut machine, 0);
    // Meanwhile A's page 3 is unmapped. Back to A, flushing.
    let [_, _, l3] = SPACES_TABLES[0];
    write_entry(&mut machine, l3 + 3 * 8, 0);
    all(&mut machine);
    select_space(&mut machine, 0, 1);
    fetch_page(&mut machine, 0);
    assert_eq!(
        held_pages(&machine, PAGES),
        [true, true, true, false],
        "A's pages from its kept list, walked again"
    );
    assert_eq!(
        machine.statistics.itlb_sources[ItlbSource::RefillKept as usize],
        1
    );
    assert!(matches!(
        machine.fetch_block(BASE + 3 * 0x1000, false),
        Ok(Fetched::Exception(_))
    ));
    // B's list outlives PENDING_FLUSHES TLBIs untaken, then goes.
    assert_eq!(lists(&machine), 1, "B's");
    for _ in 0..PENDING_FLUSHES {
        all(&mut machine);
        fetch_page(&mut machine, 0);
    }
    let b = |machine: &Machine<FakeHost, NoDevices>| {
        machine
            .pending_refills
            .keys()
            .any(|(half, key)| *half == 0 && key.ttbr0 >> 48 == 2)
    };
    assert!(b(&machine), "kept through {PENDING_FLUSHES}");
    all(&mut machine);
    assert!(!b(&machine), "gone after {PENDING_FLUSHES}");
    // Processes past PENDING_LISTS between two TLBIs: the half keeps that
    // many lists.
    for asid in 10..10 + 2 * PENDING_LISTS as u64 {
        select_space(&mut machine, 0, asid);
        fetch_page(&mut machine, 0);
    }
    all(&mut machine);
    assert_eq!(lists(&machine), PENDING_LISTS);
    assert!(
        machine
            .pending_refills
            .values()
            .all(|pending| pending.pages.len() <= REFILLS_PER_CONTEXT)
    );
}
