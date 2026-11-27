//! What a normal boot's network (`engine::network`) does with a host
//! adapter's link that needs no hardware, so it is tested here.
//!
//! A Wi-Fi adapter's link is a connection keeper's, whose failures name the
//! network (`rtw89: <name> not found`, `… offers no supported security`)
//! and reach the boot as the link's own errors: the network stack's, the
//! `NETWORK adapter … failed` line and the screen's account, a later
//! download's and the guest network's. [`hide_ssid`] takes the configured
//! name out of them where the boot receives them.
//!
//! The keeper also fails and tries again on its own: a scan that missed
//! the network, a refused handshake, an AP that went. Its failure reaches
//! the network stack as the link's, which ends the stack's wait for an
//! address, often while the keeper's next attempt is joining. So a Wi-Fi
//! link's wait is driven again past each failure another attempt may mend
//! ([`address`]), as the host check's station check drives it (M3 plan
//! Task 20), until the link's carrier wait and then the stack's DHCP wait
//! have passed since it began ([`Cut`]). A wired link's wait ends at its
//! first failure, as it always has.
use alloc::string::{String, ToString};
use core::{cell::Cell, fmt};
use stela_runtime::net::{NetError, stack::DHCP_WAIT_US};

/// What a hidden network name reads as.
const HIDDEN: &str = "<ssid>";

/// Which of the host's adapters macOS's network takes, given each one's
/// `carriers` now and whether it is `wireless`, in probe order: the first
/// with carrier; else, when a Wi-Fi network is configured (`--wifi`), the
/// first Wi-Fi adapter, whose keeper joins that network once the guest
/// owns it; else the first. A Wi-Fi adapter has no carrier before its
/// keeper runs, so on a laptop whose Ethernet port precedes its Wi-Fi and
/// has no cable, the first adapter was always the empty port and macOS
/// had no network although Wi-Fi was configured.
pub fn guest_adapter(carriers: &[bool], wireless: &[bool], wifi_configured: bool) -> usize {
    carriers
        .iter()
        .position(|&carrier| carrier)
        .or_else(|| {
            wifi_configured
                .then(|| wireless.iter().position(|&wireless| wireless))
                .flatten()
        })
        .unwrap_or(0)
}

/// How long macOS's host adapter has had no carrier before another started
/// adapter with one takes its place ([`guest_switch`]): a cable moved from
/// one port to another, or a USB adapter whose PHY was still negotiating
/// when the guest network chose, is followed within seconds, where 0.6.0
/// kept the adapter it chose at the start for the whole boot.
pub const SWITCH_AFTER_MS: u64 = 2_000;

/// How long a spare adapter's carrier has held before it takes macOS's
/// network ([`guest_switch`]): a port that blinks up for a moment does not.
/// A 0.6.0 log of an IdeaPad with a broken RTL8168G port has that port up
/// for 410 ms and later 5.6 s, and an AX88179A up for 107 ms.
pub const SPARE_STEADY_MS: u64 = 1_000;

/// How long a wired adapter's carrier has held before the boot records a
/// wired network as up for the next boot's menu
/// (`wifi_prompt::Record::Wired`), which then leaves its Wi-Fi page closed:
/// that log's broken port and AX88179A each had a link for a moment, which
/// 0.6.1 would record as a wired network, closing the page on a machine
/// whose only working network may be its Wi-Fi.
pub const WIRED_STEADY_MS: u64 = 10_000;

/// Whether a carrier up since `since_ms` (none while down) has held for
/// `hold_ms` at `now_ms`.
pub fn steady(since_ms: Option<u64>, now_ms: u64, hold_ms: u64) -> bool {
    since_ms.is_some_and(|since| now_ms.saturating_sub(since) >= hold_ms)
}

/// Which spare adapter takes macOS's network at `now_ms`, all times in one
/// clock's milliseconds: none while its host adapter has carrier
/// (`down_since_ms` none) or has had none for less than
/// [`SWITCH_AFTER_MS`]; else the first spare, in order, whose carrier has
/// been up (`spares_up_since_ms`) for [`SPARE_STEADY_MS`].
pub fn guest_switch(
    now_ms: u64,
    down_since_ms: Option<u64>,
    spares_up_since_ms: &[Option<u64>],
) -> Option<usize> {
    if !steady(down_since_ms, now_ms, SWITCH_AFTER_MS) {
        return None;
    }
    spares_up_since_ms
        .iter()
        .position(|&since| steady(since, now_ms, SPARE_STEADY_MS))
}

/// [`guest_adapter`] among the adapters whose station address is `usable`,
/// in the same order; none when no adapter has one. The guest's USB
/// Ethernet adapter takes its host adapter's address, so an Intel Wi-Fi
/// chip whose firmware had not read its address yet gave macOS all zeros
/// and 0.6.1 stopped the boot ("the USB Ethernet address must be a unicast
/// address") on a laptop with no cable and a Wi-Fi network configured.
pub fn usable_guest_adapter(
    carriers: &[bool],
    wireless: &[bool],
    usable: &[bool],
    wifi_configured: bool,
) -> Option<usize> {
    let kept: alloc::vec::Vec<usize> = (0..carriers.len())
        .filter(|&index| usable.get(index).copied().unwrap_or(false))
        .collect();
    if kept.is_empty() {
        return None;
    }
    let pick = |of: &[bool]| -> alloc::vec::Vec<bool> {
        kept.iter()
            .map(|&index| of.get(index).copied().unwrap_or(false))
            .collect()
    };
    Some(kept[guest_adapter(&pick(carriers), &pick(wireless), wifi_configured)])
}

/// `text` with the network's name `ssid` written `<ssid>` wherever it
/// occurs, byte for byte, as `String::from_utf8_lossy` writes it (the
/// connection keeper's failures name the network so): occurrences that
/// overlap are hidden as one, and a name that is also part of other words
/// is hidden there too, so the line gives up legibility, never the name. A
/// `<ssid>` already in `text` stays as it is, even for a name that is part
/// of it, so hiding twice changes nothing; an empty name hides nothing.
pub fn hide_ssid(text: &str, ssid: &[u8]) -> String {
    let name = String::from_utf8_lossy(ssid);
    let (name, bytes) = (name.as_bytes(), text.as_bytes());
    if name.is_empty() || name.len() > bytes.len() {
        return String::from(text);
    }
    // Whether `bytes[start..end]` lies inside a `<ssid>` of `text`.
    let placed = |start: usize, end: usize| {
        (start.saturating_sub(HIDDEN.len())..=start)
            .any(|at| at + HIDDEN.len() >= end && bytes[at..].starts_with(HIDDEN.as_bytes()))
    };
    let mut hidden = String::with_capacity(text.len());
    // `text[..copied]` is in `hidden`, written out or hidden.
    let mut copied = 0;
    // The occurrences being hidden as one: `text[start..end]`.
    let mut run: Option<(usize, usize)> = None;
    let mut flush = |hidden: &mut String, (start, end): (usize, usize)| {
        // An occurrence of UTF-8 in UTF-8 starts and ends on characters.
        hidden.push_str(&text[copied..start]);
        hidden.push_str(HIDDEN);
        copied = end;
    };
    for start in 0..=bytes.len() - name.len() {
        let end = start + name.len();
        if !bytes[start..].starts_with(name) || placed(start, end) {
            continue;
        }
        match &mut run {
            Some((_, until)) if start < *until => *until = end,
            _ => {
                if let Some(done) = run.replace((start, end)) {
                    flush(&mut hidden, done);
                }
            }
        }
    }
    if let Some(done) = run {
        flush(&mut hidden, done);
    }
    hidden.push_str(&text[copied..]);
    hidden
}

/// What a Wi-Fi link's wait gives when it was cut before any failure: its
/// one attempt, with carrier, did not get a lease in the time left.
pub const NO_LEASE_IN_TIME: &str =
    "no DHCPv4 lease or usable IPv6 router advertisement within the Wi-Fi link's wait";

/// When a host adapter's wait for an address ends, read by the network
/// stack's clock: a Wi-Fi adapter's [`Self::wait_us`] after the stack's
/// first read of it; a wired adapter's never (its wait ends at its first
/// failure, [`address`]). A read at or past the end fails the clock, which
/// ends the stack's wait (the stack then refuses every operation, so the
/// adapter is given up as on any other failure).
#[derive(Debug, Default)]
pub struct Cut {
    /// How long the wait lasts; `None` for a wired adapter, and once the
    /// wait is over ([`address`]), so the stack's later reads are never cut.
    wait_us: Cell<Option<u64>>,
    /// The end, from the first read on.
    at_us: Cell<Option<u64>>,
    reached: Cell<bool>,
}

impl Cut {
    /// The cut of an adapter whose link may take `carrier_wait_us` to come
    /// up (`Link::carrier_wait_us`): for a Wi-Fi adapter that and then the
    /// stack's DHCP wait ([`DHCP_WAIT_US`]), the longest one wait of the
    /// stack's takes; none for a wired adapter.
    pub fn new(wireless: bool, carrier_wait_us: u64) -> Self {
        Self {
            wait_us: Cell::new(wireless.then(|| carrier_wait_us.saturating_add(DHCP_WAIT_US))),
            ..Self::default()
        }
    }

    /// How long a Wi-Fi adapter's wait lasts, while it goes on.
    pub fn wait_us(&self) -> Option<u64> {
        self.wait_us.get()
    }

    /// Whether the stack's clock, reading `now_us`, is past the end: the
    /// first read starts the wait, and once past, every read is.
    pub fn passed(&self, now_us: u64) -> bool {
        let Some(wait_us) = self.wait_us.get() else {
            return false;
        };
        let at_us = self
            .at_us
            .get()
            .unwrap_or_else(|| now_us.saturating_add(wait_us));
        self.at_us.set(Some(at_us));
        if now_us >= at_us {
            self.reached.set(true);
        }
        self.reached.get()
    }

    /// Whether a read was past the end.
    pub fn reached(&self) -> bool {
        self.reached.get()
    }
}

/// The failures a Wi-Fi link's wait was driven past: how many, the first
/// and the last, and how long the wait was when it was cut.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Retried {
    count: u32,
    first: Option<String>,
    last: Option<String>,
    cut_us: Option<u64>,
}

impl Retried {
    fn note(&mut self, failure: String) {
        self.count = self.count.saturating_add(1);
        match self.first {
            None => self.first = Some(failure),
            Some(_) => self.last = Some(failure),
        }
    }

    /// No failure was gone past and the wait was not cut: a wired adapter's
    /// always.
    pub fn is_empty(&self) -> bool {
        self.count == 0 && self.cut_us.is_none()
    }
}

/// Nothing when [`Retried::is_empty`]; else `retried past 1 failure:
/// <first>`, `retried past <n> failures: <first>` when the last was the
/// first's words, or `retried past <n> failures, first: <first>, last:
/// <last>`, then `; no address within <s> s` when the wait was cut.
impl fmt::Display for Retried {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(first) = &self.first {
            let count = self.count;
            match &self.last {
                None => write!(f, "retried past 1 failure: {first}")?,
                Some(last) if last == first => {
                    write!(f, "retried past {count} failures: {first}")?;
                }
                Some(last) => {
                    write!(
                        f,
                        "retried past {count} failures, first: {first}, last: {last}"
                    )?;
                }
            }
        }
        if let Some(cut_us) = self.cut_us {
            if self.first.is_some() {
                f.write_str("; ")?;
            }
            write!(f, "no address within {} s", cut_us / 1_000_000)?;
        }
        Ok(())
    }
}

/// A host adapter's wait for an address, `attempt` being the network
/// stack's (`Stack::automatic_waiting`) over a clock that [`Cut::passed`]
/// cuts. A wired adapter's first failure is its result, as it always was.
/// A Wi-Fi adapter's failures that another attempt may mend
/// (`NetError::retryable`: the keeper's failures, which reach the stack as
/// the link's, a carrier that did not come or went, no lease) are noted
/// and the attempt made again; any other failure is the result at once.
/// Once the cut ends an attempt, the result is the last failure gone past,
/// else [`NO_LEASE_IN_TIME`]. The wait is over on every return, so the
/// stack's later reads of the clock are never cut.
pub fn address<T>(
    cut: &Cut,
    mut attempt: impl FnMut() -> Result<T, NetError>,
) -> (Result<T, NetError>, Retried) {
    let mut retried = Retried::default();
    let mut last = None;
    let result = loop {
        match attempt() {
            Ok(done) => break Ok(done),
            Err(NetError::Clock(_)) if cut.reached() => {
                retried.cut_us = cut.wait_us();
                break Err(last.unwrap_or(NetError::Dhcp(NO_LEASE_IN_TIME)));
            }
            Err(error) if cut.wait_us().is_some() && error.retryable() => {
                retried.note(error.to_string());
                last = Some(error);
            }
            Err(error) => break Err(error),
        }
    };
    cut.wait_us.set(None);
    (result, retried)
}

/// What became of the host adapter's receive signal once the guest's link
/// came up ([`receive_signal`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReceiveSignal {
    /// The adapter raises no receive signal: its driver polls, and its
    /// frames are read at each host service turn, wherever that runs.
    Polled,
    /// The signal now goes to the processor asked for.
    Moved,
    /// The adapter signals, but its driver could not move the signal.
    Kept,
}

/// Where the receive signal of an adapter that `signals` goes: moved by
/// `retarget` (`Link::retarget`), which is asked only of an adapter that
/// signals. Every adapter but a virtio one polled or kept its signal and
/// logged "stays: no MSI-X entry for processor apic=…" on 0.6.1, which read
/// as a missing entry for that processor when the processor was one of the
/// boot's own and the adapter (an I219, an RTL8168, an RTL8156) had no
/// signal to move.
pub fn receive_signal(signals: bool, retarget: impl FnOnce() -> bool) -> ReceiveSignal {
    match (signals, signals && retarget()) {
        (false, _) => ReceiveSignal::Polled,
        (true, true) => ReceiveSignal::Moved,
        (true, false) => ReceiveSignal::Kept,
    }
}

/// The log line for `signal`, of the adapter at `location`, asked to
/// signal processor `apic` on `vector`.
pub fn receive_signal_line(signal: ReceiveSignal, location: &str, apic: u64, vector: u8) -> String {
    match signal {
        ReceiveSignal::Polled => alloc::format!(
            "GUEST NETWORK receive polled: host adapter {location} raises no receive signal, \
             its frames are read at each host service turn\n"
        ),
        ReceiveSignal::Moved => alloc::format!(
            "GUEST NETWORK receive signal moved to processor apic={apic:#x} vector={vector:#x}\n"
        ),
        ReceiveSignal::Kept => alloc::format!(
            "GUEST NETWORK receive signal stays where host adapter {location}'s driver set it: \
             the driver cannot move it to processor apic={apic:#x} vector={vector:#x}\n"
        ),
    }
}

#[cfg(test)]
mod tests;
