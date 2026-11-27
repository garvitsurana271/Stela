//! A remote install image (`payload::Layout::Remote`): the IPSW on Apple's
//! CDN, read by URL-aware byte ranges over the mount's own network. HTTPS
//! requires the native owner's admitted TLS time and entropy capabilities.
//!
//! Each request carries `If-Range` with the entity tag setup recorded, so a
//! changed object fails instead of mixing two versions; the restore store
//! then checks the bytes as the manifest says (`payload::Member::sha256`).
//! The ranges, their read-ahead and the bounded reads of a PE that serves
//! the restore host are `stela_runtime::net::ranges::Ranges`; this is only
//! its transport over the mount's network and serial output.
//!
//! Before the guest runs, the boot processor reads the image's directory
//! and what the restore starts with (`restore::Experiment::new`) under its
//! own step ("Opening the macOS install image from Apple"): each wait then
//! moves the step's line on the screen (try k/N while a range waits for its
//! next attempt), stores the logs when due (`output::flush_due`), and tells
//! the step watchdog the boot goes on; each failure stores the logs at once
//! with the adapter's state. The reads give up as `Ranges::before_guest`
//! says, and the boot stops with [`Remote::gave_up`]'s plain reason.
use super::network::Shared;
use crate::Result;
use crate::output;
use alloc::string::String;
use stela_contracts::refuse;
use {
    stela_formats::payload::Layout,
    stela_formats::payload::PayloadManifest,
    stela_formats::payload::ReadAt,
    stela_runtime::net::HttpTransfer,
    stela_runtime::net::NetError,
    stela_runtime::net::http::Ranged,
    stela_runtime::net::ranges::{GaveUp, RangeSource, Ranges, Waiting},
};

/// The most often the screen's line moves while a read before the guest
/// waits, in milliseconds.
const DRAW_MS: u64 = 250;
/// While no byte arrives before the guest, the adapter's frame counts and
/// state are logged every this many milliseconds of the silence.
const QUIET_NOTE_MS: u64 = 10_000;

/// Ranges of one URL whose entity tag setup recorded.
pub struct Http {
    network: Shared,
    url: String,
    etag: String,
    len: u64,
    /// A range failed since the last note: its line is followed by the
    /// adapter's state.
    failed: bool,
    /// The reads are the boot processor's before the guest runs, until the
    /// first bound ([`Remote::bound`]).
    before_guest: bool,
    /// When the screen's line last moved, and what the last wait said.
    drawn_ms: u64,
    last: Waiting,
    /// The silence's next mark for the adapter's line.
    quiet_note_ms: u64,
}

impl Http {
    /// The adapter's frame counts and its own state line, for the log.
    fn adapter(&mut self) -> String {
        let Ok(mut network) = self.network.0.try_borrow_mut() else {
            return String::from("network in use");
        };
        let (sent, received) = network.frames();
        match network.diagnostic() {
            Some(state) => {
                alloc::format!("frames sent={sent} received={received}; adapter state: {state}")
            }
            None => alloc::format!("frames sent={sent} received={received}"),
        }
    }
}

impl RangeSource for Http {
    type Range = (HttpTransfer, Ranged);

    fn open(&mut self, first: u64, last: u64) -> Result<Self::Range, NetError> {
        let ranged = Ranged::with_identity(first, last, &self.etag, self.len)?;
        let transfer = self
            .network
            .0
            .borrow_mut()
            .download(&self.url, first, last, &self.etag)?;
        Ok((transfer, ranged))
    }

    fn read(
        &mut self,
        (transfer, ranged): &mut Self::Range,
        take: &mut dyn FnMut(&[u8]),
    ) -> Result<bool, NetError> {
        self.network
            .0
            .borrow_mut()
            .read_download(transfer, |bytes, closed| {
                ranged.feed(bytes, closed, &mut *take)
            })
    }

    fn close(&mut self, (transfer, _): Self::Range) {
        self.network.0.borrow_mut().close_download(transfer);
    }

    fn wait(&mut self, micros: u64) -> Result<(), NetError> {
        let result = {
            let mut network = self.network.0.borrow_mut();
            let result = network.wait(micros);
            // The stack keeps a bounded number of these, so the lines stay
            // few.
            for note in network.take_notes() {
                output::arguments(format_args!("REMOTE IMAGE network {note}\n"));
            }
            result
        };
        if self.before_guest {
            // No service stores the logs before the guest runs: a wait
            // that never ends otherwise leaves none of its lines.
            output::flush_due();
            super::step_watchdog::progress();
        }
        result
    }

    fn waiting(&mut self, waiting: Waiting) {
        if !self.before_guest {
            return;
        }
        let now = output::millis_since(0);
        let failed = waiting.failures > self.last.failures;
        let resumed = waiting.quiet_ms < self.last.quiet_ms;
        self.last = waiting;
        if resumed {
            self.quiet_note_ms = QUIET_NOTE_MS;
        }
        if failed {
            // A new failure: its line is in the log at once, and the try
            // the screen names changes now.
            if let Some(retry) = waiting.retry_in_ms {
                output::arguments(format_args!(
                    "REMOTE IMAGE next try {}/{} in {} ms, no data for {} ms, received={}: {}\n",
                    waiting.failures + 1,
                    waiting.attempts,
                    retry,
                    waiting.quiet_ms,
                    waiting.received,
                    self.adapter()
                ));
            }
            output::flush_log();
        } else if waiting.quiet_ms >= self.quiet_note_ms {
            self.quiet_note_ms =
                waiting.quiet_ms - waiting.quiet_ms % QUIET_NOTE_MS + QUIET_NOTE_MS;
            output::arguments(format_args!(
                "REMOTE IMAGE no data for {} s, received={}: {}\n",
                waiting.quiet_ms / 1000,
                waiting.received,
                self.adapter()
            ));
        }
        if failed || now.saturating_sub(self.drawn_ms) >= DRAW_MS {
            self.drawn_ms = now;
            crate::status::progress(&waiting.screen());
        }
    }

    fn millis(&mut self) -> u64 {
        output::millis_since(0)
    }

    fn note(&mut self, line: core::fmt::Arguments<'_>) {
        output::arguments(line);
        // Each failed range's line is followed by what the adapter's
        // registers, rings and PHY show then: a stalled receive or transmit
        // path is told apart from a slow server. A range fails at most a
        // few times a minute (`Ranges`' backoff), so the lines stay few.
        if core::mem::take(&mut self.failed)
            && let Ok(mut network) = self.network.0.try_borrow_mut()
            && let Some(state) = network.diagnostic()
        {
            output::arguments(format_args!("REMOTE IMAGE adapter state: {state}\n"));
        }
        if self.before_guest {
            // Before the guest runs, a blocking store costs nothing: each
            // range's line is on the stick and the disk before its wait.
            output::flush_log();
        }
    }

    fn status(&self, (_, ranged): &Self::Range) -> Option<u16> {
        ranged.status
    }

    fn state(&mut self, (transfer, _): &Self::Range) -> String {
        self.failed = true;
        match self.network.0.try_borrow_mut() {
            Ok(mut network) => network.download_state(transfer),
            Err(_) => String::from("network in use"),
        }
    }
}

pub struct Remote(Ranges<Http>);

impl Remote {
    pub fn new(network: Shared, manifest: &PayloadManifest) -> Result<Self> {
        let Layout::Remote { url, etag } = &manifest.layout else {
            return Err(refuse!("the install payload is not a remote image"));
        };
        network
            .0
            .borrow()
            .check_url(url)
            .map_err(|error| refuse!("remote image URL: {error}", %error))?;
        let last = manifest
            .source
            .bytes
            .checked_sub(1)
            .ok_or_else(|| refuse!("the remote image is empty"))?;
        Ranged::with_identity(0, last, etag, manifest.source.bytes)
            .map_err(|error| refuse!("remote image range: {error}", %error))?;
        output::arguments(format_args!(
            "REMOTE IMAGE {url} bytes={} etag={etag}\n",
            manifest.source.bytes
        ));
        let http = Http {
            network,
            url: url.clone(),
            etag: etag.clone(),
            len: manifest.source.bytes,
            failed: false,
            before_guest: true,
            drawn_ms: 0,
            last: Waiting::default(),
            quiet_note_ms: QUIET_NOTE_MS,
        };
        let mut ranges = Ranges::new(http, manifest.source.bytes);
        ranges.before_guest();
        Ok(Self(ranges))
    }

    /// Have the reads from now on wait for the network at most
    /// `ranges::BUDGET_MS` in all, until the next call: the restore host
    /// calls it at each of its turns on a PE. The first ends the reads
    /// before the guest ([`Ranges::before_guest`]).
    pub fn bound(&mut self) {
        self.0.source_mut().before_guest = false;
        self.0.bound();
    }

    /// Why the reads before the guest gave up, if they did: the stop's
    /// plain reason.
    pub fn gave_up(&self) -> Option<&GaveUp> {
        self.0.gave_up()
    }

    /// Body bytes the image's ranges received so far.
    pub fn received(&self) -> u64 {
        self.0.received()
    }

    /// Release the restore-only ranges and buffers after the protocol has
    /// ended.
    pub(super) fn retire(&mut self) {
        let bytes = self.0.retire();
        output::arguments(format_args!(
            "REMOTE IMAGE retired read-ahead capacity={bytes} ranges={}\n",
            self.0.ranges()
        ));
    }

    /// Without waiting, takes what has arrived for the open ranges.
    pub fn pump(&mut self) {
        self.0.pump();
    }
}

impl ReadAt for Remote {
    type Error = NetError;

    fn len(&self) -> u64 {
        self.0.len()
    }

    fn blocked(&mut self) -> bool {
        self.0.blocked()
    }

    fn read_at(&mut self, offset: u64, out: &mut [u8]) -> Result<(), NetError> {
        self.0.read_at(offset, out)
    }
}
