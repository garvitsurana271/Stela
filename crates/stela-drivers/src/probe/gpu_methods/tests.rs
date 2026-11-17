//! The real ACPICA over a DSDT with a hybrid laptop's AMD graphics methods.
use super::*;
use crate::platform::acpi::{
    Mode,
    test_support::{TEST_SERIAL, dsdt_fixture},
};
use alloc::{string::String, vec, vec::Vec};

/// A package: `op`, its length (which counts itself), then `body`.
fn package(op: &[u8], body: Vec<u8>) -> Vec<u8> {
    let length = if body.len() + 1 < 0x40 {
        vec![(body.len() + 1) as u8]
    } else {
        let total = body.len() + 2;
        assert!(total < 1 << 12);
        vec![0x40 | (total & 0xf) as u8, (total >> 4) as u8]
    };
    [op.to_vec(), length, body].concat()
}
fn device(name: &[u8; 4], body: Vec<Vec<u8>>) -> Vec<u8> {
    package(&[0x5b, 0x82], [name.to_vec(), body.concat()].concat())
}
fn method(name: &[u8; 4], arguments: u8, body: Vec<Vec<u8>>) -> Vec<u8> {
    package(
        &[0x14],
        [name.to_vec(), vec![arguments], body.concat()].concat(),
    )
}
/// `Buffer (size) { bytes }`, the size as a word.
fn buffer(size: u16, bytes: &[u8]) -> Vec<u8> {
    package(
        &[0x11],
        [vec![0x0b], size.to_le_bytes().to_vec(), bytes.to_vec()].concat(),
    )
}
fn ret(value: Vec<u8>) -> Vec<u8> {
    [vec![0xa4], value].concat()
}
/// `If (predicate) { body }`.
fn when(predicate: Vec<u8>, body: Vec<Vec<u8>>) -> Vec<u8> {
    package(&[0xa0], [predicate, body.concat()].concat())
}
/// `LLess (Arg0, value)` and `LEqual (Arg0, value)`, the value as a word.
fn arg0_below(value: u16) -> Vec<u8> {
    [vec![0x95, 0x68, 0x0b], value.to_le_bytes().to_vec()].concat()
}
fn arg0_is(value: u16) -> Vec<u8> {
    [vec![0x93, 0x68, 0x0b], value.to_le_bytes().to_vec()].concat()
}

fn adr(value: u32) -> Vec<u8> {
    [
        vec![0x08],
        b"_ADR".to_vec(),
        vec![0x0c],
        value.to_le_bytes().to_vec(),
    ]
    .concat()
}
/// An `ATPX` that supports power control and keeps its last power
/// argument in `kept`.
fn atpx(kept: &[u8; 4]) -> Vec<u8> {
    method(
        b"ATPX",
        2,
        vec![
            when(arg0_is(0), vec![ret(buffer(8, &[8, 0, 1, 0, 2, 0, 0, 0]))]),
            when(arg0_is(2), vec![[vec![0x70, 0x69], kept.to_vec()].concat()]),
            ret(buffer(2, &[0, 0])),
        ],
    )
}
fn kept(name: &[u8; 4]) -> Vec<u8> {
    [vec![0x08], name.to_vec(), buffer(3, &[0, 0, 0])].concat()
}

/// `\_SB.PCI0` (a PCI host bridge) with `GFX0` at 00:02.0, whose `ATRM`
/// returns full pieces below 8 KiB and a short one there and which holds
/// `ATPX` and `ATIF`, and the port `RP01` at 00:01.0, whose child `PEGP`
/// holds an `ATPX` of its own. `\_SB.GHST`, no PCI function's companion,
/// holds `ATPX` and `ATCS`.
fn dsdt() -> Vec<u8> {
    let atrm = method(
        b"ATRM",
        2,
        vec![
            when(arg0_below(0x2000), vec![ret(buffer(0x1000, &[0x55, 0xaa]))]),
            ret(buffer(0x10, &[0x01])),
        ],
    );
    let gfx = device(
        b"GFX0",
        vec![
            adr(0x0002_0000),
            atrm,
            kept(b"PWRB"),
            atpx(b"PWRB"),
            method(b"ATIF", 2, vec![ret(buffer(2, &[0, 0]))]),
        ],
    );
    let port = device(
        b"RP01",
        vec![
            adr(0x0001_0000),
            device(b"PEGP", vec![adr(0), kept(b"PWRC"), atpx(b"PWRC")]),
        ],
    );
    let host = device(
        b"PCI0",
        vec![
            [
                vec![0x08],
                b"_HID".to_vec(),
                vec![0x0c],
                0x080a_d041u32.to_le_bytes().to_vec(),
            ]
            .concat(),
            port,
            gfx,
        ],
    );
    let ghost = device(
        b"GHST",
        vec![
            kept(b"PWRG"),
            atpx(b"PWRG"),
            method(b"ATCS", 2, vec![ret(buffer(2, &[0, 0]))]),
        ],
    );
    package(&[0x10], [b"\\_SB_".to_vec(), host, ghost].concat())
}

fn session() -> Session {
    let services = dsdt_fixture(&dsdt());
    // SAFETY: the serialized fixture hands its whole provider to the one
    // ACPICA session, which outlives every handle below.
    unsafe { Session::start(services, Mode::InterpreterOnly) }.unwrap()
}

const GFX: Address = Address {
    segment: 0,
    bus: 0,
    device: 2,
    function: 0,
};
const PORT: Address = Address {
    segment: 0,
    bus: 0,
    device: 1,
    function: 0,
};

fn adapter(address: Address, class: [u8; 3]) -> Adapter {
    Adapter {
        address,
        vendor: crate::gpu::Vendor::Amd,
        device: 0x1638,
        class,
    }
}

/// The census's companions of `adapters`.
fn companions(session: &Session, adapters: &[Adapter]) -> Vec<(Address, String)> {
    adapters
        .iter()
        .filter_map(|adapter| {
            let device = companion(session, adapter.address, &[])?;
            Some((adapter.address, device.path().ok()?))
        })
        .collect()
}

/// A scan of bus 0 with the port at 00:01.0, nothing below it, and the
/// display function at 00:02.0 when `display`.
fn topology(display: bool) -> Topology {
    Topology::scan(&[(0, 0)], |address, offset| {
        let words: [u32; 4] = if address == PORT {
            [0x1234_1022, 0x0604_0000, 0x0001_0000, 0x0001_0100]
        } else if address == GFX && display {
            [0x1638_1002, 0x0300_0000, 0, 0]
        } else {
            return None;
        };
        Some(match offset {
            0 => words[0],
            8 => words[1],
            0x0c => words[2],
            0x18 => words[3],
            _ => 0,
        })
    })
}

fn power_kept(session: &Session, path: &str) -> Vec<u8> {
    match session.handle(path).unwrap().evaluate(None, &[]).unwrap() {
        Some(Value::Buffer(bytes)) => bytes,
        other => panic!("{other:?}"),
    }
}

#[test]
fn each_unserved_method_is_found_on_a_display_functions_companion() {
    let _serial = TEST_SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let session = session();
    let adapters = [adapter(GFX, [3, 0, 0])];
    let companions = companions(&session, &adapters);
    let found = find(&session, &adapters, &companions);
    // ATCS is only on a device that is no display function's companion.
    let names: Vec<_> = found.iter().map(|(method, _)| *method).collect();
    assert_eq!(names, ["ATPX", "ATIF"]);
    assert!(found.iter().all(|(_, path)| path.ends_with("GFX0")));
    // amdgpu asks no 3D controller's companion, and none without one.
    let controller = [adapter(GFX, [3, 2, 0])];
    assert_eq!(find(&session, &controller, &companions), []);
    assert_eq!(find(&session, &adapters, &[]), []);
}

#[test]
fn atrm_is_read_in_pieces_up_to_the_first_short_one() {
    let _serial = TEST_SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let session = session();
    let audio = adapter(Address { function: 1, ..GFX }, [3, 2, 0]);
    let gfx = adapter(GFX, [3, 0, 0]);
    let companions = companions(&session, &[gfx]);
    // Only a display function's companion is asked.
    assert_eq!(atrm(&session, &[audio], &companions), None);
    let read = atrm(&session, &[audio, gfx], &companions).unwrap();
    assert_eq!(read.owner, GFX);
    assert!(read.path.ends_with("GFX0"), "{}", read.path);
    assert_eq!(read.image.len(), 2 * ATRM_PAGE + 0x10);
    assert_eq!(read.image[..2], [0x55, 0xaa]);
    assert_eq!(read.image[2 * ATRM_PAGE], 0x01);
}

#[test]
fn atpx_powers_the_discrete_gpu_on_where_it_controls_its_power() {
    let _serial = TEST_SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let session = session();
    // The shown display function's companion holds it, as amdgpu takes it.
    let powered = atpx_power_on(&session, &topology(true), &[PORT]).unwrap();
    let AtpxPower::On(path) = powered else {
        panic!("{powered:?}");
    };
    assert!(path.ends_with("GFX0"), "{path}");
    assert_eq!(power_kept(&session, "\\_SB.PCI0.GFX0.PWRB"), [3, 0, 1]);
    assert_eq!(power_kept(&session, "\\_SB.PCI0.RP01.PEGP.PWRC"), [0, 0, 0]);
}

#[test]
fn atpx_of_the_gpu_off_the_bus_is_found_below_its_empty_port() {
    let _serial = TEST_SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let session = session();
    // No display function answers: the one below the empty port holds it.
    let powered = atpx_power_on(&session, &topology(false), &[PORT]).unwrap();
    let AtpxPower::On(path) = powered else {
        panic!("{powered:?}");
    };
    assert!(path.ends_with("PEGP"), "{path}");
    assert_eq!(power_kept(&session, "\\_SB.PCI0.RP01.PEGP.PWRC"), [3, 0, 1]);
    // A device that is neither is never called, though it holds ATPX.
    assert_eq!(
        atpx_power_on(&session, &topology(false), &[]).unwrap(),
        AtpxPower::Absent
    );
    assert_eq!(power_kept(&session, "\\_SB.GHST.PWRG"), [0, 0, 0]);
    assert_eq!(power_kept(&session, "\\_SB.PCI0.GFX0.PWRB"), [0, 0, 0]);
}
