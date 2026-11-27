use super::*;
use crate::scanout::{PixelFormat, Surface};
use alloc::boxed::Box;
use alloc::vec;

const SCREEN: (u32, u32) = (2560, 1600);
const TSC_HZ: u64 = 1_000_000_000;

fn shared() -> &'static Shared {
    let shared = Box::leak(Box::new(Shared::new()));
    shared.enable();
    shared.set_screen(SCREEN.0, SCREEN.1);
    shared
}

/// The guest's pointer: a `size` square surface whose `source` shows at
/// `destination`'s origin, unscaled.
fn pointer(size: u32, source: (u32, u32, u32, u32), at: (u32, u32)) -> Layer {
    Layer {
        surface: Surface {
            width: size,
            height: size,
            stride: size * 4,
            format: PixelFormat::Bgra8888,
            iova: 0x8000,
        },
        source: Rect {
            x: source.0,
            y: source.1,
            width: source.2,
            height: source.3,
        },
        destination: Rect {
            x: at.0,
            y: at.1,
            width: source.2,
            height: source.3,
        },
        premultiplied: true,
        id: 7,
    }
}

/// The guest's 128 by 128 pointer with its whole surface at `origin`.
fn at(origin: (u32, u32)) -> Layer {
    pointer(128, (0, 0, 128, 128), origin)
}

#[test]
fn a_tablet_value_lands_in_its_pixel_whichever_scaling_the_guest_uses() {
    for size in [4u32, 1366, 1600, 1920, 2560, 3840] {
        for at in 0..size as i32 {
            let value = tablet(at, size);
            assert_eq!(pixel(value, size), at, "{at} of {size}");
            let by_max = u64::from(value) * u64::from(size) / u64::from(POINTER_MAX);
            assert_eq!(by_max, at as u64, "{at} of {size} scaled by the maximum");
        }
    }
    assert_eq!(tablet(-5, 100), tablet(0, 100));
    assert_eq!(tablet(500, 100), tablet(99, 100));
}

#[test]
fn without_the_host_cursor_or_a_screen_inputs_pass_unchanged() {
    let off = Box::leak(Box::new(Shared::new()));
    let mut pointer = Pointer::default();
    let inputs = vec![Input::Motion { dx: 3, dy: -2 }, Input::MouseButtons(1)];
    let mut read = inputs.clone();
    pointer.convert(off, &mut read, 1, TSC_HZ);
    assert_eq!(read, inputs);
    off.enable();
    pointer.convert(off, &mut read, 2, TSC_HZ);
    assert_eq!(read, inputs);
    assert_eq!(off.given(), 0);
}

#[test]
fn motion_becomes_one_absolute_move_per_run_and_buttons_go_with_the_tablet() {
    let shared = shared();
    let mut pointer = Pointer::default();
    assert_eq!(shared.position(), (1280, 800));
    let mut inputs = vec![
        Input::Motion { dx: 3, dy: 0 },
        Input::Motion { dx: 2, dy: -4 },
        Input::MouseButtons(0b0_1001),
        Input::Motion { dx: -1, dy: 0 },
        Input::MouseWheel(1),
    ];
    pointer.convert(shared, &mut inputs, 1, TSC_HZ);
    // Slow motion moves a pixel a count.
    assert_eq!(
        inputs,
        [
            Input::Move {
                x: tablet(1285, 2560),
                y: tablet(796, 1600)
            },
            Input::Buttons(0b001),
            Input::MouseButtons(0b0_1000),
            Input::Move {
                x: tablet(1284, 2560),
                y: tablet(796, 1600)
            },
            Input::MouseWheel(1),
        ]
    );
    assert_eq!(shared.recent(8), [(1284, 796), (1285, 796)]);
    // An unchanged tablet button and side button give no report again.
    let mut again = vec![Input::MouseButtons(0b0_1001)];
    pointer.convert(shared, &mut again, 2, TSC_HZ);
    assert!(again.is_empty());
    let mut released = vec![Input::MouseButtons(0)];
    pointer.convert(shared, &mut released, 3, TSC_HZ);
    assert_eq!(released, [Input::Buttons(0), Input::MouseButtons(0)]);
}

#[test]
fn the_pointer_stops_at_the_screen_edges_and_a_host_tablet_sets_it() {
    let shared = shared();
    let mut pointer = Pointer::default();
    let mut inputs = vec![Input::Motion {
        dx: -32000,
        dy: 32000,
    }];
    pointer.convert(shared, &mut inputs, 1, TSC_HZ);
    assert_eq!(shared.position(), (0, 1599));
    let mut inputs = vec![Input::Move {
        x: POINTER_MAX,
        y: 0,
    }];
    pointer.convert(shared, &mut inputs, 2, TSC_HZ);
    assert_eq!(shared.position(), (2559, 0));
    assert_eq!(
        inputs,
        [Input::Move {
            x: tablet(2559, 2560),
            y: tablet(0, 1600)
        }]
    );
}

#[test]
fn fast_motion_gains_up_to_four_pixels_a_count() {
    let shared = shared();
    let mut pointer = Pointer::default();
    // One count a millisecond or slower: a pixel a count.
    let mut slow = vec![Input::Motion { dx: 1, dy: 0 }];
    pointer.convert(shared, &mut slow, TSC_HZ, TSC_HZ);
    let mut slow = vec![Input::Motion { dx: 1, dy: 0 }];
    pointer.convert(shared, &mut slow, TSC_HZ + TSC_HZ / 1000, TSC_HZ);
    assert_eq!(shared.position(), (1282, 800));
    // Five counts a millisecond: three pixels a count.
    let mut faster = vec![Input::Motion { dx: 10, dy: 0 }];
    pointer.convert(shared, &mut faster, TSC_HZ + 3 * TSC_HZ / 1000, TSC_HZ);
    assert_eq!(shared.position(), (1312, 800));
    // Far faster: four.
    let mut fastest = vec![Input::Motion { dx: 100, dy: 0 }];
    pointer.convert(shared, &mut fastest, TSC_HZ + 4 * TSC_HZ / 1000, TSC_HZ);
    assert_eq!(shared.position(), (1712, 800));
}

/// Give the guest each position, as the input service does.
fn give(shared: &Shared, positions: &[(i32, i32)]) {
    for &(x, y) in positions {
        shared.place(x, y);
    }
}

#[test]
fn a_still_pointer_finds_the_hotspot_and_then_shows_at_the_hosts_position() {
    let shared = shared();
    let mut tracker = Tracker::new(shared);
    give(shared, &[(500, 400)]);
    tracker.tick(10);
    // The guest caught up: its pointer's origin is the position less the
    // hotspot (8, 6).
    tracker.observe(&at((492, 394)), SCREEN, 10 + STILL_TICKS);
    assert!(tracker.locked());
    give(shared, &[(510, 420)]);
    let shown = tracker.place(&at((492, 394)), SCREEN).unwrap();
    assert_eq!(
        (shown.source, shown.destination),
        (
            Rect {
                x: 0,
                y: 0,
                width: 128,
                height: 128
            },
            Rect {
                x: 502,
                y: 414,
                width: 128,
                height: 128
            }
        )
    );
    assert_eq!(shown.surface, at((0, 0)).surface);
    let counts = shared.take();
    assert_eq!(
        (counts.locks, counts.locked, counts.hotspot),
        (1, true, (8, 6))
    );
}

#[test]
fn a_pointer_past_the_top_left_shows_the_part_on_the_screen() {
    let shared = shared();
    let mut tracker = Tracker::new(shared);
    give(shared, &[(300, 300)]);
    tracker.tick(0);
    tracker.observe(&at((290, 280)), SCREEN, STILL_TICKS + 1);
    assert!(tracker.locked());
    give(shared, &[(4, 2)]);
    let shown = tracker.place(&at((290, 280)), SCREEN).unwrap();
    assert_eq!(
        (shown.source, shown.destination),
        (
            Rect {
                x: 6,
                y: 18,
                width: 122,
                height: 110
            },
            Rect {
                x: 0,
                y: 0,
                width: 122,
                height: 110
            }
        )
    );
    // And past the bottom right, the part on the screen.
    give(shared, &[(2559, 1599)]);
    let shown = tracker.place(&at((290, 280)), SCREEN).unwrap();
    assert_eq!(
        shown.destination,
        Rect {
            x: 2549,
            y: 1579,
            width: 11,
            height: 21
        }
    );
}

#[test]
fn a_placement_the_guest_clipped_at_an_edge_is_read_as_its_whole_surface() {
    let shared = shared();
    let mut tracker = Tracker::new(shared);
    give(shared, &[(3, 50)]);
    tracker.tick(0);
    // The pointer at x 3 with hotspot (10, 20): the guest leaves out the
    // seven columns left of the screen.
    let clipped = pointer(128, (7, 0, 121, 128), (0, 30));
    tracker.observe(&clipped, SCREEN, STILL_TICKS + 1);
    assert_eq!(shared.take().hotspot, (10, 20));
}

#[test]
fn a_lagging_guest_keeps_the_hotspot_and_its_distance_is_counted() {
    let shared = shared();
    let mut tracker = Tracker::new(shared);
    give(shared, &[(500, 400)]);
    tracker.tick(0);
    tracker.observe(&at((492, 394)), SCREEN, STILL_TICKS + 1);
    shared.take();
    let path: Vec<(i32, i32)> = (1..=40).map(|step| (500 + 3 * step, 400 + step)).collect();
    give(shared, &path);
    // The guest shows the tenth position while the host is at the fortieth.
    tracker.observe(&at((530 - 8, 410 - 6)), SCREEN, STILL_TICKS + 2);
    assert!(tracker.locked());
    let counts = shared.take();
    assert_eq!(
        (counts.guest_moves, counts.diverged_samples, counts.unlocks),
        (1, 1, 0)
    );
    // From (530, 410) to (620, 440).
    assert_eq!(counts.diverged_most, 94);
}

#[test]
fn moving_placements_that_share_one_candidate_set_the_hotspot() {
    let shared = shared();
    let mut tracker = Tracker::new(shared);
    let hotspot = (12, 9);
    // A curve: no two of its points stand the same way apart as another two.
    let path: Vec<(i32, i32)> = (0..60)
        .map(|step| (800 + 4 * step, 600 + step * step / 8))
        .collect();
    give(shared, &path[..20]);
    let origin = |at: (i32, i32)| ((at.0 - hotspot.0) as u32, (at.1 - hotspot.1) as u32);
    tracker.observe(&at(origin(path[12])), SCREEN, 1);
    assert!(!tracker.locked());
    give(shared, &path[20..]);
    tracker.observe(&at(origin(path[41])), SCREEN, 2);
    assert!(tracker.locked());
    assert_eq!(shared.take().hotspot, hotspot);
}

#[test]
fn a_straight_line_leaves_the_hotspot_open_until_the_path_turns() {
    let shared = shared();
    let mut tracker = Tracker::new(shared);
    let hotspot = (12, 9);
    let origin = |at: (i32, i32)| ((at.0 - hotspot.0) as u32, (at.1 - hotspot.1) as u32);
    let line: Vec<(i32, i32)> = (0..30).map(|step| (800 + 2 * step, 600)).collect();
    give(shared, &line);
    tracker.observe(&at(origin(line[10])), SCREEN, 1);
    tracker.observe(&at(origin(line[20])), SCREEN, 2);
    assert!(!tracker.locked());
    let turn: Vec<(i32, i32)> = (1..30).map(|step| (858, 600 + 2 * step)).collect();
    give(shared, &turn);
    tracker.observe(&at(origin(turn[15])), SCREEN, 3);
    assert!(tracker.locked());
    assert_eq!(shared.take().hotspot, hotspot);
}

#[test]
fn a_new_shape_at_rest_takes_its_own_hotspot() {
    let shared = shared();
    let mut tracker = Tracker::new(shared);
    give(shared, &[(500, 400)]);
    tracker.tick(0);
    tracker.observe(&at((492, 394)), SCREEN, STILL_TICKS + 1);
    // An I-beam: its hotspot is its middle.
    tracker.observe(&at((436, 336)), SCREEN, STILL_TICKS + 2);
    assert!(tracker.locked());
    let counts = shared.take();
    assert_eq!(
        (counts.hotspot, counts.shapes, counts.locks, counts.unlocks),
        ((64, 64), 1, 2, 1)
    );
}

#[test]
fn a_move_the_guest_made_itself_becomes_the_hosts_position() {
    let shared = shared();
    let mut tracker = Tracker::new(shared);
    give(shared, &[(500, 400)]);
    tracker.tick(0);
    tracker.observe(&at((492, 394)), SCREEN, STILL_TICKS + 1);
    // An app warps the pointer to (1500, 900).
    tracker.observe(&at((1492, 894)), SCREEN, 2 * STILL_TICKS + 2);
    assert!(tracker.locked());
    assert_eq!(shared.position(), (1500, 900));
    let counts = shared.take();
    assert_eq!((counts.warps, counts.hotspot), (1, (8, 6)));
}

#[test]
fn a_layer_that_is_no_pointer_shows_as_the_guest_placed_it() {
    let shared = shared();
    let mut tracker = Tracker::new(shared);
    give(shared, &[(500, 400)]);
    tracker.tick(0);
    tracker.observe(&at((492, 394)), SCREEN, STILL_TICKS + 1);
    assert!(tracker.locked());
    // A video in layer 1: large, and scaled.
    let mut video = pointer(1024, (0, 0, 1024, 576), (100, 100));
    assert_eq!(tracker.place(&video, SCREEN), None);
    video.surface.width = 64;
    video.surface.height = 64;
    video.source = Rect {
        x: 0,
        y: 0,
        width: 64,
        height: 64,
    };
    assert_eq!(tracker.place(&video, SCREEN), None);
    tracker.observe(&video, SCREEN, STILL_TICKS + 2);
    assert!(!tracker.locked());
    // A sprite cut from the middle of its surface is not a whole pointer.
    let sprite = pointer(128, (32, 32, 32, 32), (600, 600));
    assert_eq!(Shape::of(&sprite, SCREEN), None);
}
