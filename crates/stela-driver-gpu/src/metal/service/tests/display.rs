use super::{MockBackend, MockCompiler, service};
use crate::metal::{
    binding::ComposeFormat,
    device::display::{DisplayFrame, DisplayLayer, DisplayPlane},
    error::Error,
    service::{ServiceHost, SharedService},
};
use alloc::vec;
use stela_contracts::gate::MemoryRange;

type Shared = SharedService<MockBackend, MockCompiler>;

const HOST: ServiceHost = ServiceHost {
    relax: || panic!("nothing waits past its bound"),
    relax_within: || false,
    stack: 0x4000,
    run: |_, call| call(),
    report: |_| {},
    note: |_| {},
    ticks: || 0,
    served: |_| {},
    wake: || {},
    bundle: |_| panic!("no kext reports"),
    job_waiting: || {},
};

fn frame() -> DisplayFrame {
    DisplayFrame {
        pitch: 64,
        width: 16,
        rows: 0..2,
        layers: vec![DisplayLayer {
            color: DisplayPlane {
                ranges: vec![MemoryRange {
                    address: 0x8000_0000,
                    bytes: 0x1000,
                }],
                origin: 0,
            },
            alpha: None,
            stride: 64,
            alpha_stride: 0,
            format: ComposeFormat::Bgra8,
            premultiplied: false,
            source: [0, 0, 16, 2],
            surface: 0,
            destination: [0, 0, 16, 2],
        }],
    }
}

#[test]
fn a_display_composition_completes_as_the_gpu_finishes_it() {
    let shared = Shared::new(service(), HOST);
    let target = shared.share_scanouts(&[(3, 128), (4, 128)]).unwrap();
    assert_eq!(target.len(), 2);
    let (serial, copied) = shared.compose_display(&target, 1, &frame()).unwrap();
    assert_eq!(copied, 0);
    assert_eq!(shared.display_composed(&target, serial), Some(false));
    {
        let mut turn = shared.try_turn().unwrap();
        turn.device().backend().finish();
    }
    // The GPU's interrupt, which the platform's waiter takes.
    shared.interrupted();
    assert_eq!(shared.display_composed(&target, serial), Some(true));
    assert!(matches!(
        shared.compose_display(&target, 2, &frame()),
        Err(Error::Invalid(_))
    ));
    shared.release_display(target);
}

/// A lost device takes none of a display's buffers, so the platform closes
/// them, and its generation stays the one the shared buffers were imported
/// in until a reopened device serves: the display shares again only then.
#[test]
fn a_lost_device_takes_no_scanout_buffer_and_keeps_its_generation() {
    let shared = Shared::new(service(), HOST);
    let generation = shared.generation();
    let target = shared.share_scanouts(&[(3, 128)]).unwrap();
    assert_eq!(target.epoch(), generation);
    let imported = {
        let mut turn = shared.try_turn().unwrap();
        turn.device().mark_lost();
        turn.device().backend().dma_bufs.len()
    };
    assert!(matches!(
        shared.share_scanouts(&[(4, 128), (5, 128)]),
        Err(Error::DeviceLost)
    ));
    let mut turn = shared.try_turn().unwrap();
    assert_eq!(turn.device().backend().dma_bufs.len(), imported);
    drop(turn);
    assert_eq!(shared.generation(), generation);
    assert!(matches!(
        shared.compose_display(&target, 0, &frame()),
        Err(Error::DeviceLost)
    ));
}

#[test]
fn a_busy_service_refuses_a_composition_past_the_wait_bound() {
    let shared = Shared::new(service(), HOST);
    let target = shared.share_scanouts(&[(3, 128)]).unwrap();
    shared.set_wait_bound(1);
    let held = shared.try_turn().unwrap();
    assert!(matches!(
        shared.compose_display(&target, 0, &frame()),
        Err(Error::Unsupported(_))
    ));
    assert_eq!(
        shared.display_composed(&target, 1),
        Some(false),
        "a held service is not waited for"
    );
    drop(held);
}
