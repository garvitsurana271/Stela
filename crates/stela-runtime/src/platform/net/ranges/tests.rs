use super::*;
use alloc::collections::VecDeque;

const GIB: u64 = 1 << 30;
const LEN: u64 = 30 * GIB;
const READ: usize = 256 << 10;

/// The object's byte at `at`, so every read can be checked.
fn byte(at: u64) -> u8 {
    (at ^ (at >> 11) ^ (at >> 23) ^ (at >> 31)) as u8
}

struct Range {
    next: u64,
    last: u64,
}

/// An object served by ranges: each read hands over up to `chunk` bytes
/// unless stalled; a clock that moves a millisecond per look and by each
/// wait.
struct Mock {
    chunk: usize,
    stalled: bool,
    opens: Vec<u64>,
    notes: Vec<String>,
    waits: usize,
    live: usize,
    open_errors: VecDeque<NetError>,
    read_errors: VecDeque<NetError>,
    now: u64,
    /// A read hands over bytes at most once in this many milliseconds.
    trickle_ms: u64,
    next_at: u64,
    /// The clock past which a test's read has waited too long: a read that
    /// never gives up fails the test instead of hanging it.
    give_up_by: u64,
    waitings: Vec<Waiting>,
}

impl Mock {
    fn new() -> Self {
        Self {
            chunk: 1 << 20,
            stalled: false,
            opens: Vec::new(),
            notes: Vec::new(),
            waits: 0,
            live: 0,
            open_errors: VecDeque::new(),
            read_errors: VecDeque::new(),
            now: 0,
            trickle_ms: 0,
            next_at: 0,
            give_up_by: u64::MAX,
            waitings: Vec::new(),
        }
    }
}

impl RangeSource for Mock {
    type Range = Range;

    fn open(&mut self, first: u64, last: u64) -> Result<Range, NetError> {
        if let Some(error) = self.open_errors.pop_front() {
            return Err(error);
        }
        self.opens.push(first);
        self.live += 1;
        Ok(Range { next: first, last })
    }

    fn read(&mut self, range: &mut Range, take: &mut dyn FnMut(&[u8])) -> Result<bool, NetError> {
        if let Some(error) = self.read_errors.pop_front() {
            return Err(error);
        }
        if self.stalled || self.now < self.next_at {
            return Ok(false);
        }
        self.next_at = self.now + self.trickle_ms;
        let n = (self.chunk as u64).min(range.last + 1 - range.next);
        let bytes: Vec<u8> = (range.next..range.next + n).map(byte).collect();
        range.next += n;
        take(&bytes);
        Ok(range.next > range.last)
    }

    fn close(&mut self, _: Range) {
        self.live -= 1;
    }

    fn wait(&mut self, micros: u64) -> Result<(), NetError> {
        self.now += (micros / 1000).max(1);
        self.waits += 1;
        assert!(self.now < self.give_up_by, "the read never gave up");
        Ok(())
    }

    fn millis(&mut self) -> u64 {
        self.now += 1;
        self.now
    }

    fn note(&mut self, line: fmt::Arguments<'_>) {
        self.notes.push(alloc::format!("{line}"));
    }

    fn state(&mut self, range: &Range) -> String {
        alloc::format!("next={}", range.next)
    }

    fn status(&self, _: &Range) -> Option<u16> {
        Some(206)
    }

    fn waiting(&mut self, waiting: Waiting) {
        self.waitings.push(waiting);
    }
}

fn ranges() -> Ranges<Mock> {
    Ranges::new(Mock::new(), LEN)
}

fn read(reader: &mut Ranges<Mock>, offset: u64, len: usize) -> Result<(), NetError> {
    let mut out = vec![0; len];
    reader.read_at(offset, &mut out)?;
    assert!(
        out.iter().zip(offset..).all(|(&got, at)| got == byte(at)),
        "bytes at {offset}"
    );
    Ok(())
}

fn bounded_memory(reader: &Ranges<Mock>) {
    assert!(reader.buffered() <= AHEAD + STREAMS * (KEEP + SLACK));
    assert!(reader.kept() <= SEGMENT_BYTES);
    assert!(reader.source().live <= STREAMS);
}

#[test]
fn two_interleaved_sequential_readers_keep_their_two_ranges() {
    let mut reader = ranges();
    let (mut system, mut other) = (2 * GIB, 24 * GIB);
    for _ in 0..400 {
        read(&mut reader, system, READ).unwrap();
        reader.pump();
        read(&mut reader, other, READ / 4).unwrap();
        reader.pump();
        bounded_memory(&reader);
        system += READ as u64;
        // The other reader skips a little now and then.
        other += READ as u64 / 4 + 4096;
    }
    assert_eq!(reader.ranges(), 2);
    assert_eq!(reader.source().opens, [2 * GIB, 24 * GIB]);
}

#[test]
fn a_third_far_reader_replaces_the_least_recently_read_stream() {
    let mut reader = ranges();
    let (a, b, c) = (2 * GIB, 24 * GIB, 10 * GIB);
    read(&mut reader, a, READ).unwrap();
    read(&mut reader, b, READ).unwrap();
    read(&mut reader, a + READ as u64, READ).unwrap();
    // `b` was read least recently.
    read(&mut reader, c, READ).unwrap();
    assert_eq!(reader.source().opens, [a, b, c]);
    // `a` goes on in its own range.
    read(&mut reader, a + 2 * READ as u64, READ).unwrap();
    assert_eq!(reader.ranges(), 3);
    // What `b`'s reads used stays as a segment; past it needs a range,
    // which replaces `c`, now the least recently read.
    read(&mut reader, b, READ).unwrap();
    assert_eq!(reader.ranges(), 3);
    read(&mut reader, b + READ as u64, READ).unwrap();
    assert_eq!(reader.source().opens, [a, b, c, b + READ as u64]);
    // `a` still goes on in its own range.
    read(&mut reader, a + 3 * READ as u64, READ).unwrap();
    assert_eq!(reader.ranges(), 4);
    bounded_memory(&reader);
}

#[test]
fn memory_stays_bounded_while_readers_jump_around() {
    let mut reader = ranges();
    for round in 0..64u64 {
        let base = (round * 7 % 29) * GIB;
        for step in 0..8 {
            read(&mut reader, base + step * READ as u64, READ).unwrap();
            for _ in 0..8 {
                reader.pump();
            }
            bounded_memory(&reader);
        }
    }
    assert!(reader.retire() > 0);
    assert_eq!(reader.buffered(), 0);
    assert_eq!(reader.source().live, 0);
}

#[test]
fn a_refused_range_passes_its_error_through() {
    let mut reader = ranges();
    reader
        .source_mut()
        .open_errors
        .push_back(NetError::Status(404));
    assert_eq!(read(&mut reader, GIB, READ), Err(NetError::Status(404)));
    // The next read asks again.
    read(&mut reader, GIB, READ).unwrap();
    assert_eq!(reader.ranges(), 1);
}

#[test]
fn a_dropped_range_resumes_from_its_last_byte() {
    let mut reader = ranges();
    read(&mut reader, GIB, READ).unwrap();
    reader.source_mut().read_errors.push_back(NetError::Reset);
    read(&mut reader, GIB + (2 << 20), READ).unwrap();
    let opens = &reader.source().opens;
    assert_eq!(opens.len(), 2);
    assert!(opens[1] > GIB, "resumed at {}", opens[1]);
    assert_eq!(reader.source().live, 1);
}

#[test]
fn a_bounded_read_that_waits_too_long_is_pending_and_goes_on_later() {
    let mut reader = ranges();
    reader.bound();
    reader.source_mut().stalled = true;
    assert!(matches!(
        read(&mut reader, 3 * GIB, READ),
        Err(NetError::Timeout(_))
    ));
    assert!(reader.blocked());
    assert!(!reader.blocked(), "asking clears it");
    // The range stays open and fills between requests.
    reader.source_mut().stalled = false;
    reader.pump();
    read(&mut reader, 3 * GIB, READ).unwrap();
    assert_eq!(reader.ranges(), 1);
    assert!(!reader.blocked());
}

#[test]
fn a_bounded_read_after_a_failure_waits_out_its_backoff_as_pending() {
    let mut reader = ranges();
    reader.bound();
    reader.source_mut().open_errors.push_back(NetError::Reset);
    assert!(matches!(
        read(&mut reader, GIB, READ),
        Err(NetError::Timeout(_))
    ));
    assert!(reader.blocked());
    reader.source_mut().now += 1000;
    read(&mut reader, GIB, READ).unwrap();
    assert_eq!(reader.ranges(), 1);
}

#[test]
fn a_bounded_turns_reads_share_one_budget() {
    // A trickling network: each look hands over 4 KiB and takes a
    // millisecond, so one 64 KiB read fits the budget but a request's run
    // of such reads, a deflated member's refills, does not.
    const PIECE: usize = 64 << 10;
    let mut reader = ranges();
    reader.source_mut().chunk = 4 << 10;
    reader.bound();
    let started = reader.source().now;
    let mut offset = GIB;
    let mut done = 0;
    let blocked = loop {
        match read(&mut reader, offset, PIECE) {
            Ok(()) => {
                done += 1;
                offset += PIECE as u64;
            }
            Err(error) => break error,
        }
        assert!(done < 16, "the reads never ran out of time");
    };
    assert!(matches!(blocked, NetError::Timeout(_)));
    assert!(reader.blocked());
    assert!(done <= 1, "{done} reads after the budget was spent");
    let held = reader.source().now - started;
    assert!(held <= BUDGET_MS + 2, "held the processor {held} ms");
    // The next turn goes on with the same read from what arrived.
    reader.bound();
    read(&mut reader, offset, PIECE).unwrap();
    assert_eq!(reader.ranges(), 1);
}

#[test]
fn reads_past_the_end_are_refused() {
    let mut reader = ranges();
    assert!(matches!(
        read(&mut reader, LEN - 10, 11),
        Err(NetError::Invalid(_))
    ));
    read(&mut reader, LEN - 10, 10).unwrap();
}

/// The waits between a range's attempts double from a second to at most
/// 30 s, and all of them together outlast a router's or Wi-Fi extender's
/// restart (two or three minutes), where seven attempts gave up after about
/// a minute and ended an install at 97 %.
#[test]
fn a_ranges_retries_outlast_a_router_restart() {
    assert_eq!(backoff_ms(1), 1000);
    assert_eq!(backoff_ms(2), 2000);
    assert_eq!(backoff_ms(5), 16_000);
    assert_eq!(backoff_ms(6), BACKOFF_CAP_MS);
    assert_eq!(backoff_ms(ATTEMPTS), BACKOFF_CAP_MS);
    assert_eq!(backoff_ms(u32::MAX), BACKOFF_CAP_MS);
    // A wait follows each failure but the last, which fails the read.
    let total: u64 = (1..ATTEMPTS).map(backoff_ms).sum();
    assert!(total >= 5 * 60 * 1000, "{total} ms");
    assert!(total <= 10 * 60 * 1000, "{total} ms");
}

/// An unbounded read waits out the backoff after a failed range through
/// the transport, which serves the network meanwhile (the other stream's
/// connection, the link's own traffic), where it spun without a look at
/// the network for up to 32 s.
#[test]
fn an_unbounded_reads_backoff_serves_the_network() {
    let mut reader = ranges();
    reader.source_mut().chunk = 4096;
    read(&mut reader, 0, 6000).unwrap();
    reader
        .source_mut()
        .read_errors
        .push_back(NetError::Timeout("TCP transfer"));
    let waits = reader.source().waits;
    read(&mut reader, 6000, 6000).unwrap();
    let source = reader.source();
    // A second of backoff in waits of at most 100 ms.
    assert!(source.waits - waits >= 10, "{}", source.waits - waits);
    assert_eq!(source.opens, [0, 8192]);
}

/// Each failed range's line says what the range got and its transport's
/// state then, and a read-ahead that fails is noted too, where it stopped
/// without a line.
#[test]
fn a_failed_range_says_what_it_got_and_a_failed_read_ahead_is_noted() {
    let mut reader = ranges();
    reader.source_mut().chunk = 4096;
    read(&mut reader, 0, 6000).unwrap();
    let failed = |reader: &Ranges<Mock>| {
        let notes = &reader.source().notes;
        notes.iter().filter(|note| note.contains("failed")).count()
    };
    assert_eq!(failed(&reader), 0);
    reader
        .source_mut()
        .read_errors
        .push_back(NetError::Timeout("TCP transfer"));
    read(&mut reader, 6000, 6000).unwrap();
    let source = reader.source();
    let line = alloc::format!(
        "REMOTE IMAGE range at 8192 failed (1/{ATTEMPTS}): timed out: TCP transfer; got=8192 next=8192\n"
    );
    assert!(source.notes.contains(&line), "{:?}", source.notes);
    // The read-ahead's failure is said and the next read retries it.
    reader.source_mut().read_errors.push_back(NetError::Reset);
    reader.pump();
    assert_eq!(failed(&reader), 2);
    let source = reader.source();
    // Its line is followed by the range's end (`Ranges::stop`).
    let note = &source.notes[source.notes.len() - 2];
    assert!(
        source.notes.last().unwrap().contains(" ended status="),
        "{:?}",
        source.notes
    );
    assert!(
        note.starts_with("REMOTE IMAGE read-ahead range at ")
            && note.contains(" failed: ")
            && note.contains("; got="),
        "{note}"
    );
    assert_eq!(source.live, 0);
    read(&mut reader, 12000, 100).unwrap();
}

/// The reads before the guest give up after two minutes without a byte,
/// with the stop's plain reason, where a silent server held the boot for
/// the install's whole budget (about a quarter of an hour) with nothing on
/// the screen or in the log.
#[test]
fn before_the_guest_a_silent_server_gives_up_after_two_minutes() {
    let mut reader = ranges();
    reader.before_guest();
    reader.source_mut().stalled = true;
    reader.source_mut().give_up_by = 20 * 60 * 1000;
    let started = reader.source().now;
    assert!(read(&mut reader, GIB, READ).is_err());
    let waited = reader.source().now - started;
    assert!(
        (QUIET_BEFORE_GUEST_MS..QUIET_BEFORE_GUEST_MS + 1000).contains(&waited),
        "gave up after {waited} ms"
    );
    let gave_up = reader.gave_up().expect("a reason").clone();
    assert!(
        matches!(gave_up, GaveUp::Quiet { last: None, .. }),
        "{gave_up:?}"
    );
    assert_eq!(
        alloc::format!("{gave_up}"),
        "Couldn't download macOS from Apple: no data for 2 minutes. Check the network, or put \
         the install files on the USB with stambha."
    );
    // Each wait was told, its quiet growing to the limit.
    let source = reader.source();
    let quiet: Vec<u64> = source.waitings.iter().map(|w| w.quiet_ms).collect();
    assert!(quiet.windows(2).all(|pair| pair[0] <= pair[1]));
    assert!(*quiet.last().unwrap() >= QUIET_BEFORE_GUEST_MS - 200);
    // The range's end was noted, with no first byte.
    assert!(
        source.notes.iter().any(|note| note
            .starts_with("REMOTE IMAGE range 1 ended status=206 got=0 first_byte=none open=")),
        "{:?}",
        source.notes
    );
    assert_eq!(source.live, 0);
}

/// A range that fails at once each time gives up after six tries before the
/// guest, and each try's line and wait say so ("1/6"), where twenty tries
/// over seven and a half minutes ran first.
#[test]
fn before_the_guest_failing_tries_give_up_after_six() {
    let mut reader = ranges();
    reader.before_guest();
    reader.source_mut().give_up_by = 5 * 60 * 1000;
    for _ in 0..ATTEMPTS {
        reader.source_mut().open_errors.push_back(NetError::Reset);
    }
    assert_eq!(read(&mut reader, GIB, READ), Err(NetError::Reset));
    assert_eq!(
        reader.gave_up(),
        Some(&GaveUp::Attempts {
            attempts: ATTEMPTS_BEFORE_GUEST,
            last: NetError::Reset
        })
    );
    let source = reader.source();
    assert!(source.now < 60_000, "gave up after {} ms", source.now);
    for attempt in 1..=ATTEMPTS_BEFORE_GUEST {
        let line = alloc::format!(
            "REMOTE IMAGE range at {GIB} failed ({attempt}/{ATTEMPTS_BEFORE_GUEST}): "
        );
        assert!(
            source.notes.iter().any(|note| note.starts_with(&line)),
            "{line} in {:?}",
            source.notes
        );
    }
    // The waits before each next try say which it is and when it comes.
    let retry = source
        .waitings
        .iter()
        .find(|w| w.failures == 3)
        .expect("a wait after the third failure");
    assert_eq!(retry.attempts, ATTEMPTS_BEFORE_GUEST);
    assert!(retry.retry_in_ms.is_some_and(|ms| ms <= backoff_ms(3)));
    let reason = alloc::format!("{}", reader.gave_up().unwrap());
    assert!(
        reason.starts_with("Couldn't download macOS from Apple: 6 tries failed, the last with: "),
        "{reason}"
    );
}

/// A server that sends a little at a time keeps the reads before the guest
/// going for longer than two minutes in all: only silence ends them.
#[test]
fn before_the_guest_a_slow_server_is_waited_for() {
    let mut reader = ranges();
    reader.before_guest();
    reader.source_mut().chunk = 4096;
    reader.source_mut().trickle_ms = 10_000;
    read(&mut reader, 0, 16 * 4096).unwrap();
    assert!(reader.source().now > QUIET_BEFORE_GUEST_MS);
    assert_eq!(reader.gave_up(), None);
    assert_eq!(reader.received(), 16 * 4096);
}

/// A refusal no try mends is named for the stop before the guest.
#[test]
fn before_the_guest_a_refusal_is_named() {
    let mut reader = ranges();
    reader.before_guest();
    reader
        .source_mut()
        .open_errors
        .push_back(NetError::Status(404));
    assert_eq!(read(&mut reader, GIB, READ), Err(NetError::Status(404)));
    assert_eq!(
        reader.gave_up(),
        Some(&GaveUp::Refused(NetError::Status(404)))
    );
}

/// Once the guest runs (the first bound), the install's own twenty tries
/// apply again and the transport hears no waits.
#[test]
fn once_the_guest_runs_the_install_budget_applies() {
    let mut reader = ranges();
    reader.before_guest();
    reader.bound();
    reader.source_mut().open_errors.push_back(NetError::Reset);
    assert!(matches!(
        read(&mut reader, GIB, READ),
        Err(NetError::Timeout(_))
    ));
    let source = reader.source();
    let line = alloc::format!("REMOTE IMAGE range at {GIB} failed (1/{ATTEMPTS}): ");
    assert!(
        source.notes.iter().any(|note| note.starts_with(&line)),
        "{:?}",
        source.notes
    );
    assert!(source.waitings.is_empty());
    assert_eq!(reader.gave_up(), None);
}

/// Each range's end is noted with its status, the bytes it got, when its
/// first byte came and how long it was open.
#[test]
fn a_ranges_end_says_its_status_bytes_and_times() {
    let mut reader = ranges();
    reader.source_mut().chunk = 4096;
    read(&mut reader, 0, 6000).unwrap();
    reader.source_mut().read_errors.push_back(NetError::Reset);
    read(&mut reader, 6000, 6000).unwrap();
    let source = reader.source();
    let ended = source
        .notes
        .iter()
        .find(|note| note.starts_with("REMOTE IMAGE range 1 ended "))
        .expect("the first range's end");
    assert!(
        ended.starts_with("REMOTE IMAGE range 1 ended status=206 got=8192 first_byte=0 ms open=")
            && ended.ends_with(" ms\n"),
        "{ended}"
    );
    assert_eq!(reader.received(), 12288);
}

/// The step's line names what arrived, the silence and the next try.
#[test]
fn a_waits_screen_line_names_the_silence_and_the_next_try() {
    let waiting = |received, quiet_ms, failures, retry_in_ms| {
        Waiting {
            received,
            quiet_ms,
            failures,
            attempts: ATTEMPTS_BEFORE_GUEST,
            retry_in_ms,
        }
        .screen()
    };
    assert_eq!(waiting(512 << 10, 300, 0, None), "512 KB read");
    assert_eq!(waiting(3 << 20, 300, 0, None), "3 MB read");
    assert_eq!(waiting(0, 31_500, 0, None), "0 KB read, no data for 31 s");
    assert_eq!(
        waiting(0, 45_000, 2, Some(3_200)),
        "try 3/6 in 4 s, no data for 45 s"
    );
    assert_eq!(waiting(0, 46_000, 2, None), "try 3/6, no data for 46 s");
}

/// The reason names the last read's failure: a read that succeeds after
/// one that gave up leaves none, so a later, unrelated stop is not told as
/// a download that gave up.
#[test]
fn a_read_after_one_that_gave_up_clears_the_reason() {
    let mut reader = ranges();
    reader.before_guest();
    reader
        .source_mut()
        .open_errors
        .push_back(NetError::Status(404));
    assert!(read(&mut reader, GIB, READ).is_err());
    assert!(reader.gave_up().is_some());
    read(&mut reader, GIB, READ).unwrap();
    assert_eq!(reader.gave_up(), None);
}
