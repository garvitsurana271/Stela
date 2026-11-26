//! `--share-aliases on` ([`super::super::share_aliases`]): the same frames
//! mapped at two VAs, as the shared cache is at two slides, run one compile
//! of each block. The fetch at the second VA takes an alias of the first's
//! block, with its PC, successors and fault rows moved to its page, and a
//! run that ends in shared code at another page base names that page's
//! alias. Off, each VA compiles its own (`shared_code.rs`).

use super::*;

const PAGE: u64 = 0x1000;
const PAGES: u64 = 2;
const BLOCKS: u64 = 8;
/// ldr x0, [x1]; adr x2, #0; nop; ret, again and again: blocks of every
/// suffix, with a fault site and a PC-relative address.
const LDR_X0_X1: u32 = 0xf940_0020;
const ADR_X2: u32 = 0x1000_0002;
const NOP: u32 = 0xd503_201f;
const RET: u32 = 0xd65f_03c0;
/// The second VA of the frames: 2 MiB up.
const MOVED: u64 = BASE + (1 << 21);

/// A machine whose address space maps the test RAM's first pages at BASE
/// and again at [`MOVED`], sharing translations across aliases or not.
fn two_mappings(share: bool) -> Machine<FakeHost, NoDevices> {
    let words: Vec<u32> = (0..PAGES * PAGE / 4)
        .map(|at| [LDR_X0_X1, ADR_X2, NOP, RET][at as usize % 4])
        .collect();
    let mut machine = machine(&words);
    machine.set_share_aliases(share);
    let frames: Vec<u64> = (0..PAGES).map(|page| BASE + page * PAGE).collect();
    paged_space(&mut machine, 1, &frames);
    let [_, l2, l3] = SPACES_TABLES[1];
    write_entry(&mut machine, l2 + 8, (BASE + l3) | 0b11);
    machine
}

/// The blocks a fetch of every block of the pages from `base` found.
fn fetch_all(machine: &mut Machine<FakeHost, NoDevices>, base: u64) -> Vec<usize> {
    let mut found = Vec::new();
    for page in 0..PAGES {
        for block in 0..BLOCKS {
            let pc = base + page * PAGE + 4 * block;
            let Ok(Fetched::Block(index)) = machine.fetch_block(pc, false) else {
                panic!("a block at {pc:#x}");
            };
            assert_eq!(machine.code.blocks[index].pc, pc);
            found.push(index);
        }
    }
    found
}

/// Blocks compiled so far: fetches that found no block.
fn compiled(machine: &Machine<FakeHost, NoDevices>) -> u64 {
    machine.statistics.fetches[4]
}

/// The PCs of block `index`'s fault rows.
fn row_pcs(machine: &Machine<FakeHost, NoDevices>, index: usize) -> Vec<u64> {
    machine
        .code
        .blocks
        .recovery()
        .get(&index)
        .unwrap()
        .test_rows()
        .iter()
        .map(|row| row.retirement.instruction_pc().0)
        .collect()
}

#[test]
fn the_aliases_of_one_frame_run_one_compile() {
    let mut machine = two_mappings(true);
    let owners = fetch_all(&mut machine, BASE);
    let count = PAGES * BLOCKS;
    assert_eq!(compiled(&machine), count);
    assert_eq!(machine.statistics.share_aliases.owners, count);
    assert!(
        owners
            .iter()
            .all(|&owner| machine.code.blocks[owner].marks.independent())
    );
    let aliases = fetch_all(&mut machine, MOVED);
    // Nothing more compiled or installed: each alias runs its owner's code.
    assert_eq!(compiled(&machine), count);
    assert_eq!(machine.statistics.share_aliases.shared_fetches, count);
    assert_eq!(machine.code.blocks.live() as u64, 2 * count);
    for (&owner, &alias) in owners.iter().zip(&aliases) {
        let (held, shared) = (&machine.code.blocks[owner], &machine.code.blocks[alias]);
        assert!(shared.marks.alias() && !shared.marks.independent());
        assert_eq!(shared.code, held.code);
        assert_eq!(shared.pc, held.pc + (MOVED - BASE));
        assert_eq!(shared.instructions, held.instructions);
        for (exit, moved) in held.successors.iter().zip(&shared.successors) {
            assert_eq!(moved.site, exit.site);
            assert_eq!(moved.target.0, exit.target.0 + (MOVED - BASE));
        }
        let moved: Vec<u64> = row_pcs(&machine, owner)
            .iter()
            .map(|pc| pc + (MOVED - BASE))
            .collect();
        assert_eq!(row_pcs(&machine, alias), moved);
        // Shared code links through its owner's record.
        assert_eq!(
            machine.link_owner(alias, shared.pc + 4),
            Some((owner, held.pc + 4))
        );
    }
    // Both run on: nothing is compiled again.
    assert_eq!(fetch_all(&mut machine, BASE), owners);
    assert_eq!(fetch_all(&mut machine, MOVED), aliases);
    assert_eq!(compiled(&machine), count);
}

#[test]
fn without_sharing_each_alias_compiles_its_own_blocks() {
    let mut machine = two_mappings(false);
    let owners = fetch_all(&mut machine, BASE);
    let others = fetch_all(&mut machine, MOVED);
    let count = PAGES * BLOCKS;
    assert_eq!(compiled(&machine), 2 * count);
    assert_eq!(
        machine.statistics.share_aliases,
        ShareAliasCounts::default()
    );
    for (&one, &other) in owners.iter().zip(&others) {
        let (one, other) = (&machine.code.blocks[one], &machine.code.blocks[other]);
        assert_ne!(one.code, other.code);
        assert!(!one.marks.shared() && !other.marks.shared());
    }
}

#[test]
fn a_run_ending_in_shared_code_at_another_page_names_its_alias() {
    let mut machine = two_mappings(true);
    let owner = fetch_all(&mut machine, BASE)[0];
    // A link another alias made: the run ended in the owner at MOVED's
    // page. Its alias there is made now, then found by the fetch.
    let alias = machine.shared_block(owner, MOVED & !(PAGE - 1)).unwrap();
    assert_ne!(alias, owner);
    assert_eq!(machine.code.blocks[alias].pc, MOVED);
    assert_eq!(
        machine.code.blocks[alias].code,
        machine.code.blocks[owner].code
    );
    let before = compiled(&machine);
    assert!(
        matches!(machine.fetch_block(MOVED, false), Ok(Fetched::Block(found)) if found == alias)
    );
    assert_eq!(compiled(&machine), before);
    // Again: the PE's memo of the last.
    assert_eq!(
        machine.shared_block(owner, MOVED & !(PAGE - 1)).unwrap(),
        alias
    );
    // The alias at the owner's page is the owner.
    assert_eq!(machine.shared_block(alias, BASE).unwrap(), owner);
    assert_eq!(machine.shared_block(owner, BASE).unwrap(), owner);
    let counts = machine.statistics.share_aliases;
    assert_eq!(
        (counts.made, counts.mapped, counts.checks, counts.mismatches),
        (1, 3, 1, 0)
    );
}

#[test]
fn code_that_is_not_position_independent_at_another_page_base_is_a_mismatch() {
    // MRS ends its block: the engine completes it through its address.
    const MRS_X0_SCTLR_EL1: u32 = 0xd538_1000;
    let mut machine = two_mappings(true);
    machine.host.ram[..4].copy_from_slice(&MRS_X0_SCTLR_EL1.to_le_bytes());
    let Ok(Fetched::Block(block)) = machine.fetch_block(BASE, false) else {
        panic!("a block");
    };
    assert!(!machine.code.blocks[block].marks.shared());
    assert_eq!(machine.shared_block(block, MOVED).unwrap(), block);
    assert_eq!(machine.statistics.share_aliases.mismatches, 1);
}

#[test]
fn an_ic_ivau_of_the_frame_takes_its_owner_and_aliases() {
    const IC_IVAU_X0: u32 = 0xd50b_7520;
    let mut machine = two_mappings(true);
    fetch_all(&mut machine, BASE);
    fetch_all(&mut machine, MOVED);
    machine.cpu.state.x[0] = MOVED + 0x40;
    machine.system_instruction(IC_IVAU_X0).unwrap();
    let left: Vec<u64> = (machine.code.index().sweep(None, usize::MAX).into_iter())
        .map(|(_, slot)| machine.code.blocks[slot].pc & !(PAGE - 1))
        .collect();
    // The first frame's blocks went at both addresses; the second's stay.
    assert!(
        left.iter()
            .all(|&page| page == BASE + PAGE || page == MOVED + PAGE)
    );
    assert_eq!(left.len() as u64, 2 * BLOCKS);
    // Compiled again at the fetch: once, for both addresses.
    let before = compiled(&machine);
    fetch_all(&mut machine, MOVED);
    fetch_all(&mut machine, BASE);
    assert_eq!(compiled(&machine) - before, BLOCKS);
}

#[test]
fn a_fault_in_an_alias_recovers_at_the_aliass_pc() {
    let mut machine = two_mappings(true);
    let (owner, offset) = native_fault_site_in_entry(
        &mut machine,
        BASE,
        BASE,
        stela_compiler::block::RecoverySiteKind::GuestAccess,
    );
    assert!(machine.code.blocks[owner].marks.independent());
    machine.cpu.pc = MOVED;
    machine.cpu.state.x[1] = 0x8000_0000_0000_0000;
    machine.host.exits.push(NativeExit::Fault(NativeFault {
        origin: crate::machine::NativeFaultOrigin::Exception,
        trap: 13,
        error_code: 0,
        spills: stela_compiler::block::HostSpillSnapshot::new(),
        registers: stela_compiler::block::HostGprSnapshot::legacy([0; 16]),
        offset,
        address: 0x8000_0000_0000_0000,
        write: false,
        cause: FaultCause::NonCanonical,
    }));
    machine.step().unwrap();
    assert_eq!(machine.cpu.read(SystemRegister::ELR_EL1), MOVED);
    assert_eq!(machine.statistics.share_aliases.shared_fetches, 1);
}
