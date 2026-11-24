//! This boot's GuC mode for an Intel GPU i915 drives
//! ([`stela_boot_core::intel_guc`]): decided once, at the claim of the
//! first GPU whose platform runs GuC submission by default, from
//! `--i915-guc` and the earlier boot's record, which the loader read and
//! the handoff carries ([`set_earlier`]); the record of a failure this
//! boot sees, written as it is seen so a boot that hangs after it still
//! leaves it; and the stop that asks for a second start.
use crate::output;
use alloc::string::String;
use core::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};
use stela_boot_core::intel_guc::{self as guc, Cause, Mode, Record, Setting};

/// `--i915-guc`: 0 auto, 1 off, 2 on.
static SETTING: AtomicU8 = AtomicU8::new(0);
/// The earlier boot's record ([`encode_record`]); 0 for none.
static EARLIER: AtomicU64 = AtomicU64::new(0);
/// The decided mode and its device ([`encode_mode`]); 0 before the claim.
static MODE: AtomicU32 = AtomicU32::new(0);
/// The failure this boot recorded ([`encode`]); 0 for none.
static FAILED: AtomicU32 = AtomicU32::new(0);

/// Takes the boot's `--i915-guc`, before any GPU is claimed.
pub(in crate::engine) fn set_setting(setting: Setting) {
    let word = match setting {
        Setting::Auto => 0,
        Setting::Off => 1,
        Setting::On => 2,
    };
    SETTING.store(word, Ordering::Relaxed);
}

fn setting() -> Setting {
    match SETTING.load(Ordering::Relaxed) {
        1 => Setting::Off,
        2 => Setting::On,
        _ => Setting::Auto,
    }
}

/// Takes the earlier boot's record the loader read from the USB's log
/// (`Handoff::intel_guc_earlier`), before any GPU is claimed. The engine
/// runs a copy of the image taken before any static was set, so the
/// loader's own copy of it (`log_file::earlier_guc`) is not this one's:
/// dev64-1 read the record and the engine never saw it.
pub(in crate::engine) fn set_earlier(record: Option<Record>) {
    EARLIER.store(record.map_or(0, encode_record), Ordering::Relaxed);
}

/// A present flag, `kept`, the device and the cause ([`encode`]).
fn encode_record(record: Record) -> u64 {
    (1 << 63)
        | (u64::from(record.kept) << 62)
        | (u64::from(record.device) << 32)
        | u64::from(encode(record.cause))
}

fn earlier() -> Option<Record> {
    let word = EARLIER.load(Ordering::Relaxed);
    if word >> 63 == 0 {
        return None;
    }
    Some(Record {
        device: (word >> 32) as u16,
        cause: decode(word as u32)?,
        kept: (word >> 62) & 1 == 1,
    })
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

/// The mode in the high half (1 GuC, 2 off by hand, 3 fallback, 4 off by
/// default), the device in the low half; the fallback's cause is its
/// record's.
fn encode_mode(mode: Mode, device: u16) -> u32 {
    let kind = match mode {
        Mode::Guc => 1,
        Mode::OffByHand => 2,
        Mode::Fallback(_) => 3,
        Mode::OffByDefault => 4,
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
    let record = earlier();
    let mode = Mode::decide(setting(), record, device, platform);
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
