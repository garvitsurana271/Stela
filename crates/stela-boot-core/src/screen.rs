//! Geometry, colours and text the mount's screens compute before drawing.
use alloc::{format, string::String, vec::Vec};
use core::{
    cell::UnsafeCell,
    sync::atomic::{AtomicBool, AtomicU8, Ordering},
};

const IDLE: u8 = 0;
const ACTIVE: u8 = 1;
const RETIRED: u8 = 2;

/// Exclusive, nonblocking admission for a framebuffer shared by copied views.
/// The actual writer must finish its CPU stores before dropping its permit.
pub struct Access(AtomicU8);

impl Access {
    pub const fn new() -> Self {
        Self(AtomicU8::new(IDLE))
    }

    pub fn enter(&self) -> Option<Permit<'_>> {
        self.0
            .compare_exchange(IDLE, ACTIVE, Ordering::Acquire, Ordering::Relaxed)
            .ok()
            .map(|_| Permit { gate: self })
    }

    pub fn is_retired(&self) -> bool {
        self.0.load(Ordering::Acquire) == RETIRED
    }

    /// Close admission only after the current writer has acknowledged return.
    pub fn retire(&self) -> bool {
        matches!(
            self.0
                .compare_exchange(IDLE, RETIRED, Ordering::AcqRel, Ordering::Acquire),
            Ok(_) | Err(RETIRED)
        )
    }

    /// Open admission again after [`Self::retire`], for an owner that
    /// retired the framebuffer and then never touched the device showing it:
    /// the picture is still scanned out. False if it was not retired.
    pub fn restore(&self) -> bool {
        self.0
            .compare_exchange(RETIRED, IDLE, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
}

impl Default for Access {
    fn default() -> Self {
        Self::new()
    }
}

pub struct Permit<'a> {
    gate: &'a Access,
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        self.gate.0.store(IDLE, Ordering::Release);
    }
}

/// The screen's text state, which any processor may reach: the boot PE's
/// reports, and the host drivers' waits on whichever processor runs them.
/// One at a time: a marker tick that finds it in use skips its frame, as a
/// firmware timer tick that interrupts a draw does, while a report waits a
/// bounded time for another processor's turn to end. A `RefCell` is not
/// enough, as two processors updating its borrow count at once can leave
/// it counting a borrow nobody holds, which freezes the screen for the
/// rest of the boot.
pub struct Exclusive<T> {
    busy: AtomicBool,
    value: UnsafeCell<T>,
}

// SAFETY: `busy` admits one processor at a time to `value`, which moves
// between them only through `with_until`'s exclusive borrow.
unsafe impl<T: Send> Sync for Exclusive<T> {}

impl<T> Exclusive<T> {
    pub const fn new(value: T) -> Self {
        Self {
            busy: AtomicBool::new(false),
            value: UnsafeCell::new(value),
        }
    }

    /// Run `f` on the value unless another processor, or this one further
    /// up its stack, has it now.
    pub fn try_with<R>(&self, f: impl FnOnce(&mut T) -> R) -> Option<R> {
        self.with_until(|| true, f)
    }

    /// Run `f` on the value once it is free, asking `give_up` after each
    /// failed try whether to stop waiting: the holder may be this processor
    /// further up its stack, which never lets it go while this one waits.
    pub fn with_until<R>(
        &self,
        mut give_up: impl FnMut() -> bool,
        f: impl FnOnce(&mut T) -> R,
    ) -> Option<R> {
        struct Release<'a>(&'a AtomicBool);
        impl Drop for Release<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Release);
            }
        }
        while self
            .busy
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            if give_up() {
                return None;
            }
            core::hint::spin_loop();
        }
        let _release = Release(&self.busy);
        // SAFETY: `busy` was false and is now ours until `_release` drops,
        // so this is the only reference to the value.
        Some(f(unsafe { &mut *self.value.get() }))
    }
}

/// Screen pixels per page unit: pages are laid out for about 720 rows.
pub fn scale(rows: usize) -> usize {
    (rows / 720).clamp(1, 4)
}

/// The first of `count` rows to show so that `selected` is among `fits`.
pub fn window(selected: usize, count: usize, fits: usize) -> usize {
    selected
        .saturating_sub(fits - 1)
        .min(count.saturating_sub(fits))
}

/// `completed` of `total` as a percentage rounded down, so 100 needs
/// completion; `None` for an unknown or inconsistent total. Wide
/// intermediates keep full-width byte counts exact.
pub fn percentage(completed: u64, total: u64) -> Option<u8> {
    if total == 0 || completed > total {
        return None;
    }
    let completed = u128::from(completed);
    let total = u128::from(total);
    Some((completed * 100 / total) as u8)
}

const GB: u64 = 1_000_000_000;

/// `bytes` as people read disk sizes: decimal units, three figures.
pub fn disk_size(bytes: u64) -> String {
    let tenths = |unit: u64| (bytes * 10 + unit / 2) / unit;
    match bytes {
        b if b >= 1000 * GB => {
            let t = tenths(1000 * GB);
            format!("{}.{} TB", t / 10, t % 10)
        }
        b if b >= 10 * GB => format!("{} GB", (b + GB / 2) / GB),
        b if b >= GB => {
            let t = tenths(GB);
            format!("{}.{} GB", t / 10, t % 10)
        }
        b => format!("{} MB", (b + 500_000) / 1_000_000),
    }
}

/// Draw the cells of a text grid that differ from what the screen shows,
/// in order, while `more` allows: `draw` paints cell `index` as `cells`
/// holds it, and `shown` then records it. Whether every cell is shown now;
/// when not, the next call goes on where this one stopped, so a caller with
/// a time budget spreads a whole-screen change over several calls.
pub fn present_changed<C: Copy + PartialEq>(
    cells: &[C],
    shown: &mut [C],
    mut draw: impl FnMut(usize, C),
    mut more: impl FnMut() -> bool,
) -> bool {
    for (index, (&cell, seen)) in cells.iter().zip(shown.iter_mut()).enumerate() {
        if cell == *seen {
            continue;
        }
        if !more() {
            return false;
        }
        draw(index, cell);
        *seen = cell;
    }
    true
}

/// The logo's sample (`x`, `y`) of a `side` by `side` grid, taken at the
/// sample's centre: Stambha's pillar on a rounded indigo tile, the shapes
/// and colours of `identity/icon/stambha-icon.svg` in integers. `None`
/// outside the tile's rounded corners.
pub fn logo_pixel(x: u32, y: u32, side: u32) -> Option<u32> {
    // The icon's 1024-unit square in tenths of a unit, as its shapes are
    // written: the sample's centre, rounded down.
    let at = |i: u32| (2 * i64::from(i) + 1) * i64::from(SQUARE / 2) / i64::from(side.max(1));
    let (x, y) = (at(x), at(y));
    let square = i64::from(SQUARE);
    if !(0..square).contains(&x) || !(0..square).contains(&y) {
        return None;
    }
    // Outside a rounded corner: radius 0.2 of the square.
    let radius = square / 5;
    let (dx, dy) = (
        x - x.clamp(radius, square - radius),
        y - y.clamp(radius, square - radius),
    );
    if dx * dx + dy * dy > radius * radius {
        return None;
    }
    let colour = if pillar(x, y) {
        // Gold to saffron, over the pillar's height.
        mix(0xffd27a, 0xf07a28, y - 1178, 8704 - 1178)
    } else {
        mix(0x2b3a8c, 0x141a3d, y, square)
    };
    Some(match polygon(&SHINE, x, y) {
        // The highlight, #FFF1C9 at 45% over what is under it.
        true => mix(colour, 0xfff1c9, 45, 100),
        false => colour,
    })
}

/// Samples a side of each pixel of [`logo_image`]: four by four.
const LOGO_SAMPLES: usize = 4;

/// The logo `side` pixels square, row by row, as the boot screen shows it:
/// each pixel the mean of [`LOGO_SAMPLES`] by [`LOGO_SAMPLES`] samples of
/// [`logo_pixel`], `background` outside the tile's rounded corners.
pub fn logo_image(side: usize, background: u32) -> Vec<u32> {
    let mut pixels = Vec::with_capacity(side * side);
    let samples = (side * LOGO_SAMPLES) as u32;
    let n = (LOGO_SAMPLES * LOGO_SAMPLES) as u32;
    for py in 0..side {
        for px in 0..side {
            let mut sum = [0u32; 3];
            for sy in 0..LOGO_SAMPLES {
                for sx in 0..LOGO_SAMPLES {
                    let colour = logo_pixel(
                        (px * LOGO_SAMPLES + sx) as u32,
                        (py * LOGO_SAMPLES + sy) as u32,
                        samples,
                    )
                    .unwrap_or(background);
                    for (channel, total) in sum.iter_mut().enumerate() {
                        *total += (colour >> (16 - 8 * channel)) & 0xff;
                    }
                }
            }
            pixels.push(
                sum.iter()
                    .fold(0, |pixel, total| (pixel << 8) | ((total + n / 2) / n)),
            );
        }
    }
    pixels
}

/// [`logo_image`] made once for the size and background it is shown at.
/// Each sample walks the pillar's outlines, 16 a pixel, which took 14 to
/// 24 ms a logo at 4K; the header is drawn at one size a screen, again on
/// every key press of the menu, the Wi-Fi page and the disk picker.
#[derive(Debug, Default)]
pub struct LogoCache {
    image: Option<(usize, u32, Vec<u32>)>,
    /// Images made, for the tests.
    made: u32,
}

impl LogoCache {
    pub const fn new() -> Self {
        Self {
            image: None,
            made: 0,
        }
    }

    /// The logo `side` pixels square over `background`, made only when the
    /// last one asked for was of another size or background.
    pub fn get(&mut self, side: usize, background: u32) -> &[u32] {
        let made = &mut self.made;
        let image = self
            .image
            .take_if(|(s, b, _)| (*s, *b) == (side, background))
            .unwrap_or_else(|| {
                *made += 1;
                (side, background, logo_image(side, background))
            });
        &self.image.insert(image).2
    }
}

/// The icon's square in tenths of its 1024 units.
const SQUARE: u32 = 10240;

/// Whether (`x`, `y`), in tenths of the icon's units, is on the pillar: its
/// dome, abacus, bell and shaft, and two base steps.
fn pillar(x: i64, y: i64) -> bool {
    let rect = |left: i64, top: i64, right: i64, bottom: i64| {
        (left..right).contains(&x) && (top..bottom).contains(&y)
    };
    // The dome: the top half of an ellipse 87 by 56.3 units about (512, 169).
    let (rx, ry) = (870, 563);
    let (dx, dy) = (x - 5120, y - 1690);
    let dome = dy <= 0 && (dx * ry) * (dx * ry) + (dy * rx) * (dy * rx) <= (rx * ry) * (rx * ry);
    dome || rect(2918, 1690, 7321, 2509)
        || polygon(&BODY, x, y)
        || rect(3430, 7762, 6809, 8192)
        || rect(2816, 8192, 7424, 8704)
}

/// Whether (`x`, `y`) is inside `points`, by the even-odd rule, in exact
/// integer arithmetic.
fn polygon(points: &[(i16, i16)], x: i64, y: i64) -> bool {
    let mut inside = false;
    let mut previous = points[points.len() - 1];
    for &point in points {
        let (xi, yi) = (i64::from(point.0), i64::from(point.1));
        let (xj, yj) = (i64::from(previous.0), i64::from(previous.1));
        if (yi > y) != (yj > y) {
            // Left of the edge's crossing at row `y`, with both sides
            // multiplied by the edge's height to stay in integers.
            let (left, crossing) = ((x - xi) * (yj - yi), (y - yi) * (xj - xi));
            if (yj > yi && left < crossing) || (yj < yi && left > crossing) {
                inside = !inside;
            }
        }
        previous = point;
    }
    inside
}

/// The bell and shaft, one outline: the bell's two curves to the abacus
/// above it, the shaft's tapered sides down to the base.
#[rustfmt::skip]
const BODY: [(i16, i16); 64] = [
    (6810, 2642), (6715, 2686), (6656, 2731), (6606, 2776), (6561, 2820), (6519, 2865),
    (6479, 2909), (6441, 2954), (6405, 2998), (6370, 3043), (6337, 3087), (6304, 3132),
    (6272, 3176), (6242, 3221), (6211, 3266), (6182, 3310), (6153, 3355), (6124, 3399),
    (6096, 3444), (6069, 3488), (6042, 3533), (5960, 3533), (5978, 3942), (5997, 4352),
    (6015, 4762), (6033, 5171), (6052, 5581), (6070, 5990), (6089, 6400), (6107, 6810),
    (6126, 7219), (6144, 7629), (4096, 7629), (4114, 7219), (4133, 6810), (4151, 6400),
    (4170, 5990), (4188, 5581), (4207, 5171), (4225, 4762), (4243, 4352), (4262, 3942),
    (4280, 3533), (4198, 3533), (4171, 3488), (4144, 3444), (4116, 3399), (4087, 3355),
    (4058, 3310), (4029, 3266), (3998, 3221), (3968, 3176), (3936, 3132), (3903, 3087),
    (3870, 3043), (3835, 2998), (3799, 2954), (3761, 2909), (3721, 2865), (3679, 2820),
    (3634, 2776), (3584, 2731), (3525, 2686), (3430, 2642),
];

/// The pale stripe down the left of the bell and shaft.
#[rustfmt::skip]
const SHINE: [(i16, i16); 82] = [
    (3684, 2642), (3849, 2767), (3952, 2891), (4039, 3016), (4119, 3141), (4192, 3265),
    (4261, 3390), (4327, 3515), (4402, 3639), (4397, 3764), (4393, 3889), (4388, 4013),
    (4383, 4138), (4378, 4263), (4374, 4387), (4369, 4512), (4364, 4637), (4359, 4761),
    (4355, 4886), (4350, 5011), (4345, 5135), (4340, 5260), (4335, 5385), (4331, 5509),
    (4326, 5634), (4321, 5759), (4316, 5883), (4312, 6008), (4307, 6133), (4302, 6257),
    (4297, 6382), (4293, 6507), (4288, 6631), (4283, 6756), (4278, 6881), (4273, 7005),
    (4269, 7130), (4264, 7255), (4259, 7379), (4254, 7504), (4250, 7629), (4659, 7629),
    (4662, 7504), (4664, 7379), (4667, 7255), (4669, 7130), (4672, 7005), (4674, 6881),
    (4677, 6756), (4679, 6631), (4682, 6507), (4684, 6382), (4687, 6257), (4689, 6133),
    (4692, 6008), (4695, 5883), (4697, 5759), (4700, 5634), (4702, 5509), (4705, 5385),
    (4707, 5260), (4710, 5135), (4712, 5011), (4715, 4886), (4717, 4761), (4720, 4637),
    (4722, 4512), (4725, 4387), (4727, 4263), (4730, 4138), (4732, 4013), (4735, 3889),
    (4737, 3764), (4740, 3639), (4700, 3515), (4665, 3390), (4629, 3265), (4590, 3141),
    (4548, 3016), (4501, 2891), (4447, 2767), (4360, 2642),
];

/// `from` blended toward `to` by `part` of `whole`, clamped to the two, with
/// each channel rounded to nearest.
fn mix(from: u32, to: u32, part: i64, whole: i64) -> u32 {
    let whole = whole.max(1);
    let part = part.clamp(0, whole);
    (0..3).fold(0, |pixel, channel| {
        let shift = 16 - 8 * channel;
        let (a, b) = (
            i64::from((from >> shift) & 0xff),
            i64::from((to >> shift) & 0xff),
        );
        (pixel << 8) | ((a * (whole - part) + b * part + whole / 2) / whole) as u32
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bounded_present_draws_changed_cells_in_steps_and_resumes() {
        let cells = [1, 2, 3, 4, 5, 6];
        let mut shown = [1, 0, 3, 0, 0, 0];
        let mut drawn = alloc::vec::Vec::new();
        // Two cells' budget: the first two changed cells, then a stop.
        let mut budget = 2;
        let done = present_changed(
            &cells,
            &mut shown,
            |index, _| drawn.push(index),
            || {
                budget -= 1;
                budget >= 0
            },
        );
        assert!(!done);
        assert_eq!(drawn, [1, 3]);
        assert_eq!(shown, [1, 2, 3, 4, 0, 0]);
        // The next call goes on with the rest and draws nothing twice.
        let done = present_changed(&cells, &mut shown, |index, _| drawn.push(index), || true);
        assert!(done);
        assert_eq!(drawn, [1, 3, 4, 5]);
        assert_eq!(shown, cells);
        // Nothing changed: nothing drawn, done at once.
        assert!(present_changed(
            &cells,
            &mut shown,
            |_, _| panic!("drawn"),
            || false
        ));
    }

    #[test]
    fn retirement_requires_the_writer_to_return_and_permanently_closes_admission() {
        let access = Access::new();
        let writer = access.enter().unwrap();
        assert!(access.enter().is_none());
        assert!(!access.retire());
        assert!(!access.is_retired());
        drop(writer);
        assert!(access.retire());
        assert!(access.is_retired());
        assert!(access.enter().is_none());
        assert!(access.retire());
    }

    #[test]
    fn a_retired_framebuffer_restored_admits_writers_again() {
        let access = Access::new();
        // Nothing to restore before a retirement.
        assert!(!access.restore());
        assert!(access.retire());
        assert!(access.restore());
        assert!(!access.is_retired());
        let writer = access.enter().unwrap();
        // A restore never takes over a writer's admission.
        assert!(!access.restore());
        drop(writer);
        assert!(access.retire());
    }

    #[test]
    fn exclusive_state_skips_a_nested_change_and_is_free_again_after_one() {
        let state = Exclusive::new(0u32);
        let nested = state.try_with(|value| {
            *value += 1;
            state.try_with(|value| *value += 10)
        });
        assert_eq!(nested, Some(None));
        assert_eq!(state.try_with(|value| *value), Some(1));
    }

    /// A report waits out another processor's turn instead of skipping,
    /// and gives up on a holder that never lets go.
    #[test]
    fn exclusive_state_waits_for_a_turn_and_gives_up_on_a_stuck_holder() {
        use std::{sync::Arc, sync::mpsc, thread};
        let state = Arc::new(Exclusive::new(0u32));
        let (taken, take) = mpsc::channel();
        let (release, released) = mpsc::channel::<()>();
        let holder = {
            let state = Arc::clone(&state);
            thread::spawn(move || {
                state.try_with(|value| {
                    taken.send(()).unwrap();
                    released.recv().unwrap();
                    *value += 1;
                })
            })
        };
        take.recv().unwrap();
        assert_eq!(state.try_with(|value| *value), None);
        let mut tries = 0;
        let stuck = state.with_until(
            || {
                tries += 1;
                tries > 3
            },
            |value| *value,
        );
        assert_eq!((stuck, tries), (None, 4));
        let mut release = Some(release);
        let seen = state.with_until(
            || {
                if let Some(release) = release.take() {
                    release.send(()).unwrap();
                }
                false
            },
            |value| *value,
        );
        assert_eq!(seen, Some(1));
        assert_eq!(holder.join().unwrap(), Some(()));
    }

    /// Processors that update the state at once never lose it: every
    /// change either lands or is skipped, and the state is free after all
    /// of them, where a `RefCell`'s count could stay borrowed for good.
    #[test]
    fn exclusive_state_stays_usable_after_processors_race_for_it() {
        use std::{sync::Arc, thread, vec::Vec};
        let state = Arc::new(Exclusive::new(0u64));
        let landed: u64 = (0..8)
            .map(|_| {
                let state = Arc::clone(&state);
                thread::spawn(move || {
                    (0..20_000)
                        .filter(|_| state.try_with(|value| *value += 1).is_some())
                        .count() as u64
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .sum();
        assert!(landed > 0);
        assert_eq!(state.try_with(|value| *value), Some(landed));
    }

    #[test]
    fn pages_are_laid_out_for_about_720_rows() {
        assert_eq!(scale(800), 1);
        assert_eq!(scale(1080), 1);
        assert_eq!(scale(1440), 2);
        assert_eq!(scale(2160), 3);
    }

    #[test]
    fn the_window_keeps_the_selection_in_view() {
        assert_eq!(window(0, 10, 4), 0);
        assert_eq!(window(5, 10, 4), 2);
        assert_eq!(window(9, 10, 4), 6);
        assert_eq!(window(1, 2, 4), 0);
    }

    #[test]
    fn percentages_need_a_consistent_total_and_complete_only_at_the_end() {
        assert_eq!(percentage(0, 0), None);
        assert_eq!(percentage(1, 0), None);
        assert_eq!(percentage(11, 10), None);
        assert_eq!(percentage(0, 3), Some(0));
        assert_eq!(percentage(1, 3), Some(33));
        assert_eq!(percentage(2, 3), Some(66));
        assert_eq!(percentage(99, 100), Some(99));
        assert_eq!(percentage(3, 3), Some(100));
    }

    #[test]
    fn full_width_byte_counts_do_not_overflow_or_round_up_to_completion() {
        assert_eq!(percentage(u64::MAX - 1, u64::MAX), Some(99));
        assert_eq!(percentage(u64::MAX, u64::MAX), Some(100));
    }

    #[test]
    fn sizes_read_as_three_figures_in_decimal_units() {
        assert_eq!(disk_size(1_000_204_886_016), "1.0 TB");
        assert_eq!(disk_size(512 * GB), "512 GB");
        assert_eq!(disk_size(8_600_000_000), "8.6 GB");
        assert_eq!(disk_size(260 * 1_000_000), "260 MB");
    }

    /// Whether each channel of `pixel` is within 2 of `expected`'s.
    fn near(pixel: u32, expected: u32) -> bool {
        (0..3).all(|channel| {
            let value = |colour: u32| i32::from((colour >> (8 * channel)) as u8);
            (value(pixel) - value(expected)).abs() <= 2
        })
    }

    /// Indigo: more blue than red.
    fn tile(pixel: u32) -> bool {
        pixel & 0xff > pixel >> 16
    }

    #[test]
    fn the_logo_is_a_gold_pillar_on_an_indigo_tile() {
        // The tile's rounded corners are transparent at any size.
        for side in [32, 128, 1024] {
            let last = side - 1;
            for (x, y) in [(0, 0), (last, 0), (0, last), (last, last)] {
                assert_eq!(logo_pixel(x, y, side), None, "{side}: ({x}, {y})");
            }
        }
        // One sample per unit of the icon's 1024.
        let at = |x, y| logo_pixel(x, y, 1024).unwrap();
        // The tile: indigo at the top to deep navy at the bottom.
        assert!(near(at(512, 5), 0x2b3a8c), "{:06x}", at(512, 5));
        assert!(near(at(512, 1018), 0x141a3d), "{:06x}", at(512, 1018));
        // The dome's top, at 112.7: the tile just above, gold just below.
        assert!(tile(at(512, 111)), "{:06x}", at(512, 111));
        assert_eq!(at(512, 114), 0xffd27a);
        // The abacus, and the tile in the gap between it and the bell.
        assert!(near(at(300, 200), 0xfdc871), "{:06x}", at(300, 200));
        assert!(tile(at(512, 257)), "{:06x}", at(512, 257));
        // The shaft, gold toward saffron, and its pale stripe lighter.
        assert!(near(at(512, 563), 0xf69e49), "{:06x}", at(512, 563));
        assert!(at(450, 500) & 0xff > (at(540, 500) & 0xff) + 40);
        // The base step is wider than the step above it.
        assert!(!tile(at(290, 845)));
        assert!(tile(at(300, 800)));
        assert!(!tile(at(350, 800)));
    }

    /// The logo `side` pixels square, as `stela-boot`'s `ui::Canvas` drew
    /// it before [`LogoCache`]: four by four samples a pixel,
    /// `ui::BACKGROUND` outside the tile.
    fn drawn(side: usize) -> Vec<u32> {
        const SAMPLES: usize = 4;
        let mut pixels = Vec::new();
        for py in 0..side {
            for px in 0..side {
                let mut sum = [0u32; 3];
                for sy in 0..SAMPLES {
                    for sx in 0..SAMPLES {
                        let colour = logo_pixel(
                            (px * SAMPLES + sx) as u32,
                            (py * SAMPLES + sy) as u32,
                            (side * SAMPLES) as u32,
                        )
                        .unwrap_or(BACKGROUND);
                        for (channel, total) in sum.iter_mut().enumerate() {
                            *total += (colour >> (16 - 8 * channel)) & 0xff;
                        }
                    }
                }
                let n = (SAMPLES * SAMPLES) as u32;
                pixels.push(
                    sum.iter()
                        .fold(0, |pixel, total| (pixel << 8) | ((total + n / 2) / n)),
                );
            }
        }
        pixels
    }

    /// The cache makes the logo once for each size and background in turn,
    /// the same pixels the header drew each time before it.
    #[test]
    fn the_logo_is_made_once_for_its_size() {
        let mut cache = LogoCache::new();
        for _ in 0..3 {
            assert_eq!(cache.get(96, BACKGROUND), &drawn(96)[..]);
        }
        assert_eq!(cache.made, 1);
        assert_eq!(cache.get(32, BACKGROUND), &drawn(32)[..]);
        assert_eq!(cache.get(32, TEXT), &logo_image(32, TEXT)[..]);
        assert_eq!(cache.get(32, TEXT).len(), 32 * 32);
        assert_eq!(cache.made, 3);
    }

    /// `ui::BACKGROUND` and `ui::TEXT`.
    const BACKGROUND: u32 = 0x18191c;
    const TEXT: u32 = 0xe6e7ea;

    /// The console font (`stela-boot`'s `console::font`): PSF1, 8 by 16.
    static SPLEEN: &[u8] = include_bytes!("../../stela-boot/src/console/spleen-8x16.psfu");

    fn glyph(character: u8, x: usize, y: usize) -> bool {
        assert_eq!(SPLEEN[..2], [0x36, 0x04]);
        SPLEEN[4 + usize::from(character) * 16 + y] & (0x80 >> x) != 0
    }

    /// The page header `ui::Canvas::header` draws, `width` units wide at
    /// `scale`: the logo and the name, together centred.
    fn header(width: usize, scale: usize, name: &str) -> (usize, Vec<u32>) {
        const LOGO: usize = 32;
        const HEADER: usize = 88;
        let (columns, rows) = (width * scale, HEADER * scale);
        let mut pixels = vec![BACKGROUND; columns * rows];
        let name_width = name.len() * 8 * 2;
        let gap = 12;
        let left = (width - (LOGO + gap + name_width)) / 2;
        let top = 28;
        let logo = drawn(LOGO * scale);
        for (index, &colour) in logo.iter().enumerate() {
            let (x, y) = (index % (LOGO * scale), index / (LOGO * scale));
            pixels[(top * scale + y) * columns + left * scale + x] = colour;
        }
        let set = |y| name.bytes().any(|c| (0..8).any(|x| glyph(c, x, y)));
        let first = (0..16).find(|&y| set(y)).unwrap();
        let last = (0..16).rev().find(|&y| set(y)).unwrap();
        let text_top = (top + LOGO / 2) - (first + last + 1);
        let s = 2 * scale;
        for (index, character) in name.bytes().enumerate() {
            let x0 = (left + LOGO + gap + index * 16) * scale;
            for gy in 0..16 {
                for gx in (0..8).filter(|&gx| glyph(character, gx, gy)) {
                    for dy in 0..s {
                        let row = text_top * scale + gy * s + dy;
                        let start = row * columns + x0 + gx * s;
                        pixels[start..start + s].fill(TEXT);
                    }
                }
            }
        }
        (columns, pixels)
    }

    /// A PNG of `pixels`, rows of `width`, stored without compression.
    fn png(width: usize, pixels: &[u32]) -> Vec<u8> {
        fn crc(bytes: &[u8]) -> u32 {
            let mut crc = !0u32;
            for &byte in bytes {
                crc ^= u32::from(byte);
                for _ in 0..8 {
                    crc = (crc >> 1) ^ (0xedb8_8320 & (crc & 1).wrapping_neg());
                }
            }
            !crc
        }
        let mut raw = Vec::new();
        for row in pixels.chunks(width) {
            raw.push(0);
            for &pixel in row {
                raw.extend_from_slice(&[(pixel >> 16) as u8, (pixel >> 8) as u8, pixel as u8]);
            }
        }
        let mut zlib = vec![0x78, 0x01];
        let blocks = raw.chunks(0xffff).count();
        for (index, block) in raw.chunks(0xffff).enumerate() {
            zlib.push(u8::from(index + 1 == blocks));
            let length = block.len() as u16;
            zlib.extend_from_slice(&length.to_le_bytes());
            zlib.extend_from_slice(&(!length).to_le_bytes());
            zlib.extend_from_slice(block);
        }
        let (mut a, mut b) = (1u32, 0u32);
        for &byte in &raw {
            a = (a + u32::from(byte)) % 65521;
            b = (b + a) % 65521;
        }
        zlib.extend_from_slice(&((b << 16) | a).to_be_bytes());
        let mut header = Vec::new();
        header.extend_from_slice(&(width as u32).to_be_bytes());
        header.extend_from_slice(&((pixels.len() / width) as u32).to_be_bytes());
        header.extend_from_slice(&[8, 2, 0, 0, 0]);
        let mut file = b"\x89PNG\r\n\x1a\n".to_vec();
        for (kind, data) in [(b"IHDR", &header[..]), (b"IDAT", &zlib), (b"IEND", &[])] {
            file.extend_from_slice(&(data.len() as u32).to_be_bytes());
            let start = file.len();
            file.extend_from_slice(kind);
            file.extend_from_slice(data);
            let sum = crc(&file[start..]);
            file.extend_from_slice(&sum.to_be_bytes());
        }
        file
    }

    /// The logo and the header render; with `STELA_BRAND_SHOTS=DIR` they
    /// are also written to `DIR` as `brand-*.png`, to look at.
    #[test]
    fn the_logo_and_header_render_as_the_boot_screen_draws_them() {
        let mut shots = Vec::new();
        let large = (0..1024 * 1024)
            .map(|index| logo_pixel(index % 1024, index / 1024, 1024).unwrap_or(BACKGROUND))
            .collect::<Vec<_>>();
        shots.push((String::from("brand-logo-1024"), 1024, large));
        for side in [32, 64, 96] {
            shots.push((format!("brand-logo-{side}"), side, drawn(side)));
        }
        for scale in [1, 2] {
            let (width, pixels) = header(480, scale, "Stambha");
            // The name's text is drawn right of the logo.
            assert!(pixels.contains(&TEXT));
            shots.push((format!("brand-header-{scale}x"), width, pixels));
        }
        let directory = std::env::var_os("STELA_BRAND_SHOTS");
        for (name, width, pixels) in &shots {
            let file = png(*width, pixels);
            assert!(file.starts_with(b"\x89PNG"));
            if let Some(directory) = &directory {
                let path = std::path::Path::new(directory).join(format!("{name}.png"));
                std::fs::write(&path, file).expect("the screenshot is written");
            }
        }
    }
}
