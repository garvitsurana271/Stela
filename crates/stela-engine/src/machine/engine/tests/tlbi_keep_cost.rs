//! Host timing of a TLBI of everything with the contexts kept
//! (`--tlbi-keep on`, `tlbi_keep.rs`) against forgetting them, on a PE
//! shaped like boot 27's: about 1300 lower-half contexts (a few dozen busy
//! address spaces with large working sets, the rest small), the upper
//! context holding the shared region's global pages, which each address
//! space walks through its own tables, and a full ITLB. Each round is a
//! TLBI of everything followed by every busy address space fetching its
//! working set, as XNU runs them between flushes; a few of its descriptors
//! change each round. Prints, per TLBI, the wall time of the TLBI and of
//! the fetches after it, the ITLB misses (pages walked at a fetch), and
//! for the keep its verification's share. Manual:
//! `cargo test -p stela-engine --features test-support,std --release --lib
//! tlbi_keep_cost -- --ignored --nocapture` (`KEEP_ROUNDS`, default 200;
//! `KEEP_BUSY`, default 27; `KEEP_IDLE`, default 1270; `KEEP_CHANGES`,
//! the descriptors moved each round, default 4).

use super::reinstall_cost::number;
use super::*;
use std::time::Instant;

type M = Machine<FakeHost, NoDevices>;

const RET: u32 = 0xd65f_03c0;
const NG: u64 = 1 << 11;
const MSR_TTBR0_EL1_X0: u32 = 0xd518_2000;
/// RAM of the frames code runs from: private ones, then the shared
/// region's.
const FRAMES_AT: u64 = 0x100_0000;
const PRIVATE_FRAMES: u64 = 1024;
const SHARED_FRAMES: u64 = 256;
/// RAM of each address space's tables: a level 1, 2 and 3 table.
const TABLES_AT: u64 = 0x200_0000;
const TABLES_BYTES: u64 = 0x3000;
/// The first shared page's index in an address space's level 3 table.
const SHARED_PAGE: u64 = 256;
/// Pages a busy address space maps privately, of which it runs `BUSY_RUNS`
/// each round; pages an idle one maps (and ran once).
const BUSY_PAGES: u64 = 200;
const BUSY_RUNS: u64 = 60;
const IDLE_PAGES: u64 = 6;
/// Shared pages each address space maps, of which a busy one runs
/// `SHARED_RUNS` each round.
const SHARED_PAGES: u64 = 128;
const SHARED_RUNS: u64 = 24;

fn tables(space: u64) -> [u64; 3] {
    let at = TABLES_AT + space * TABLES_BYTES;
    [at, at + 0x1000, at + 0x2000]
}

fn pc(page: u64) -> u64 {
    BASE + page * 0x1000
}

fn private_frame(space: u64, page: u64) -> u64 {
    BASE + FRAMES_AT + ((space * 37 + page) % PRIVATE_FRAMES) * 0x1000
}

fn shared_frame(page: u64) -> u64 {
    BASE + FRAMES_AT + (PRIVATE_FRAMES + page % SHARED_FRAMES) * 0x1000
}

/// Address space `space`: `pages` private pages and the shared region.
fn build(machine: &mut M, space: u64, pages: u64) {
    let [l1, l2, l3] = tables(space);
    write_entry(machine, l1 + (BASE >> 30) * 8, (BASE + l2) | 0b11);
    write_entry(machine, l2, (BASE + l3) | 0b11);
    for page in 0..pages {
        let descriptor = page_descriptor(private_frame(space, page), false) | NG;
        write_entry(machine, l3 + page * 8, descriptor);
    }
    for page in 0..SHARED_PAGES {
        let descriptor = page_descriptor(shared_frame(page), false);
        write_entry(machine, l3 + (SHARED_PAGE + page) * 8, descriptor);
    }
}

fn select(machine: &mut M, space: u64) {
    let [l1, _, _] = tables(space);
    let asid = space % 255 + 1;
    machine.cpu.state.x[0] = (BASE + l1) | asid << 48;
    machine.system_instruction(MSR_TTBR0_EL1_X0).unwrap();
}

fn run(machine: &mut M, page: u64) {
    match machine.fetch_block(pc(page), false) {
        Ok(Fetched::Block(_)) => {}
        _ => panic!("{page}: not a block"),
    }
}

fn all(machine: &mut M) {
    use mmu::tlbi::{Everywhere, Reach};
    machine.tlb_maintained(Reach::All, Everywhere::All, None);
}

/// One mode's totals over the measured rounds.
#[derive(Debug, Default)]
struct Totals {
    tlbi_ns: u128,
    fetch_ns: u128,
    misses: u64,
    reads: usize,
    refill_ticks: u64,
    keep: TlbiKeepCounts,
}

fn measure(keep: bool, busy: u64, idle: u64, rounds: u64, changes: u64) -> Totals {
    let bytes = (TABLES_AT + (busy + idle) * TABLES_BYTES) as usize;
    let mut machine = machine(&[]);
    machine.host.ram.resize(bytes, 0);
    for frame in 0..PRIVATE_FRAMES + SHARED_FRAMES {
        let at = (FRAMES_AT + frame * 0x1000) as usize;
        machine.host.ram[at..at + 4].copy_from_slice(&RET.to_le_bytes());
    }
    machine.host.real_counter = true;
    machine.set_tlbi_keep(keep, false);
    normal_memory(&mut machine, false);
    for space in 0..busy + idle {
        let pages = if space < busy { BUSY_PAGES } else { IDLE_PAGES };
        build(&mut machine, space, pages);
    }
    // Every space runs its pages once: the ITLB fills, the idle spaces'
    // contexts stay behind.
    for space in (0..busy + idle).rev() {
        select(&mut machine, space);
        let pages = if space < busy { BUSY_PAGES } else { IDLE_PAGES };
        for page in 0..pages {
            run(&mut machine, page);
        }
        for page in 0..SHARED_PAGES.min(if space < busy { SHARED_PAGES } else { 4 }) {
            run(&mut machine, SHARED_PAGE + page);
        }
    }
    let mut draws = 0x9e37_79b9_7f4a_7c15u64;
    let mut draw = |below: u64| {
        draws ^= draws << 13;
        draws ^= draws >> 7;
        draws ^= draws << 17;
        draws % below
    };
    let mut totals = Totals::default();
    let before = (
        machine.statistics.itlb[0],
        machine.host.descriptor_reads,
        machine.statistics.refill_ticks,
        machine.statistics.tlbi_keep,
    );
    for round in 0..rounds {
        // A few descriptors of the busy spaces move to another frame.
        for _ in 0..changes {
            let space = draw(busy);
            let page = draw(BUSY_PAGES);
            let [_, _, l3] = tables(space);
            let frame = private_frame(space, page + 1 + draw(8));
            write_entry(
                &mut machine,
                l3 + page * 8,
                page_descriptor(frame, false) | NG,
            );
        }
        let started = Instant::now();
        all(&mut machine);
        let flushed = Instant::now();
        for space in 0..busy {
            select(&mut machine, (space + round) % busy);
            let first = draw(BUSY_PAGES - BUSY_RUNS);
            for page in first..first + BUSY_RUNS {
                run(&mut machine, page);
            }
            let first = draw(SHARED_PAGES - SHARED_RUNS);
            for page in first..first + SHARED_RUNS {
                run(&mut machine, SHARED_PAGE + page);
            }
        }
        let done = Instant::now();
        totals.tlbi_ns += (flushed - started).as_nanos();
        totals.fetch_ns += (done - flushed).as_nanos();
    }
    totals.misses = machine.statistics.itlb[0] - before.0;
    totals.reads = machine.host.descriptor_reads - before.1;
    totals.refill_ticks = machine.statistics.refill_ticks - before.2;
    let mut keep = machine.statistics.tlbi_keep;
    let earlier = before.3;
    keep.tlbis -= earlier.tlbis;
    keep.contexts -= earlier.contexts;
    keep.verified -= earlier.verified;
    keep.forgotten -= earlier.forgotten;
    keep.kept -= earlier.kept;
    keep.dropped -= earlier.dropped;
    keep.ticks -= earlier.ticks;
    totals.keep = keep;
    totals
}

/// The TSC's ticks per nanosecond, from a short wall-clock sample.
fn tsc_per_ns() -> f64 {
    let (wall, tsc) = (Instant::now(), super::super::board::timestamp());
    while wall.elapsed().as_millis() < 50 {}
    let ticks = super::super::board::timestamp() - tsc;
    ticks as f64 / wall.elapsed().as_nanos() as f64
}

#[test]
#[ignore = "manual: opt-in"]
fn tlbi_keep_cost() {
    let rounds = number("KEEP_ROUNDS", 200) as u64;
    let busy = number("KEEP_BUSY", 27) as u64;
    let idle = number("KEEP_IDLE", 1270) as u64;
    let changes = number("KEEP_CHANGES", 4) as u64;
    let per_ns = tsc_per_ns();
    for keep in [false, true] {
        let totals = measure(keep, busy, idle, rounds, changes);
        let each = |ns: u128| ns as f64 / rounds as f64 / 1000.0;
        let counts = &totals.keep;
        println!(
            "bench keep={keep} rounds={rounds} busy={busy} idle={idle} changes={changes}: per TLBI \
             tlbi_us={:.1} fetch_us={:.1} total_us={:.1} misses={:.1} reads={:.1} \
             refill_us={:.1} verify_us={:.1} contexts={:.1} verified={:.1} kept={:.1} \
             dropped={:.1}",
            each(totals.tlbi_ns),
            each(totals.fetch_ns),
            each(totals.tlbi_ns + totals.fetch_ns),
            totals.misses as f64 / rounds as f64,
            totals.reads as f64 / rounds as f64,
            totals.refill_ticks as f64 / per_ns / rounds as f64 / 1000.0,
            counts.ticks as f64 / per_ns / rounds as f64 / 1000.0,
            counts.contexts as f64 / rounds as f64,
            counts.verified as f64 / rounds as f64,
            counts.kept as f64 / rounds as f64,
            counts.dropped as f64 / rounds as f64,
        );
    }
}
