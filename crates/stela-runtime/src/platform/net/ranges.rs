//! Reading a remote object by byte ranges, with read-ahead, for readers
//! that jump around it: the restore's install image on a CDN.
//!
//! [`Ranges`] keeps up to [`STREAMS`] streams. Each has one open range,
//! from where its reads are to the object's end (an ordinary download from
//! there), filling a read-ahead buffer: a read at or just past a stream's
//! buffer continues that stream, and a read elsewhere starts a free stream
//! there or, with none free, restarts the least recently read one. So two
//! transfers that read far apart, as a restore that streams the system
//! image while it reads other members does, each keep their own range and
//! read-ahead instead of trading one range between them.
//!
//! [`Ranges::pump`] keeps the buffers filling between reads, [`AHEAD`] in
//! all, split among the streams. A range that fails resumes from its last
//! byte received, a few times. Once [`Ranges::bound`] is set, a read waits
//! at most [`BUDGET_MS`] for the network and otherwise fails as blocked
//! (`ReadAt::blocked`); the same read later goes on from what arrived. The
//! budget counts from the bound, so the many reads one request makes (a
//! deflated member's refills) share it. A
//! restarted stream keeps the bytes its reads used as a segment
//! ([`SEGMENT_BYTES`] in all), so a request that reads several members
//! again from its first finds those it already read.
//!
//! Before the guest runs ([`Ranges::before_guest`]) the reads wait for the
//! network, but no longer than [`QUIET_BEFORE_GUEST_MS`] without a byte, and
//! [`ATTEMPTS_BEFORE_GUEST`] attempts at one range: a boot that streams the
//! image then stops with the cause ([`GaveUp`]) within about two minutes,
//! where it waited out the install's whole budget, a quarter of an hour,
//! with nothing on the screen. [`RangeSource::waiting`] hears each wait.
//! Each range's end is noted with its status, bytes and times.
//!
//! The transport is a [`RangeSource`], so the boot's network and tests
//! serve the same logic.
use super::NetError;
use alloc::{collections::VecDeque, string::String, vec::Vec};
use core::fmt;
use stela_formats::payload::ReadAt;

/// Streams with their own range and read-ahead.
pub const STREAMS: usize = 2;
/// How far [`Ranges::pump`] fills ahead of the reads, all streams together.
pub const AHEAD: usize = 32 << 20;
/// A read up to this far past a stream's buffered bytes continues it.
pub const SKIP: u64 = 8 << 20;
/// Bytes already read that stay buffered in each stream for a re-read.
pub const KEEP: usize = 8 << 20;
/// Room for what one step takes past a stream's read-ahead (about a
/// socket's worth), so its ring rarely grows.
pub const SLACK: usize = 8 << 20;
/// Consecutive failures of one range's progress before a read fails; the
/// waits between them double from a second up to [`BACKOFF_CAP_MS`], about
/// seven and a half minutes in all: a router or Wi-Fi extender that loses
/// power and restarts takes two or three minutes, which a minute's budget
/// turned into a failed install at 97 %.
pub const ATTEMPTS: u32 = 20;
/// The longest wait between two attempts at one range, so that a network
/// that comes back is used again within half a minute.
pub const BACKOFF_CAP_MS: u64 = 30_000;

/// Attempts at one range before the guest runs ([`Ranges::before_guest`]):
/// their waits (1, 2, 4, 8 and 16 s) end within a minute where every
/// attempt fails at once, and [`QUIET_BEFORE_GUEST_MS`] ends them where
/// each waits out its connection's idle timeout.
pub const ATTEMPTS_BEFORE_GUEST: u32 = 6;
/// How long a read before the guest runs goes without a byte from the
/// server before it fails ([`GaveUp::Quiet`]): no install has waited this
/// long for the first bytes of a working server, and a tester's boot waited
/// about ten minutes in silence before he reset it.
pub const QUIET_BEFORE_GUEST_MS: u64 = 120_000;

/// Why the reads before the guest gave up ([`Ranges::gave_up`]), which
/// displays as the stop's plain reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GaveUp {
    /// No byte arrived for `quiet_ms`; `last` is the last attempt's
    /// failure, if one failed.
    Quiet {
        quiet_ms: u64,
        last: Option<NetError>,
    },
    /// Every one of `attempts` attempts at one range failed, the last with
    /// `last`.
    Attempts { attempts: u32, last: NetError },
    /// The server refused in a way no attempt mends, such as a status other
    /// than a partial range or a changed image.
    Refused(NetError),
}

impl fmt::Display for GaveUp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Couldn't download macOS from Apple: ")?;
        match self {
            Self::Quiet { quiet_ms, last } => {
                let seconds = quiet_ms / 1000;
                match seconds % 60 {
                    0 => write!(f, "no data for {} minutes", seconds / 60)?,
                    _ => write!(f, "no data for {seconds} s")?,
                }
                if let Some(last) = last {
                    write!(f, " (the last try: {last})")?;
                }
            }
            Self::Attempts { attempts, last } => {
                write!(f, "{attempts} tries failed, the last with: {last}")?;
            }
            Self::Refused(error) => write!(f, "the server refused: {error}")?,
        }
        f.write_str(". Check the network, or put the install files on the USB with stambha.")
    }
}

/// What a read before the guest runs is waiting for, at each of its waits
/// ([`RangeSource::waiting`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Waiting {
    /// Body bytes every range received so far.
    pub received: u64,
    /// Milliseconds since the read started or last received a byte.
    pub quiet_ms: u64,
    /// The range's failed attempts in a row, and the attempts it has.
    pub failures: u32,
    pub attempts: u32,
    /// Milliseconds until the next attempt, while one is due.
    pub retry_in_ms: Option<u64>,
}

impl Waiting {
    /// The running step's line on the screen: what arrived, how long the
    /// server has been silent, and which try comes next and when.
    pub fn screen(&self) -> String {
        let quiet = self.quiet_ms / 1000;
        let next = self.failures + 1;
        let attempts = self.attempts;
        match (self.retry_in_ms, self.failures) {
            (Some(ms), _) => alloc::format!(
                "try {next}/{attempts} in {} s, no data for {quiet} s",
                ms.div_ceil(1000)
            ),
            (None, 0) if quiet < 2 => alloc::format!("{} read", amount(self.received)),
            (None, 0) => alloc::format!("{} read, no data for {quiet} s", amount(self.received)),
            (None, _) => alloc::format!("try {next}/{attempts}, no data for {quiet} s"),
        }
    }
}

/// `bytes` for the screen, in KB below a megabyte.
fn amount(bytes: u64) -> String {
    match bytes < 1 << 20 {
        true => alloc::format!("{} KB", bytes >> 10),
        false => alloc::format!("{} MB", bytes >> 20),
    }
}

/// The wait before a range's next attempt after `failures` consecutive
/// failures (at least one): a second, doubling, at most [`BACKOFF_CAP_MS`].
fn backoff_ms(failures: u32) -> u64 {
    (1000u64 << failures.saturating_sub(1).min(15)).min(BACKOFF_CAP_MS)
}
/// The longest one wait for an open range's next frame lasts.
pub const WAIT_US: u64 = 100_000;
/// The longest a bounded read waits for the network, and so holds the
/// processor that serves it.
pub const BUDGET_MS: u64 = 20;
/// The bytes kept from restarted streams for reads that come back to them.
pub const SEGMENT_BYTES: usize = 64 << 20;

/// A transport of byte ranges of one object.
pub trait RangeSource {
    /// One open range.
    type Range;
    /// Requests bytes `first..=last`.
    fn open(&mut self, first: u64, last: u64) -> Result<Self::Range, NetError>;
    /// Without waiting, hands what arrived of the range's body to `take`.
    /// Returns whether the range is complete.
    fn read(
        &mut self,
        range: &mut Self::Range,
        take: &mut dyn FnMut(&[u8]),
    ) -> Result<bool, NetError>;
    fn close(&mut self, range: Self::Range);
    /// Waits up to `micros` for the network's next event.
    fn wait(&mut self, micros: u64) -> Result<(), NetError>;
    /// A monotonic millisecond clock.
    fn millis(&mut self) -> u64;
    /// Records a progress line.
    fn note(&mut self, line: fmt::Arguments<'_>);
    /// What the transport knows of `range` as it fails, for the failure's
    /// line: its connection's state, what it carried and when it last did.
    /// Empty where the transport keeps none.
    fn state(&mut self, _range: &Self::Range) -> String {
        String::new()
    }
    /// The HTTP status `range`'s response head carried, once it arrived.
    fn status(&self, _range: &Self::Range) -> Option<u16> {
        None
    }
    /// A read before the guest runs ([`Ranges::before_guest`]) is about to
    /// wait: what it has and waits for, for the screen and the log.
    fn waiting(&mut self, _waiting: Waiting) {}
}

struct Stream<R> {
    /// Object bytes from `start`; the open range continues at their end.
    start: u64,
    buffer: VecDeque<u8>,
    range: Option<R>,
    /// The object offset the open range started at.
    opened: u64,
    /// The open range's number (`Ranges::ranges` when it opened), when it
    /// opened and when its first body byte arrived, for its end's line.
    number: u64,
    opened_ms: u64,
    first_byte_ms: Option<u64>,
    /// The farthest byte a read asked for since the buffer started.
    wanted: u64,
    /// When a read last used the stream, on [`Ranges::tick`].
    used: u64,
    /// Consecutive failures of the open range's progress, and when the
    /// last one was, for a bounded read's backoff.
    failures: u32,
    failed_at: u64,
}

impl<R> Stream<R> {
    fn end(&self) -> u64 {
        self.start + self.buffer.len() as u64
    }

    /// Whether a read at `offset` continues this stream.
    fn holds(&self, offset: u64) -> bool {
        offset >= self.start && offset <= self.end().saturating_add(SKIP)
    }
}

pub struct Ranges<S: RangeSource> {
    source: S,
    len: u64,
    streams: Vec<Stream<S::Range>>,
    /// Counts reads, to find the least recently read stream.
    tick: u64,
    /// Ranges requested so far.
    ranges: u64,
    /// Whether [`Ranges::pump`] reads ahead: from a read that succeeded
    /// until a read-ahead fails.
    ahead: bool,
    /// Earlier buffers, by their first byte, oldest first: each up to
    /// where its reads reached, without what was read ahead past them.
    segments: VecDeque<(u64, Vec<u8>)>,
    /// Whether reads wait for the network at most [`BUDGET_MS`].
    bounded: bool,
    /// When [`Ranges::bound`] was last called: the budget counts from it.
    since: u64,
    /// The last read failed only because its time ran out.
    blocked: bool,
    /// Body bytes every range received.
    received: u64,
    /// Whether the reads are the ones before the guest runs, bounded by
    /// [`QUIET_BEFORE_GUEST_MS`] and [`ATTEMPTS_BEFORE_GUEST`].
    before_guest: bool,
    /// Why a read before the guest gave up, for the stop's reason.
    gave_up: Option<GaveUp>,
}

impl<S: RangeSource> Ranges<S> {
    pub fn new(source: S, len: u64) -> Self {
        Self {
            source,
            len,
            streams: Vec::with_capacity(STREAMS),
            tick: 0,
            ranges: 0,
            ahead: false,
            segments: VecDeque::new(),
            bounded: false,
            since: 0,
            blocked: false,
            received: 0,
            before_guest: false,
            gave_up: None,
        }
    }

    /// The reads from now on are the ones before the guest runs, until
    /// [`Ranges::bound`]: each waits for the network, but fails after
    /// [`QUIET_BEFORE_GUEST_MS`] without a byte or [`ATTEMPTS_BEFORE_GUEST`]
    /// failed attempts at one range, which [`Ranges::gave_up`] then names.
    pub fn before_guest(&mut self) {
        self.before_guest = true;
    }

    /// Why a read before the guest gave up, if one did.
    pub fn gave_up(&self) -> Option<&GaveUp> {
        self.gave_up.as_ref()
    }

    /// Body bytes every range received so far.
    pub fn received(&self) -> u64 {
        self.received
    }

    /// The attempts one range has: fewer before the guest runs.
    fn attempts(&self) -> u32 {
        match self.before_guest {
            true => ATTEMPTS_BEFORE_GUEST,
            false => ATTEMPTS,
        }
    }

    pub fn source(&self) -> &S {
        &self.source
    }

    pub fn source_mut(&mut self) -> &mut S {
        &mut self.source
    }

    /// Ranges requested so far.
    pub fn ranges(&self) -> u64 {
        self.ranges
    }

    /// Bytes held in the streams' buffers.
    pub fn buffered(&self) -> usize {
        self.streams.iter().map(|stream| stream.buffer.len()).sum()
    }

    /// Bytes held in kept segments.
    pub fn kept(&self) -> usize {
        self.segments.iter().map(|(_, bytes)| bytes.len()).sum()
    }

    /// Have the reads from now on wait for the network at most
    /// [`BUDGET_MS`] in all, until the next call: a caller that serves on a
    /// processor whose guest waits bounds each of its turns.
    pub fn bound(&mut self) {
        // The guest runs: the install's own budget applies from now on.
        self.before_guest = false;
        self.bounded = true;
        self.since = self.source.millis();
    }

    /// Closes every range and frees the buffers, returning the capacity
    /// the buffers had.
    pub fn retire(&mut self) -> usize {
        self.ahead = false;
        let mut capacity = 0;
        for mut stream in core::mem::take(&mut self.streams) {
            if let Some(range) = stream.range.take() {
                self.source.close(range);
            }
            capacity += stream.buffer.capacity();
        }
        self.segments = VecDeque::new();
        capacity
    }

    /// Closes stream `at`'s open range, noting how it ended: its HTTP
    /// status, the bytes it got, when the first came and how long it was
    /// open.
    fn stop(&mut self, at: usize) {
        let Some(range) = self.streams[at].range.take() else {
            return;
        };
        let status = self.source.status(&range);
        self.source.close(range);
        let now = self.source.millis();
        let stream = &self.streams[at];
        let got = stream.end() - stream.opened;
        let first = match stream.first_byte_ms {
            Some(first) => alloc::format!("{} ms", first.saturating_sub(stream.opened_ms)),
            None => String::from("none"),
        };
        let status = match status {
            Some(status) => alloc::format!("{status}"),
            None => String::from("none"),
        };
        let (number, open) = (stream.number, now.saturating_sub(stream.opened_ms));
        self.source.note(format_args!(
            "REMOTE IMAGE range {number} ended status={status} got={got} first_byte={first} open={open} ms\n"
        ));
    }

    /// What a failing range of stream `at` got and its transport's state
    /// ([`RangeSource::state`]), for the failure's line, before the range
    /// is closed. A range that never opened got nothing and has no state.
    fn failed(&mut self, at: usize) -> String {
        let stream = &self.streams[at];
        match &stream.range {
            Some(range) => {
                let got = stream.end() - stream.opened;
                let state = self.source.state(range);
                alloc::format!("got={got} {state}")
            }
            None => String::from("not opened"),
        }
    }

    /// The stream a read at `offset` uses: the one it continues, else a new
    /// one while fewer than [`STREAMS`] exist, else the least recently read
    /// one, restarted at `offset`.
    fn choose(&mut self, offset: u64) -> usize {
        if let Some(at) = self.streams.iter().position(|stream| stream.holds(offset)) {
            return at;
        }
        if self.streams.len() < STREAMS {
            // The first stream may read ahead alone; later ones share.
            let ahead = match self.streams.is_empty() {
                true => AHEAD,
                false => AHEAD / STREAMS,
            };
            self.streams.push(Stream {
                start: offset,
                buffer: VecDeque::with_capacity(ahead + KEEP + SLACK),
                range: None,
                opened: offset,
                number: 0,
                opened_ms: 0,
                first_byte_ms: None,
                wanted: offset,
                used: 0,
                failures: 0,
                failed_at: 0,
            });
            return self.streams.len() - 1;
        }
        let at = (0..self.streams.len())
            .min_by_key(|&at| self.streams[at].used)
            .expect("STREAMS is not zero");
        self.stop(at);
        self.keep_segment(at);
        let stream = &mut self.streams[at];
        stream.start = offset;
        stream.wanted = offset;
        stream.failures = 0;
        at
    }

    /// Keep a stream's buffer as a segment before it starts over elsewhere,
    /// dropping the oldest segments past [`SEGMENT_BYTES`].
    fn keep_segment(&mut self, at: usize) {
        let stream = &mut self.streams[at];
        if stream.buffer.is_empty() {
            return;
        }
        let reached = stream.wanted.saturating_sub(stream.start) as usize;
        let mut bytes: Vec<u8> = stream.buffer.drain(..).take(reached).collect();
        bytes.truncate(SEGMENT_BYTES);
        if bytes.is_empty() {
            return;
        }
        self.segments.push_back((stream.start, bytes));
        let mut total = self.kept();
        while total > SEGMENT_BYTES
            && let Some((_, dropped)) = self.segments.pop_front()
        {
            total -= dropped.len();
        }
    }

    /// Copy `out` from a kept segment that holds all of it.
    fn read_segment(&self, offset: u64, out: &mut [u8]) -> bool {
        let found = self.segments.iter().rev().find(|(start, bytes)| {
            offset >= *start && offset + out.len() as u64 <= *start + bytes.len() as u64
        });
        if let Some((start, bytes)) = found {
            let at = (offset - start) as usize;
            out.copy_from_slice(&bytes[at..at + out.len()]);
        }
        found.is_some()
    }

    /// Takes what has arrived for a stream, opening a range at its buffer's
    /// end first when none is open; `now` is the caller's clock reading,
    /// for the range's times.
    fn step(&mut self, at: usize, now: u64) -> Result<(), NetError> {
        if self.streams[at].range.is_none() {
            let (first, last) = (self.streams[at].end(), self.len - 1);
            let range = self.source.open(first, last)?;
            self.ranges += 1;
            let ranges = self.ranges;
            let stream = &mut self.streams[at];
            stream.range = Some(range);
            stream.opened = first;
            stream.number = ranges;
            stream.opened_ms = now;
            stream.first_byte_ms = None;
            self.source.note(format_args!(
                "REMOTE IMAGE range {ranges} stream {at} bytes={first}-{last}\n"
            ));
        }
        let stream = &mut self.streams[at];
        let range = stream.range.as_mut().expect("opened above");
        let buffer = &mut stream.buffer;
        let before = buffer.len();
        let result = self.source.read(range, &mut |body| buffer.extend(body));
        let arrived = (self.streams[at].buffer.len() - before) as u64;
        if arrived > 0 {
            self.received += arrived;
            if self.streams[at].first_byte_ms.is_none() {
                self.streams[at].first_byte_ms = Some(now);
            }
        }
        if result? {
            self.stop(at);
        }
        Ok(())
    }

    /// Fills a stream up to `until`: bounded, within what is left of
    /// [`BUDGET_MS`] since the bound, taking what already arrived even when
    /// none is, and after a failure not before its backoff ends; unbounded,
    /// waiting those out. A failure's count goes on across bounded reads.
    /// Before the guest runs, the read gives up ([`Ranges::gave_up`]) after
    /// [`QUIET_BEFORE_GUEST_MS`] without a byte, [`ATTEMPTS_BEFORE_GUEST`]
    /// failed attempts or a failure no attempt mends, and each of its waits
    /// is told to the transport first ([`RangeSource::waiting`]).
    fn fill(&mut self, at: usize, until: u64) -> Result<(), NetError> {
        let started = match self.bounded {
            true => self.since,
            false => self.source.millis(),
        };
        // When this read last received a byte, and the last attempt's
        // failure since, for the reason it gives up before the guest.
        let (mut heard, mut last) = (started, None);
        let mut looked = false;
        while self.streams[at].end() < until {
            let now = self.source.millis();
            let spent = now.saturating_sub(started);
            if self.bounded && spent >= BUDGET_MS && looked {
                return Err(self.block("the read's time ran out"));
            }
            let quiet_ms = now.saturating_sub(heard);
            if self.before_guest && quiet_ms >= QUIET_BEFORE_GUEST_MS {
                return Err(self.give_up(at, GaveUp::Quiet { quiet_ms, last }));
            }
            looked = true;
            let (failures, failed_at) = (self.streams[at].failures, self.streams[at].failed_at);
            if failures > 0 {
                let backoff = backoff_ms(failures);
                if now.saturating_sub(failed_at) < backoff {
                    if self.bounded {
                        return Err(self.block("the range waits before it is asked again"));
                    }
                    // The network is served through the wait: the other
                    // stream's connection and the link's own traffic go on.
                    loop {
                        let now = self.source.millis();
                        let waited = now.saturating_sub(failed_at);
                        if waited >= backoff {
                            break;
                        }
                        let mut wait = (backoff - waited) * 1000;
                        if self.before_guest {
                            // The look above gives up once the quiet is
                            // spent, before the next attempt is due.
                            let quiet_ms = now.saturating_sub(heard);
                            if quiet_ms >= QUIET_BEFORE_GUEST_MS {
                                break;
                            }
                            wait = wait.min((QUIET_BEFORE_GUEST_MS - quiet_ms) * 1000);
                            self.waiting(at, quiet_ms, Some(backoff - waited));
                        }
                        self.source.wait(wait.min(WAIT_US))?;
                    }
                    continue;
                }
            }
            let before = self.streams[at].end();
            if let Err(error) = self.step(at, now) {
                if self.streams[at].end() > before {
                    heard = now;
                }
                let detail = self.failed(at);
                if !error.retryable() {
                    self.stop(at);
                    self.streams[at].failures = 0;
                    if self.before_guest {
                        self.gave_up = Some(GaveUp::Refused(error.clone()));
                    }
                    return Err(error);
                }
                self.streams[at].failures += 1;
                let failures = self.streams[at].failures;
                let attempts = self.attempts();
                self.source.note(format_args!(
                    "REMOTE IMAGE range at {before} failed ({failures}/{attempts}): {error}; {detail}\n"
                ));
                // The range's own end follows its failure's line.
                self.stop(at);
                if failures >= attempts {
                    self.streams[at].failures = 0;
                    if self.before_guest {
                        self.gave_up = Some(GaveUp::Attempts {
                            attempts,
                            last: error.clone(),
                        });
                    }
                    return Err(error);
                }
                self.streams[at].failed_at = self.source.millis();
                last = Some(error);
            } else if self.streams[at].end() > before {
                self.streams[at].failures = 0;
                heard = now;
            } else if self.streams[at].range.is_some() {
                let wait = match self.bounded {
                    true => BUDGET_MS.saturating_sub(spent) * 1000,
                    false => WAIT_US,
                };
                // A spent budget waits no more; the look above blocks.
                if wait == 0 {
                    continue;
                }
                if self.before_guest {
                    self.waiting(at, quiet_ms, None);
                }
                if let Err(error) = self.source.wait(wait.min(WAIT_US)) {
                    self.stop(at);
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    /// Tells the transport what a read before the guest waits for.
    fn waiting(&mut self, at: usize, quiet_ms: u64, retry_in_ms: Option<u64>) {
        let waiting = Waiting {
            received: self.received,
            quiet_ms,
            failures: self.streams[at].failures,
            attempts: self.attempts(),
            retry_in_ms,
        };
        self.source.waiting(waiting);
    }

    /// A read before the guest gives up for `why`: its range ends, and
    /// [`Ranges::gave_up`] keeps the reason for the stop.
    fn give_up(&mut self, at: usize, why: GaveUp) -> NetError {
        self.stop(at);
        self.streams[at].failures = 0;
        self.gave_up = Some(why);
        NetError::Timeout("no data from the image's server")
    }

    /// A bounded read that ran out of time: the ranges keep filling between
    /// requests, and the same read later goes on.
    fn block(&mut self, why: &'static str) -> NetError {
        self.blocked = true;
        self.ahead = true;
        NetError::Timeout(why)
    }

    /// Without waiting, takes what has arrived for each stream while its
    /// share of [`AHEAD`] is not buffered, opening the next range when one
    /// ends. A failure is noted and leaves the next read to retry.
    pub fn pump(&mut self) {
        if !self.ahead || self.streams.is_empty() {
            return;
        }
        let share = AHEAD / self.streams.len();
        for at in 0..self.streams.len() {
            let stream = &self.streams[at];
            if stream.buffer.len() >= share || stream.end() >= self.len {
                continue;
            }
            let now = self.source.millis();
            if let Err(error) = self.step(at, now) {
                let (end, detail) = (self.streams[at].end(), self.failed(at));
                self.source.note(format_args!(
                    "REMOTE IMAGE read-ahead range at {end} failed: {error}; {detail}\n"
                ));
                self.stop(at);
                self.ahead = false;
                return;
            }
        }
    }
}

impl<S: RangeSource> ReadAt for Ranges<S> {
    type Error = NetError;

    fn len(&self) -> u64 {
        self.len
    }

    fn blocked(&mut self) -> bool {
        core::mem::take(&mut self.blocked)
    }

    fn read_at(&mut self, offset: u64, out: &mut [u8]) -> Result<(), NetError> {
        // The reason names the last read's failure, not one a caller
        // already got past.
        self.gave_up = None;
        let until = offset
            .checked_add(out.len() as u64)
            .ok_or(NetError::Invalid("remote image offset overflow"))?;
        if until > self.len {
            return Err(NetError::Invalid("read past the remote image's end"));
        }
        if self.read_segment(offset, out) {
            return Ok(());
        }
        let at = self.choose(offset);
        self.tick += 1;
        let stream = &mut self.streams[at];
        stream.used = self.tick;
        stream.wanted = stream.wanted.max(until);
        self.fill(at, until)?;
        self.ahead = true;
        let stream = &mut self.streams[at];
        // The ring's two halves, from the read's first byte.
        let (mut skip, mut filled) = ((offset - stream.start) as usize, 0);
        for part in <[&[u8]; 2]>::from(stream.buffer.as_slices()) {
            if skip < part.len() {
                let take = (part.len() - skip).min(out.len() - filled);
                out[filled..filled + take].copy_from_slice(&part[skip..skip + take]);
                filled += take;
                skip = 0;
            } else {
                skip -= part.len();
            }
        }
        // Bytes before this read are done with, but the last few stay for
        // a re-read.
        let done = (until - stream.start) as usize;
        if done > KEEP {
            stream.buffer.drain(..done - KEEP);
            stream.start = until - KEEP as u64;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
