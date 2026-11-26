//! The PE's memo of variant numbers (`Machine::variant_memo`) gives what the
//! board's registry gives, for more variants than it holds, and forgets the
//! numbers of a registry the machine no longer reads.

use super::*;

/// The fetch's variant with `n` in a field no other test variant uses.
fn numbered(machine: &mut Machine<FakeHost, NoDevices>, n: u16) -> Variant {
    let base = machine.current_variant(false, false);
    let system = stela_compiler::x86_64::NativeSystem {
        zero_block: Some(0x6000 + n),
        ..base.system
    };
    Variant { system, ..base }
}

#[test]
fn the_memo_gives_the_registry_s_numbers() {
    let mut machine = machine(&[]);
    let variants: Vec<Variant> = (0..3 * VARIANT_MEMO as u16)
        .map(|n| numbered(&mut machine, n))
        .collect();
    assert_eq!(machine.known_variant(variants[0]), None, "not numbered yet");
    let ids: Vec<VariantId> = variants
        .iter()
        .map(|&variant| machine.variant_id(variant))
        .collect();
    // In every order, through the memo and past it, each variant has the
    // registry's number, and a guarded page's variant its own.
    for round in 0..3 {
        for (at, &variant) in variants.iter().enumerate().rev().skip(round) {
            let registry = machine.variants.find(&variant).map(|at| VariantId(at as u32));
            assert_eq!(machine.known_variant(variant), Some(ids[at]));
            assert_eq!(registry, Some(ids[at]));
            assert_eq!(machine.variant_id(variant), ids[at]);
            let guarded = Variant {
                guarded_page: !variant.guarded_page,
                ..variant
            };
            assert_ne!(machine.variant_id(guarded), ids[at]);
            assert_eq!(
                machine.known_variant(guarded),
                machine.variants.find(&guarded).map(|at| VariantId(at as u32))
            );
        }
    }
}

#[test]
fn another_registry_forgets_the_memo() {
    let mut machine = machine(&[]);
    let first = numbered(&mut machine, 1);
    let second = numbered(&mut machine, 2);
    machine.variant_id(first);
    let id = machine.variant_id(second);
    assert_eq!(machine.known_variant(second), Some(id));
    // A registry where the second variant has another number, and the
    // first none.
    let other = Arc::new(numbering::Registry::new(numbering::VARIANTS));
    for n in 0..=id.0 as u16 {
        other.find_or_add(numbered(&mut machine, 100 + n), || true);
    }
    let renumbered = other.find_or_add(second, || true).unwrap();
    assert_ne!(VariantId(renumbered as u32), id);
    machine.variants = other;
    assert_eq!(machine.known_variant(first), None);
    assert_eq!(machine.known_variant(second), Some(VariantId(renumbered as u32)));
    assert_eq!(
        machine.variant_id(first),
        VariantId(renumbered as u32 + 1)
    );
}
