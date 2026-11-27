//! The host's own pointer, shown at the display's rate (`--host-cursor on`).
//!
//! WindowServer draws the pointer as a layer of its own: its cursor client
//! submits swaps that set layer 1 alone, a surface the CPU drew (128 by 128
//! on a 2560 by 1600 panel), placed with its hotspot at the pointer
//! ([`super::iomfb`]). Each move is a whole guest round trip: the host's
//! report reaches the guest's USB peripheral, then IOHIDFamily,
//! WindowServer's event loop and its cursor client, then a swap. Under
//! translation that came to 3-20 moves a second, 35-950 ms on average from
//! a report to the screen and up to 28 s at worst, on a 165 Hz panel (boot
//! 35, `CURSOR` lines).
//!
//! With the host cursor on, the host owns where the pointer is. A mouse's
//! motion (or a touchpad's, read as a mouse) is accelerated on the host
//! ([`Pointer`]) into a position the guest is given as its absolute
//! pointer's (the tablet interface of [`crate::usb::hid`]), which macOS
//! takes as it is: once the guest has read the report, guest and host agree
//! on the position exactly, so a click lands where the host shows the
//! pointer. The display ([`Tracker`]) shows the image the guest last gave
//! at the host's position less its hotspot, at each vblank the position
//! changed, whatever the guest's own frame rate.
//!
//! The hotspot is not in the swap. It is learned from the guest's own
//! placements: the guest shows its pointer at `position - hotspot` for a
//! position it was given, so each placement's origin plus the hotspot is one
//! of the positions recently given ([`Shared`] keeps the last [`HISTORY`]).
//! A hotspot that explains a placement holds. One that explains none drops
//! the lock, and the guest's own placement is shown, as without the host
//! cursor, until a hotspot is found again:
//!
//! - the host's pointer still for [`STILL_TICKS`]: the guest has caught up,
//!   so the hotspot is the newest position less the origin when that lies
//!   inside the surface (a new shape); outside it, the guest moved its
//!   pointer itself (a warp) and the host takes the guest's position as its
//!   own;
//! - while it moves: the candidates two placements at different origins
//!   share, once one alone is left (motion along a straight line leaves
//!   several, which later placements narrow).
//!
//! Positions are in the scanout's pixels; the guest's tablet covers the
//! screen, so a position is given as the tablet value whose pixel it is
//! whichever of the two usual scalings (by the logical maximum, or by one
//! more) the guest applies ([`tablet`]).
use super::super::scanout::{Layer, Rect};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use stela_drivers::input::{Input, POINTER_MAX};

/// The positions given the guest that a placement is matched against: at a
/// mouse's 1000 reports a second, half a second of motion.
pub const HISTORY: usize = 512;
/// The newest positions a placement is matched against.
const MATCHED: usize = 256;
/// The host's pointer is still once no position was given for this long,
/// in counter ticks (200 ms).
pub const STILL_TICKS: u64 = crate::counter::FREQUENCY_HZ / 5;
/// The largest pointer surface taken for a pointer, each way.
const LARGEST: u32 = 256;
/// How far, in pixels each way, a placement may stand from a position and
/// still be its: the guest scales the tablet to points and its points to
/// pixels, each of which may round.
const NEAR: i32 = 2;
/// Fractional bits of the host's position.
const FRACTION: u32 = 8;
/// The pointer buttons the tablet interface carries: left, right, middle.
const TABLET_BUTTONS: u8 = 0b111;

/// What the input service and the display share: whether the host cursor
/// is on, the screen, the host's position, the positions given and the
/// counts reported each beat. One lives for the boot ([`HOST_CURSOR`]).
pub struct Shared {
    on: AtomicBool,
    /// `width << 32 | height`, zero until the display is placed.
    screen: AtomicU64,
    /// The host's position, `x << 32 | y` in 1/256 pixels.
    position: AtomicU64,
    /// The positions given, `x << 32 | y` in pixels, by [`Self::given`]
    /// modulo [`HISTORY`].
    history: [AtomicU64; HISTORY],
    given: AtomicU64,
    counts: [AtomicU64; COUNTS],
    /// Whether the hotspot holds, and the hotspot (`x << 32 | y`), as the
    /// display last set them.
    locked: AtomicBool,
    hotspot: AtomicU64,
}

/// The host cursor of this boot.
pub static HOST_CURSOR: Shared = Shared::new();

const GIVEN: usize = 0;
const DRAWN: usize = 1;
const GUEST_MOVES: usize = 2;
const DIVERGED_TOTAL: usize = 3;
const DIVERGED_MOST: usize = 4;
const DIVERGED_SAMPLES: usize = 5;
const LOCKS: usize = 6;
const UNLOCKS: usize = 7;
const WARPS: usize = 8;
const SHAPES: usize = 9;
const COUNTS: usize = 10;

/// What the host cursor did since the counts were last taken.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// Positions given the guest.
    pub given: u64,
    /// Presents that showed the pointer where the host put it, with no
    /// swap of the guest's.
    pub drawn: u64,
    /// Guest placements of its pointer at a new origin.
    pub guest_moves: u64,
    /// How far the guest's pointer stood from the host's at its placements
    /// while locked, in pixels: in total, at most, and the placements.
    pub diverged_total: u64,
    pub diverged_most: u64,
    pub diverged_samples: u64,
    pub locks: u64,
    pub unlocks: u64,
    /// Moves the guest made itself that the host took as its position.
    pub warps: u64,
    /// New hotspots found while the host's pointer was still.
    pub shapes: u64,
    /// Whether the hotspot holds now, and the hotspot.
    pub locked: bool,
    pub hotspot: (i32, i32),
}

fn pack(x: i32, y: i32) -> u64 {
    u64::from(x as u32) << 32 | u64::from(y as u32)
}

fn unpack(word: u64) -> (i32, i32) {
    ((word >> 32) as u32 as i32, word as u32 as i32)
}

impl Default for Shared {
    fn default() -> Self {
        Self::new()
    }
}

impl Shared {
    pub const fn new() -> Self {
        Self {
            on: AtomicBool::new(false),
            screen: AtomicU64::new(0),
            position: AtomicU64::new(0),
            history: [const { AtomicU64::new(0) }; HISTORY],
            given: AtomicU64::new(0),
            counts: [const { AtomicU64::new(0) }; COUNTS],
            locked: AtomicBool::new(false),
            hotspot: AtomicU64::new(0),
        }
    }

    /// Turn the host cursor on: the input service gives the guest absolute
    /// positions from then on, once the display has set the screen.
    pub fn enable(&self) {
        self.on.store(true, Ordering::Release);
    }

    pub fn on(&self) -> bool {
        self.on.load(Ordering::Acquire)
    }

    /// The screen is `width` by `height` pixels; the host's pointer starts
    /// at its middle.
    pub fn set_screen(&self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        let first = self
            .screen
            .swap(u64::from(width) << 32 | u64::from(height), Ordering::AcqRel);
        if first == 0 {
            self.store((width / 2) as i32, (height / 2) as i32);
        }
    }

    pub fn screen(&self) -> Option<(u32, u32)> {
        match self.screen.load(Ordering::Acquire) {
            0 => None,
            word => Some(((word >> 32) as u32, word as u32)),
        }
    }

    /// The host's position in whole pixels.
    pub fn position(&self) -> (i32, i32) {
        let (x, y) = unpack(self.position.load(Ordering::Acquire));
        (x >> FRACTION, y >> FRACTION)
    }

    fn store(&self, x: i32, y: i32) {
        self.position
            .store(pack(x << FRACTION, y << FRACTION), Ordering::Release);
    }

    /// The host's position is `(x, y)`, clamped to the screen, and is given
    /// the guest: the history records it.
    pub fn place(&self, x: i32, y: i32) -> (i32, i32) {
        let (width, height) = self.screen().unwrap_or((1, 1));
        let x = x.clamp(0, width as i32 - 1);
        let y = y.clamp(0, height as i32 - 1);
        self.store(x, y);
        self.give(x, y);
        (x, y)
    }

    /// Move the host's position by `(dx, dy)` counts at `gain` (in 1/256
    /// pixels a count), clamped to the screen of `width` by `height`; the
    /// whole pixels it reaches.
    fn advance(&self, dx: i32, dy: i32, gain: i32, width: u32, height: u32) -> (i32, i32) {
        let (x, y) = unpack(self.position.load(Ordering::Acquire));
        let clamp = |at: i32, by: i32, size: u32| {
            let most = (size as i64 - 1) << FRACTION;
            (i64::from(at) + i64::from(by) * i64::from(gain)).clamp(0, most) as i32
        };
        let (x, y) = (clamp(x, dx, width), clamp(y, dy, height));
        self.position.store(pack(x, y), Ordering::Release);
        (x >> FRACTION, y >> FRACTION)
    }

    /// Record that the guest was given `(x, y)`.
    fn give(&self, x: i32, y: i32) {
        let given = self.given.load(Ordering::Relaxed);
        self.history[given as usize % HISTORY].store(pack(x, y), Ordering::Relaxed);
        self.given.store(given + 1, Ordering::Release);
        self.count(GIVEN, 1);
    }

    /// The positions given since boot.
    pub fn given(&self) -> u64 {
        self.given.load(Ordering::Acquire)
    }

    /// The newest `most` positions given, newest first, each once in a row.
    fn recent(&self, most: usize) -> Vec<(i32, i32)> {
        let given = self.given();
        let mut positions: Vec<(i32, i32)> = Vec::new();
        for back in 0..given.min(HISTORY as u64).min(most as u64) {
            let at = (given - 1 - back) as usize % HISTORY;
            let position = unpack(self.history[at].load(Ordering::Relaxed));
            if positions.last() != Some(&position) {
                positions.push(position);
            }
        }
        positions
    }

    fn count(&self, which: usize, by: u64) {
        self.counts[which].fetch_add(by, Ordering::Relaxed);
    }

    fn most(&self, which: usize, value: u64) {
        self.counts[which].fetch_max(value, Ordering::Relaxed);
    }

    /// A present showed the pointer where the host put it.
    pub fn drawn(&self) {
        self.count(DRAWN, 1);
    }

    /// The counts since they were last taken, which start again from zero.
    pub fn take(&self) -> Counts {
        let take = |which: usize| self.counts[which].swap(0, Ordering::Relaxed);
        Counts {
            given: take(GIVEN),
            drawn: take(DRAWN),
            guest_moves: take(GUEST_MOVES),
            diverged_total: take(DIVERGED_TOTAL),
            diverged_most: take(DIVERGED_MOST),
            diverged_samples: take(DIVERGED_SAMPLES),
            locks: take(LOCKS),
            unlocks: take(UNLOCKS),
            warps: take(WARPS),
            shapes: take(SHAPES),
            locked: self.locked.load(Ordering::Relaxed),
            hotspot: unpack(self.hotspot.load(Ordering::Relaxed)),
        }
    }
}

/// The tablet value of pixel `at` of `size`: the middle of the pixel, so
/// that the guest's scaling by [`POINTER_MAX`] and by one more both land
/// in it.
pub fn tablet(at: i32, size: u32) -> u16 {
    let size = u64::from(size.max(1));
    let at = u64::from(at.max(0) as u32).min(size - 1);
    ((2 * at + 1) * u64::from(POINTER_MAX) / (2 * size)).min(u64::from(POINTER_MAX)) as u16
}

/// The pixel of `size` a tablet value `value` stands for.
pub fn pixel(value: u16, size: u32) -> i32 {
    (u64::from(value.min(POINTER_MAX)) * u64::from(size) / (u64::from(POINTER_MAX) + 1)) as i32
}

/// The input service's side: a mouse's motion made a position the guest's
/// tablet is given ([`Self::convert`]).
#[derive(Debug, Default)]
pub struct Pointer {
    /// The TSC at the last motion, zero before it.
    last_motion: u64,
    /// The buttons last given the tablet, and the mouse buttons past the
    /// tablet's three (side and extra) last given the mouse.
    tablet_buttons: Option<u8>,
    side_buttons: u8,
}

impl Pointer {
    /// With the host cursor on and its screen set, turn `inputs`, read at
    /// TSC `now` (`tsc_hz` a second), into the guest's absolute pointer's:
    /// motion moves the host's position, given as one absolute move per run
    /// of motion; a host tablet's move sets it; a mouse's first three
    /// buttons go with the tablet, so a click is seen at the tablet's
    /// position, and its side buttons stay on the mouse.
    pub fn convert(&mut self, shared: &Shared, inputs: &mut Vec<Input>, now: u64, tsc_hz: u64) {
        if !shared.on() {
            return;
        }
        let Some((width, height)) = shared.screen() else {
            return;
        };
        if !inputs.iter().any(|input| {
            matches!(
                input,
                Input::Motion { .. } | Input::Move { .. } | Input::MouseButtons(_)
            )
        }) {
            return;
        }
        let gain = self.gain(inputs, now, tsc_hz);
        let mut converted = Vec::with_capacity(inputs.len());
        let mut moved = None;
        let give = |moved: &mut Option<(i32, i32)>, converted: &mut Vec<Input>| {
            if let Some((x, y)) = moved.take() {
                shared.give(x, y);
                converted.push(Input::Move {
                    x: tablet(x, width),
                    y: tablet(y, height),
                });
            }
        };
        for input in inputs.drain(..) {
            match input {
                Input::Motion { dx, dy } => {
                    moved = Some(shared.advance(dx.into(), dy.into(), gain, width, height));
                }
                Input::Move { x, y } => {
                    give(&mut moved, &mut converted);
                    let (x, y) = shared.place(pixel(x, width), pixel(y, height));
                    converted.push(Input::Move {
                        x: tablet(x, width),
                        y: tablet(y, height),
                    });
                }
                Input::MouseButtons(held) => {
                    give(&mut moved, &mut converted);
                    let tablet_held = held & TABLET_BUTTONS;
                    if self.tablet_buttons.replace(tablet_held) != Some(tablet_held) {
                        converted.push(Input::Buttons(tablet_held));
                    }
                    let side = held & !TABLET_BUTTONS;
                    if side != self.side_buttons {
                        self.side_buttons = side;
                        converted.push(Input::MouseButtons(side));
                    }
                }
                other => {
                    give(&mut moved, &mut converted);
                    converted.push(other);
                }
            }
        }
        give(&mut moved, &mut converted);
        *inputs = converted;
    }

    /// The gain of the motion in `inputs`, in 1/256 pixels a count, from
    /// its speed since the last motion: one pixel a count up to one count a
    /// millisecond, then half a pixel more for each count a millisecond
    /// faster, up to four (a host curve near macOS's default mouse
    /// tracking at 1x; the guest's own acceleration does not apply to an
    /// absolute pointer). The time since the last motion counts as 1 to 50
    /// ms, so the first motion after a rest is slow and precise.
    fn gain(&mut self, inputs: &[Input], now: u64, tsc_hz: u64) -> i32 {
        let counts: u64 = inputs
            .iter()
            .map(|input| match *input {
                Input::Motion { dx, dy } => {
                    let (a, b) = (dx.unsigned_abs(), dy.unsigned_abs());
                    u64::from(a.max(b)) + u64::from(a.min(b)) / 2
                }
                _ => 0,
            })
            .sum();
        if counts == 0 {
            return 1 << FRACTION;
        }
        let tsc_hz = tsc_hz.max(1000);
        let elapsed = match self.last_motion {
            0 => tsc_hz / 20,
            last => now.saturating_sub(last).clamp(tsc_hz / 1000, tsc_hz / 20),
        };
        self.last_motion = now.max(1);
        // Counts a millisecond, in 1/256.
        let speed = u128::from(counts) * 256 * u128::from(tsc_hz) / (1000 * u128::from(elapsed));
        let speed = speed.min(1 << 20) as i32;
        ((1 << FRACTION) + (speed - 256).max(0) / 2).min(4 << FRACTION)
    }
}

/// Where a pointer layer's whole surface stands, and its size: its
/// destination less its source's offset, as the guest clips a pointer at
/// the screen's edges by its source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Shape {
    origin: (i32, i32),
    size: (i32, i32),
}

impl Shape {
    /// `layer` as a pointer on a `screen`: shown unscaled, from a surface
    /// no larger than [`LARGEST`] each way, whose source leaves out only
    /// what lies past the screen's edges. `None` for any other layer.
    fn of(layer: &Layer, screen: (u32, u32)) -> Option<Self> {
        let (source, destination, surface) = (layer.source, layer.destination, layer.surface);
        if (source.width, source.height) != (destination.width, destination.height)
            || surface.width > LARGEST
            || surface.height > LARGEST
        {
            return None;
        }
        let whole = |start: u32, length: u32, at: u32, size: u32, screen: u32| {
            (start == 0 || at == 0) && (start + length == size || at + length >= screen)
        };
        if !whole(
            source.x,
            source.width,
            destination.x,
            surface.width,
            screen.0,
        ) || !whole(
            source.y,
            source.height,
            destination.y,
            surface.height,
            screen.1,
        ) {
            return None;
        }
        Some(Self {
            origin: (
                destination.x as i32 - source.x as i32,
                destination.y as i32 - source.y as i32,
            ),
            size: (surface.width as i32, surface.height as i32),
        })
    }

    fn holds(&self, hotspot: (i32, i32)) -> bool {
        (0..self.size.0).contains(&hotspot.0) && (0..self.size.1).contains(&hotspot.1)
    }
}

fn near(a: (i32, i32), b: (i32, i32)) -> bool {
    (a.0 - b.0).abs() <= NEAR && (a.1 - b.1).abs() <= NEAR
}

fn distance(a: (i32, i32), b: (i32, i32)) -> u64 {
    let (dx, dy) = (i64::from(a.0 - b.0), i64::from(a.1 - b.1));
    (dx * dx + dy * dy).unsigned_abs().isqrt()
}

/// The display's side: the hotspot learned from the guest's placements
/// ([`Self::observe`]) and the pointer layer shown at the host's position
/// ([`Self::place`]).
pub struct Tracker {
    shared: &'static Shared,
    hotspot: Option<(i32, i32)>,
    locked: bool,
    /// The origin a hotspot was found at, which a placement elsewhere
    /// confirms.
    found_at: Option<(i32, i32)>,
    /// The candidates left from the placements no hotspot explained while
    /// the host's pointer moved, and the last one's origin.
    candidates: Vec<(i32, i32)>,
    candidates_at: Option<(i32, i32)>,
    /// The guest's last placement's origin.
    origin: Option<(i32, i32)>,
    /// The positions given at the last look, and the counter when that
    /// count last changed.
    given: u64,
    still_since: u64,
}

impl Tracker {
    pub fn new(shared: &'static Shared) -> Self {
        Self {
            shared,
            hotspot: None,
            locked: false,
            found_at: None,
            candidates: Vec::new(),
            candidates_at: None,
            origin: None,
            given: shared.given(),
            still_since: 0,
        }
    }

    pub fn shared(&self) -> &'static Shared {
        self.shared
    }

    /// Whether the pointer is shown at the host's position.
    pub fn locked(&self) -> bool {
        self.locked
    }

    /// The display looked at counter `now`.
    pub fn tick(&mut self, now: u64) {
        let given = self.shared.given();
        if given != self.given {
            self.given = given;
            self.still_since = now;
        }
    }

    fn still(&self, now: u64) -> bool {
        self.shared.given() == self.given && now.saturating_sub(self.still_since) >= STILL_TICKS
    }

    fn lock(&mut self, hotspot: (i32, i32), at: (i32, i32)) {
        self.hotspot = Some(hotspot);
        self.found_at = Some(at);
        self.candidates.clear();
        self.candidates_at = None;
        if !self.locked {
            self.locked = true;
            self.shared.count(LOCKS, 1);
        }
        self.publish();
    }

    fn unlock(&mut self) {
        if self.locked {
            self.locked = false;
            self.shared.count(UNLOCKS, 1);
            self.publish();
        }
    }

    fn publish(&self) {
        self.shared.locked.store(self.locked, Ordering::Relaxed);
        let (x, y) = self.hotspot.unwrap_or_default();
        self.shared.hotspot.store(pack(x, y), Ordering::Relaxed);
    }

    /// The guest placed its pointer as `layer` on a `screen`, in a swap it
    /// submitted at counter `now`.
    pub fn observe(&mut self, layer: &Layer, screen: (u32, u32), now: u64) {
        self.tick(now);
        let Some(shape) = Shape::of(layer, screen) else {
            self.unlock();
            return;
        };
        let origin = shape.origin;
        if self.origin.replace(origin) != Some(origin) {
            self.shared.count(GUEST_MOVES, 1);
        }
        let positions = self.shared.recent(MATCHED);
        let explains = |hotspot: (i32, i32)| {
            positions
                .iter()
                .any(|&(x, y)| near((x - origin.0, y - origin.1), hotspot))
        };
        if let Some(hotspot) = self.hotspot
            && explains(hotspot)
        {
            if !self.locked && self.found_at != Some(origin) {
                self.lock(hotspot, origin);
            }
            if self.locked
                && let Some(&newest) = positions.first()
            {
                let diverged = distance(newest, (origin.0 + hotspot.0, origin.1 + hotspot.1));
                self.shared.count(DIVERGED_TOTAL, diverged);
                self.shared.most(DIVERGED_MOST, diverged);
                self.shared.count(DIVERGED_SAMPLES, 1);
            }
            return;
        }
        self.unlock();
        let Some(&newest) = positions.first() else {
            return;
        };
        if self.still(now) {
            let hotspot = (newest.0 - origin.0, newest.1 - origin.1);
            if shape.holds(hotspot) {
                // A new shape at rest: the guest has caught up.
                if self.hotspot.is_some() {
                    self.shared.count(SHAPES, 1);
                }
                self.lock(hotspot, origin);
            } else if let Some(hotspot) = self.hotspot {
                // The guest moved its pointer itself: the host follows.
                let placed = self
                    .shared
                    .place(origin.0 + hotspot.0, origin.1 + hotspot.1);
                self.given = self.shared.given();
                self.still_since = now;
                self.shared.count(WARPS, 1);
                self.lock((placed.0 - origin.0, placed.1 - origin.1), origin);
            }
            return;
        }
        let mut candidates: Vec<(i32, i32)> = Vec::new();
        for &(x, y) in &positions {
            let candidate = (x - origin.0, y - origin.1);
            if shape.holds(candidate) && !candidates.contains(&candidate) {
                candidates.push(candidate);
            }
        }
        if self.candidates_at.is_some_and(|at| at != origin) {
            let kept = &self.candidates;
            let exact: Vec<(i32, i32)> = candidates
                .iter()
                .copied()
                .filter(|candidate| kept.contains(candidate))
                .collect();
            if let [hotspot] = exact.as_slice() {
                self.lock(*hotspot, origin);
                return;
            }
            if !exact.is_empty() {
                candidates = exact;
            } else {
                // The guest's rounding may move a placement a pixel or two:
                // candidates that close together are one.
                let close: Vec<(i32, i32)> = candidates
                    .iter()
                    .copied()
                    .filter(|&candidate| kept.iter().any(|&kept| near(kept, candidate)))
                    .collect();
                if let [first, rest @ ..] = close.as_slice() {
                    if rest.iter().all(|&other| near(other, *first)) {
                        self.lock(*first, origin);
                        return;
                    }
                    candidates = close;
                }
            }
        }
        self.candidates = candidates;
        self.candidates_at = Some(origin);
    }

    /// `layer`, the guest's pointer on a `screen`, shown at the host's
    /// position instead, while the hotspot holds; `None` otherwise or for a
    /// layer that is no pointer. Only the part on the screen is shown.
    pub fn place(&self, layer: &Layer, screen: (u32, u32)) -> Option<Layer> {
        if !self.locked {
            return None;
        }
        let hotspot = self.hotspot?;
        let shape = Shape::of(layer, screen)?;
        let (x, y) = self.shared.position();
        let origin = (x - hotspot.0, y - hotspot.1);
        let skipped = (
            origin.0.min(0).saturating_neg(),
            origin.1.min(0).saturating_neg(),
        );
        let at = (origin.0.max(0), origin.1.max(0));
        let width = (shape.size.0 - skipped.0).min(screen.0 as i32 - at.0);
        let height = (shape.size.1 - skipped.1).min(screen.1 as i32 - at.1);
        if width <= 0 || height <= 0 {
            return None;
        }
        let (width, height) = (width as u32, height as u32);
        Some(Layer {
            source: Rect {
                x: skipped.0 as u32,
                y: skipped.1 as u32,
                width,
                height,
            },
            destination: Rect {
                x: at.0 as u32,
                y: at.1 as u32,
                width,
                height,
            },
            ..*layer
        })
    }
}

#[cfg(test)]
mod tests;
