//! This boot's GuC mode for an Intel GPU i915 drives
//! ([`stela_boot_core::intel_guc`]): decided once, at the claim of the
//! first GPU whose platform runs GuC submission by default, from
//! `--i915-guc` and the earlier boot's record; the record of a failure this
//! boot sees, written as it is seen so a boot that hangs after it still
//! leaves it; and the stop that asks for a second start.
use crate::output;
use alloc::string::String;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use stela_boot_core::intel_guc::{self as guc, Cause, Mode, Record, Setting};

/// `--i915-guc off`.
static OFF_BY_HAND: AtomicBool = AtomicBool::new(false);
/// The decided mode and its device ([`encode_mode`]); 0 before the claim.
static MODE: AtomicU32 = AtomicU32::new(0);
/// The failure this boot recorded ([`encode`]); 0 for none.
static FAILED: AtomicU32 = AtomicU32::new(0);

/// Takes the boot's `--i915-guc`, before any GPU is claimed.
pub(in crate::engine) fn set_setting(setting: Setting) {
    OFF_BY_HAND.store(setting == Setting::Off, Ordering::Relaxed);
}

fn encode(cause: Cause) -> u32 {
    match cause {
        Cause::Init(error) => (1 << 16) | u32::from(error as i16 as u16),
        Cause::Wedged => 2 << 16,
        Cause::Transport => 3 << 16,
        Cause::Error => 4 << 16,
        Cause::Metal => 5 << 16,
    }
}

fn decode(word: u32) -> Option<Cause> {
    match word >> 16 {
        1 => Some(Cause::Init(i32::from(word as u16 as i16))),
        2 => Some(Cause::Wedged),
        3 => Some(Cause::Transport),
        4 => Some(Cause::Error),
        5 => Some(Cause::Metal),
        _ => None,
    }
}

/// The mode in the high half (1 GuC, 2 off by hand, 3 fallback), the
/// device in the low half; the fallback's cause is its record's.
fn encode_mode(mode: Mode, device: u16) -> u32 {
    let kind = match mode {
        Mode::Guc => 1,
        Mode::OffByHand => 2,
        Mode::Fallback(_) => 3,
    };
    (kind << 16) | u32::from(device)
}

/// The decided mode's device and whether the GuC runs.
fn decided() -> Option<(u16, bool)> {
    let word = MODE.load(Ordering::Acquire);
    (word != 0).then_some((word as u16, word >> 16 == 1))
}

/// Whether i915 starts `device`, of i915's `platform`, without its GuC.
/// The first call for a platform that falls back decides the boot's mode
/// and says it in one line; a platform that does not is left as i915's
/// default has it, with no line.
pub(super) fn off_for(device: u16, platform: &str) -> bool {
    if !guc::falls_back(platform) {
        return false;
    }
    if let Some((_, guc)) = decided() {
        return !guc;
    }
    let record = crate::log_file::earlier_guc();
    let setting = if OFF_BY_HAND.load(Ordering::Relaxed) {
        Setting::Off
    } else {
        Setting::Auto
    };
    let mode = Mode::decide(setting, record, device);
    if MODE
        .compare_exchange(0, encode_mode(mode, device), Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return decided().is_some_and(|(_, guc)| !guc);
    }
    output::arguments(format_args!("{}", mode.line(device, record)));
    if let Mode::Fallback(cause) = mode {
        // So the boot after this one stays without the GuC too.
        let kept = Record {
            device,
            cause,
            kept: true,
        };
        output::arguments(format_args!("{}", kept.line()));
    }
    !mode.guc()
}

/// Records the GuC's failure once, as the first cause seen.
fn record(device: u16, cause: Cause) {
    if FAILED
        .compare_exchange(0, encode(cause), Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        let failed = Record {
            device,
            cause,
            kept: false,
        };
        output::arguments(format_args!("{}", failed.line()));
    }
}

/// A line Linux printed at `level`: an error of a running GuC is recorded.
pub(super) fn observe(level: i32, message: &str) {
    if !(1..=3).contains(&level) {
        return;
    }
    if let Some((device, true)) = decided()
        && let Some(cause) = Cause::of_line(message)
    {
        record(device, cause);
    }
}

/// The stop of a boot that has no Metal device while an Intel GPU ran its
/// GuC: the GuC's failure, recorded now as the Metal device's when nothing
/// was before, makes the next start go without it.
pub(in crate::engine) fn second_start() -> Option<String> {
    let Some((device, true)) = decided() else {
        return None;
    };
    if FAILED.load(Ordering::Acquire) == 0 {
        record(device, Cause::Metal);
    }
    decode(FAILED.load(Ordering::Acquire)).map(guc::second_start)
}
