//! The Wi-Fi picker after the firmware exits: "Choose a network", with the
//! networks a scan found, when none of the boot's own (`--wifi` and
//! `wifi.saved`) is in range, or when Option was held. The system engine
//! reads the scan from the Wi-Fi adapter, the keys from the host's
//! keyboards (`hid_keys`), and paints the [`Scene`] in the startup menu's
//! look; what the picker shows, how keys change it and when it shows at
//! all ([`decide`]) are here, where the host tests reach them.
//!
//! Each network the scan found is one row: its name, a lock, "saved" for
//! one of the boot's own, and one to four signal bars ([`bars`]), the
//! strongest first. Networks Stambha cannot join (open or enterprise ones)
//! are one line's count, and "Join other network..." takes a hidden one's
//! name. Enter on a network asks for its password, masked unless Tab shows
//! it; a saved one joins with its saved key. A failed join asks again, with
//! why. Esc, or a minute with no key, goes on without Wi-Fi.
//!
//! No name or password reaches a log line ([`Gate::line`], [`line`]) or a
//! `Debug` form; passwords are wiped as they shrink and when dropped.
use crate::{
    wifi_page::{
        self, CARD, Content, Field, Form, Geometry, Key, Layout, ROW, Rect, Save, Scene, Tone,
    },
    wifi_saved,
};
use alloc::{format, string::String, vec::Vec};
use stela_drivers::net::wifi::network::{Secret, SecretError};

/// A minute with no key goes on without Wi-Fi.
pub const IDLE_TIMEOUT_MS: u64 = 60_000;
/// The footer counts the last half minute down.
pub const COUNTDOWN_MS: u64 = 30_000;
/// The most networks listed, the strongest.
pub const MAX_CHOICES: usize = 32;
/// Failed joins after which the dialog says how to get back to the list.
pub const FAILURES_BEFORE_HINT: u32 = 3;

/// What a BSS offers, as the adapter's scan tells it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// WPA2 or WPA3 Personal, which a password joins.
    Joinable,
    /// No security: Stambha joins only networks with a key.
    Open,
    /// Enterprise or anything else the station does not do.
    Unsupported,
}

/// One BSS a scan found.
#[derive(Clone, PartialEq, Eq)]
pub struct Nearby {
    pub ssid: Vec<u8>,
    pub signal_dbm: Option<i8>,
    pub kind: Kind,
}

impl core::fmt::Debug for Nearby {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Nearby")
            .field("ssid", &"..")
            .field("signal_dbm", &self.signal_dbm)
            .field("kind", &self.kind)
            .finish()
    }
}

/// Signal bars, one to four, at the steps phones draw: -55, -67 and -75 dBm.
/// A BSS with no signal reading, which was still heard, has one.
pub fn bars(signal_dbm: Option<i8>) -> u8 {
    match signal_dbm {
        Some(dbm) if dbm >= -55 => 4,
        Some(dbm) if dbm >= -67 => 3,
        Some(dbm) if dbm >= -75 => 2,
        _ => 1,
    }
}

/// A name as the screen draws it: UTF-8, with what is not, and control
/// characters, as `?`.
pub fn display_name(ssid: &[u8]) -> String {
    String::from_utf8_lossy(ssid)
        .chars()
        .map(|character| match character {
            '\u{fffd}' => '?',
            character if character.is_control() => '?',
            character => character,
        })
        .collect()
}

/// One network the picker lists.
#[derive(Clone, PartialEq, Eq)]
pub struct Choice {
    /// The name as the network sends it, which a join uses.
    pub ssid: Vec<u8>,
    /// [`display_name`].
    pub name: String,
    pub bars: u8,
    /// One of the boot's own networks, `--wifi` or saved, which joins with
    /// its key.
    pub saved: bool,
}

impl core::fmt::Debug for Choice {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Choice")
            .field("ssid", &"..")
            .field("bars", &self.bars)
            .field("saved", &self.saved)
            .finish()
    }
}

/// What a scan gives the picker.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Scan {
    /// The networks a password joins, the strongest first.
    pub choices: Vec<Choice>,
    /// The networks heard that none of whose BSSes Stambha can join.
    pub unjoinable: usize,
}

/// The networks of a scan's BSSes, each once: hidden ones (no name, or all
/// zero bytes) left out, a network joinable when any of its BSSes is, with
/// its strongest signal, `saved` marking the boot's own, the strongest
/// first, then by name's bytes, at most [`MAX_CHOICES`].
pub fn scan(nearby: &[Nearby], saved: &[Vec<u8>]) -> Scan {
    // Each network once: its name, its joinable BSSes' strongest signal,
    // and whether any of its BSSes is joinable.
    let mut networks: Vec<(&[u8], Option<i8>, bool)> = Vec::new();
    for bss in nearby {
        if bss.ssid.is_empty() || bss.ssid.iter().all(|&byte| byte == 0) {
            continue;
        }
        let joinable = bss.kind == Kind::Joinable;
        let signal = bss.signal_dbm.filter(|_| joinable);
        match networks
            .iter_mut()
            .find(|(ssid, ..)| *ssid == &bss.ssid[..])
        {
            // `None` is below every reading.
            Some((_, strongest, known)) => {
                *strongest = (*strongest).max(signal);
                *known |= joinable;
            }
            None => networks.push((&bss.ssid, signal, joinable)),
        }
    }
    let unjoinable = networks.iter().filter(|(.., joinable)| !joinable).count();
    networks.retain(|(.., joinable)| *joinable);
    networks.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    let choices = networks
        .into_iter()
        .take(MAX_CHOICES)
        .map(|(ssid, signal, _)| Choice {
            ssid: ssid.to_vec(),
            name: display_name(ssid),
            bars: bars(signal),
            saved: saved.iter().any(|name| name[..] == *ssid),
        })
        .collect();
    Scan {
        choices,
        unjoinable,
    }
}

/// What the engine knows when the adapter's first scan pass ended.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Facts {
    /// A Wi-Fi adapter started.
    pub adapter: bool,
    /// The host screen can be drawn on.
    pub screen: bool,
    /// A host keyboard is there.
    pub keyboard: bool,
    /// A whole scan pass ended within the wait.
    pub scanned: bool,
    /// A `--wifi` or saved network had a usable BSS in it.
    pub in_range: bool,
    /// Option was held while the devices started.
    pub held: bool,
}

/// Why the picker shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shown {
    /// None of the boot's own networks is in range.
    NoneInRange,
    /// Option was held.
    Asked,
}

/// Why the picker does not show; the boot goes on as it would without it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Skipped {
    NoAdapter,
    NoScreen,
    NoKeyboard,
    /// No scan pass ended in time: nothing to choose from.
    NoScan,
    /// A saved network is in range, which the adapter is joining.
    InRange,
}

/// Whether the picker shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gate {
    Show(Shown),
    Skip(Skipped),
}

/// The gate: the picker shows only with an adapter, a screen, a keyboard
/// and a finished scan, when no network of the boot's own is in range or
/// Option was held.
pub fn decide(facts: Facts) -> Gate {
    let skip = match facts {
        Facts { adapter: false, .. } => Skipped::NoAdapter,
        Facts { screen: false, .. } => Skipped::NoScreen,
        Facts {
            keyboard: false, ..
        } => Skipped::NoKeyboard,
        Facts { held: true, .. } => return Gate::Show(Shown::Asked),
        Facts { scanned: false, .. } => Skipped::NoScan,
        Facts { in_range: true, .. } => Skipped::InRange,
        _ => return Gate::Show(Shown::NoneInRange),
    };
    Gate::Skip(skip)
}

impl Gate {
    /// The log's line, with the scan's counts and no name.
    pub fn line(&self, scan: &Scan) -> String {
        let counts = format!(
            "{} networks nearby, {} not joinable",
            scan.choices.len(),
            scan.unjoinable
        );
        match self {
            Self::Show(Shown::NoneInRange) => {
                format!("WIFI PICKER shown: no saved network in range ({counts})\n")
            }
            Self::Show(Shown::Asked) => {
                format!("WIFI PICKER shown: Option was held ({counts})\n")
            }
            Self::Skip(why) => format!(
                "WIFI PICKER skipped: {}\n",
                match why {
                    Skipped::NoAdapter => "no Wi-Fi adapter started",
                    Skipped::NoScreen => "no screen",
                    Skipped::NoKeyboard => "no keyboard",
                    Skipped::NoScan => "no scan finished in time",
                    Skipped::InRange => "a saved network is in range",
                }
            ),
        }
    }
}

/// A network to join this boot, its password wiped when this is dropped.
#[derive(PartialEq, Eq)]
pub struct Request {
    pub ssid: Vec<u8>,
    /// A key `Secret::parse` takes.
    pub password: Vec<u8>,
}

impl core::fmt::Debug for Request {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Request")
            .field("ssid", &"..")
            .field("password", &"..")
            .finish()
    }
}

impl Drop for Request {
    fn drop(&mut self) {
        wifi_saved::wipe(&mut self.password);
    }
}

/// How a join the picker asked for ended, as the adapter's keeper tells it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Joined,
    /// The 4-way handshake failed its MIC.
    WrongPassword,
    /// A scan no longer finds the network.
    Gone,
    /// No connection within the join's budget.
    TimedOut,
    Failed,
}

/// Why the picker closed without a network.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ended {
    Escape,
    Timeout,
}

/// What the engine does after a key, a tick or an outcome.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    /// Draw again.
    Stay,
    /// Go on without Wi-Fi.
    Skip(Ended),
    /// Join this network, then [`Picker::joined`].
    Join(Request),
    /// Join this network of the boot's own with its saved key, then
    /// [`Picker::joined`].
    JoinSaved(Vec<u8>),
    /// Scan again, then [`Picker::update`].
    Rescan,
    /// Stop the join under way and go back to the boot's own networks.
    Cancel,
    /// Joined: go on with the boot.
    Done,
}

/// The log's line for `action`, with no name or password; `None` for what
/// is not logged.
pub fn line(action: &Action) -> Option<&'static str> {
    Some(match action {
        Action::Join(_) => {
            "WIFI PICKER join requested (a network from the scan; its name and password are not logged)\n"
        }
        Action::JoinSaved(_) => "WIFI PICKER join requested of a saved network\n",
        Action::Skip(Ended::Escape) => "WIFI PICKER closed: going on without Wi-Fi\n",
        Action::Skip(Ended::Timeout) => {
            "WIFI PICKER closed: no key for a minute, going on without Wi-Fi\n"
        }
        Action::Cancel => "WIFI PICKER join stopped\n",
        Action::Done => "WIFI PICKER joined\n",
        Action::Stay | Action::Rescan => return None,
    })
}

/// The log's line for a join's `outcome`.
pub fn outcome_line(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::Joined => "WIFI PICKER joined\n",
        Outcome::WrongPassword => "WIFI PICKER join failed: wrong password\n",
        Outcome::Gone => "WIFI PICKER join failed: the network is out of range\n",
        Outcome::TimedOut => "WIFI PICKER join failed: no connection in time\n",
        Outcome::Failed => "WIFI PICKER join failed\n",
    }
}

pub const WRONG_PASSWORD: &str = "The password may be wrong. Type it again.";
pub const TIMED_OUT: &str = "The network did not answer in time. Try again.";
pub const FAILED: &str = "Joining failed. Try again.";
pub const BACK_HINT: &str = "Esc goes back to the list of networks.";

/// The password dialog of one network.
pub struct Password {
    ssid: Vec<u8>,
    name: String,
    /// Never longer than [`wifi_page::SECRET_BYTES`], so it never moves,
    /// and wiped byte by byte as it shrinks and when dropped.
    password: Vec<u8>,
    shown: bool,
    error: Option<&'static str>,
}

impl Password {
    fn new(ssid: Vec<u8>, name: String, error: Option<&'static str>) -> Self {
        Self {
            ssid,
            name,
            password: Vec::with_capacity(wifi_page::SECRET_BYTES),
            shown: false,
            error,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn shown(&self) -> bool {
        self.shown
    }

    pub fn error(&self) -> Option<&'static str> {
        self.error
    }

    /// Its length; its bytes leave only in a [`Request`].
    pub fn password_len(&self) -> usize {
        self.password.len()
    }

    /// A key: the network to join once Enter finds a usable password.
    fn press(&mut self, key: Key) -> Option<Request> {
        match key {
            Key::Enter => return self.enter(),
            Key::Tab => self.shown = !self.shown,
            Key::Backspace => {
                self.error = None;
                if let Some(last) = self.password.last_mut() {
                    *last = 0;
                }
                self.password.pop();
            }
            Key::Character(character) => {
                self.error = None;
                if !(' '..='~').contains(&character) {
                    self.error = Some(wifi_page::PLAIN_PASSWORD);
                } else if self.password.len() >= wifi_page::SECRET_BYTES {
                    self.error = Some(wifi_page::LONG_PASSWORD);
                } else {
                    self.password.push(character as u8);
                }
            }
            _ => {}
        }
        None
    }

    fn enter(&mut self) -> Option<Request> {
        if let Err(error) = Secret::parse(&self.password) {
            self.error = Some(match error {
                _ if self.password.is_empty() => wifi_page::EMPTY_PASSWORD,
                SecretError::Length => wifi_page::PASSWORD_LENGTH,
                SecretError::NotAscii => wifi_page::PLAIN_PASSWORD,
                SecretError::BadHex => wifi_page::PASSWORD_HEX,
            });
            return None;
        }
        Some(Request {
            ssid: self.ssid.clone(),
            password: core::mem::take(&mut self.password),
        })
    }
}

impl Drop for Password {
    fn drop(&mut self) {
        wifi_saved::wipe(&mut self.password);
    }
}

/// What the picker shows.
pub enum Mode {
    /// The networks.
    List,
    /// A network's password.
    Password(Password),
    /// "Join other network": a hidden network's name and password.
    Other(Form),
    /// A join under way.
    Joining { ssid: Vec<u8>, name: String },
}

/// One selectable row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row {
    /// The choice of that index.
    Network(usize),
    /// "Join other network...".
    JoinOther,
}

/// The picker.
pub struct Picker {
    scan: Scan,
    selected: usize,
    mode: Mode,
    notice: Option<String>,
    failures: u32,
    /// When the last key came, which the idle timeout runs from.
    last_key_ms: u64,
    now_ms: u64,
    /// The USB can save a joined network (`wifi_pending`).
    saving: bool,
}

impl Picker {
    /// The picker over `scan` at `now_ms`, the strongest network selected;
    /// `saving` when the USB can save the network joined.
    pub fn new(scan: Scan, saving: bool, now_ms: u64) -> Self {
        Self {
            scan,
            selected: 0,
            mode: Mode::List,
            notice: None,
            failures: 0,
            last_key_ms: now_ms,
            now_ms,
            saving,
        }
    }

    pub fn mode(&self) -> &Mode {
        &self.mode
    }

    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    pub fn failures(&self) -> u32 {
        self.failures
    }

    pub fn choices(&self) -> &[Choice] {
        &self.scan.choices
    }

    pub fn selected(&self) -> Row {
        self.row(self.selected)
    }

    fn rows(&self) -> usize {
        self.scan.choices.len() + 1
    }

    fn row(&self, index: usize) -> Row {
        match index < self.scan.choices.len() {
            true => Row::Network(index),
            false => Row::JoinOther,
        }
    }

    /// A new scan's networks, the selected one kept selected where it is
    /// still there.
    pub fn update(&mut self, scan: Scan) {
        let kept = match self.selected() {
            Row::Network(index) => Some(self.scan.choices[index].ssid.clone()),
            Row::JoinOther => None,
        };
        self.scan = scan;
        self.selected = match kept {
            Some(ssid) => self
                .scan
                .choices
                .iter()
                .position(|choice| choice.ssid == ssid)
                .unwrap_or(0),
            None => self.rows() - 1,
        };
    }

    /// What `key` at `now_ms` does.
    pub fn press(&mut self, key: Key, now_ms: u64) -> Action {
        self.now_ms = now_ms;
        self.last_key_ms = now_ms;
        self.notice = None;
        match &mut self.mode {
            Mode::List => self.list(key),
            Mode::Password(_) | Mode::Other(_) if key == Key::Escape => {
                self.mode = Mode::List;
                Action::Stay
            }
            Mode::Password(password) => match password.press(key) {
                Some(request) => self.join(request),
                None => Action::Stay,
            },
            Mode::Other(form) => match form.press(key) {
                Some(save) => {
                    let request = request(save);
                    self.join(request)
                }
                None => Action::Stay,
            },
            Mode::Joining { .. } if key == Key::Escape => {
                self.mode = Mode::List;
                self.notice = Some(String::from("Stopped joining."));
                Action::Cancel
            }
            Mode::Joining { .. } => Action::Stay,
        }
    }

    fn join(&mut self, request: Request) -> Action {
        self.mode = Mode::Joining {
            name: display_name(&request.ssid),
            ssid: request.ssid.clone(),
        };
        Action::Join(request)
    }

    fn list(&mut self, key: Key) -> Action {
        let rows = self.rows();
        match key {
            Key::Up => self.selected = (self.selected + rows - 1) % rows,
            Key::Down => self.selected = (self.selected + 1) % rows,
            Key::Home => self.selected = 0,
            Key::End => self.selected = rows - 1,
            Key::Escape => return Action::Skip(Ended::Escape),
            Key::Character('r' | 'R') => {
                self.notice = Some(String::from("Looking for networks again..."));
                return Action::Rescan;
            }
            Key::Enter => match self.selected() {
                Row::JoinOther => self.mode = Mode::Other(Form::new()),
                Row::Network(index) => {
                    let choice = &self.scan.choices[index];
                    if choice.saved {
                        self.mode = Mode::Joining {
                            ssid: choice.ssid.clone(),
                            name: choice.name.clone(),
                        };
                        return Action::JoinSaved(choice.ssid.clone());
                    }
                    let (ssid, name) = (choice.ssid.clone(), choice.name.clone());
                    self.mode = Mode::Password(Password::new(ssid, name, None));
                }
            },
            _ => {}
        }
        Action::Stay
    }

    /// The idle timeout at `now_ms`: a minute with no key goes on without
    /// Wi-Fi, except while a join runs, which has its own budget.
    pub fn tick(&mut self, now_ms: u64) -> Action {
        self.now_ms = now_ms;
        if matches!(self.mode, Mode::Joining { .. }) {
            return Action::Stay;
        }
        match now_ms.saturating_sub(self.last_key_ms) >= IDLE_TIMEOUT_MS {
            true => Action::Skip(Ended::Timeout),
            false => Action::Stay,
        }
    }

    /// The join [`Action::Join`] or [`Action::JoinSaved`] asked for ended
    /// with `outcome` at `now_ms`: [`Action::Done`] once joined; otherwise
    /// its password is asked again, with why, or, for a network gone out of
    /// range, the list says so.
    pub fn joined(&mut self, outcome: Outcome, now_ms: u64) -> Action {
        let Mode::Joining { ssid, name } = &mut self.mode else {
            return Action::Stay;
        };
        let (ssid, name) = (core::mem::take(ssid), core::mem::take(name));
        // The timeout runs again from the outcome.
        self.last_key_ms = now_ms;
        self.now_ms = now_ms;
        if outcome == Outcome::Joined {
            self.mode = Mode::List;
            return Action::Done;
        }
        self.failures += 1;
        let error = match outcome {
            Outcome::Gone => {
                self.mode = Mode::List;
                self.notice = Some(format!("{name} is no longer in range."));
                return Action::Stay;
            }
            Outcome::WrongPassword => WRONG_PASSWORD,
            Outcome::TimedOut => TIMED_OUT,
            Outcome::Joined | Outcome::Failed => FAILED,
        };
        self.mode = Mode::Password(Password::new(ssid, name, Some(error)));
        Action::Stay
    }

    /// The rows that fit, from the one [`crate::screen::window`] keeps the
    /// selected row in.
    pub fn visible(&self, geometry: Geometry) -> core::ops::Range<usize> {
        let layout = Layout::new(geometry);
        let fits = layout.fits(self.scan.choices.is_empty());
        let first = crate::screen::window(self.selected, self.rows(), fits);
        first..(first + fits).min(self.rows())
    }

    /// The picker drawn on a screen of `geometry`.
    pub fn scene(&self, geometry: Geometry) -> Scene {
        let mut scene = Scene::new(geometry.glyph);
        let layout = Layout::new(geometry);
        scene.text(layout.left + 16, layout.top, "Choose a network", Tone::Text);
        let keys = match &self.mode {
            Mode::List => {
                self.draw_list(&mut scene, &layout);
                let footer = match &self.notice {
                    Some(notice) => Some((notice.clone(), Tone::Orange)),
                    None => self.countdown().map(|seconds| {
                        (format!("Going on without Wi-Fi in {seconds} s"), Tone::Dim)
                    }),
                };
                if let Some((text, tone)) = footer {
                    scene.centred(layout.middle, layout.notice, &text, tone);
                }
                match self.selected() {
                    Row::JoinOther => {
                        "Up/Down: choose   Enter: join other network   Esc: go on without Wi-Fi"
                    }
                    Row::Network(_) => {
                        "Up/Down: choose   Enter: join   R: look again   Esc: go on without Wi-Fi"
                    }
                }
            }
            Mode::Password(password) => {
                self.draw_password(&mut scene, &layout, password);
                "Enter: join   Tab: show or hide   Esc: back"
            }
            Mode::Other(form) => {
                wifi_page::draw_form(&mut scene, &layout, form, self.about());
                match form.field() {
                    Field::Name => "Enter: next   Esc: back",
                    Field::Password => "Enter: join   Tab: show or hide   Up: name   Esc: back",
                }
            }
            Mode::Joining { name, .. } => {
                let (left, y) = (layout.left, layout.caption);
                scene.panel(left, y, layout.column, 72, 12, Tone::Card);
                scene.panel(left + 6, y + 18, 3, 36, 1, Tone::Orange);
                scene.text(left + 20, y + 16, &format!("Joining {name}..."), Tone::Text);
                scene.text(
                    left + 20,
                    y + 40,
                    "This can take up to half a minute.",
                    Tone::Dim,
                );
                "Esc: stop"
            }
        };
        scene.centred(layout.middle, layout.keys, keys, Tone::Dim);
        scene
    }

    /// Seconds left before the picker goes on without Wi-Fi, in the last
    /// [`COUNTDOWN_MS`].
    fn countdown(&self) -> Option<u64> {
        let idle = self.now_ms.saturating_sub(self.last_key_ms);
        let left = IDLE_TIMEOUT_MS.saturating_sub(idle);
        (left <= COUNTDOWN_MS).then(|| left.div_ceil(1000))
    }

    /// What a dialog says becomes of the network joined.
    fn about(&self) -> &'static str {
        match self.saving {
            true => "Joined now, and saved on this USB for the next boot.",
            false => "Joined for this boot only: this USB cannot save it yet.",
        }
    }

    fn draw_list(&self, scene: &mut Scene, layout: &Layout) {
        let mut y = layout.caption;
        let caption = match self.scan.choices.is_empty() {
            true => "Looking for networks...",
            false => "Networks nearby",
        };
        scene.text(layout.left + 16, y, caption, Tone::Dim);
        let visible = self.visible(layout.geometry);
        if visible.len() < self.rows() {
            let position = format!("{} of {}", self.selected + 1, self.rows());
            scene.right(layout.right - 16, y, &position, Tone::Dim);
        }
        y = layout.rows;
        if self.scan.choices.is_empty() {
            scene.text(
                layout.left + 20,
                y + 8,
                "No network Stambha can join was found yet.",
                Tone::Dim,
            );
            y += ROW;
        }
        for index in visible {
            let selected = index == self.selected;
            let behind = match selected {
                true => Tone::Raised,
                false => Tone::Background,
            };
            if selected {
                scene.panel(layout.left, y, layout.column, ROW - 4, 8, Tone::Raised);
                scene.panel(layout.left + 6, y + 9, 3, ROW - 22, 1, Tone::Orange);
            }
            match self.row(index) {
                Row::Network(n) => {
                    let choice = &self.scan.choices[n];
                    scene.lock(layout.left + 20, y + 8, Tone::Dim, behind);
                    scene.text(layout.left + 40, y + 8, &choice.name, Tone::Text);
                    let bars_left = layout.right - 16 - BARS_WIDTH;
                    draw_bars(scene, bars_left, y + 8, choice.bars, behind);
                    if choice.saved {
                        scene.right(bars_left - 12, y + 8, "saved", Tone::Dim);
                    }
                }
                Row::JoinOther => {
                    scene.plus(layout.left + 20, y + 8, Tone::Blue);
                    scene.text(layout.left + 40, y + 8, "Join other network...", Tone::Blue);
                }
            }
            y += ROW;
        }
        if self.scan.unjoinable > 0 {
            let count = self.scan.unjoinable;
            let text = match count {
                1 => String::from("1 more network Stambha cannot join (open or enterprise)"),
                _ => format!("{count} more networks Stambha cannot join (open or enterprise)"),
            };
            scene.text(layout.left + 20, y + 8, &text, Tone::Dim);
        }
    }

    fn draw_password(&self, scene: &mut Scene, layout: &Layout, password: &Password) {
        let (left, y) = (layout.left, layout.caption);
        scene.panel(left, y, layout.column, CARD - 68, 12, Tone::Card);
        scene.text(
            left + 20,
            y + 20,
            &format!("Join {}", password.name),
            Tone::Text,
        );
        scene.text(left + 20, y + 44, self.about(), Tone::Dim);
        scene.text(left + 20, y + 76, "Password", Tone::Dim);
        let toggle = match password.shown {
            true => "Tab: hide",
            false => "Tab: show",
        };
        scene.right(layout.right - 20, y + 76, toggle, Tone::Dim);
        // Only ASCII is typed into the password.
        let typed = core::str::from_utf8(&password.password).unwrap_or("");
        let content = match password.shown {
            true => Content::Plain(typed),
            false => Content::Masked(password.password.len()),
        };
        let field = Rect {
            x: left + 16,
            y: y + 96,
            width: layout.column.saturating_sub(32),
            height: ROW,
        };
        scene.field(field, content, true);
        let (message, tone) = match password.error {
            Some(error) => (error, Tone::Red),
            None => ("8 to 63 characters, or 64 hex digits.", Tone::Dim),
        };
        scene.text(left + 20, y + 144, message, tone);
        if self.failures >= FAILURES_BEFORE_HINT {
            scene.text(left + 20, y + 196, BACK_HINT, Tone::Dim);
        }
    }
}

/// Four bars 3 units wide, 2 apart.
const BARS_WIDTH: usize = 18;

/// `lit` of four signal bars from (`x`, `y`) in a 16 unit line, rising to
/// the right; the unlit ones a shade off `behind`.
fn draw_bars(scene: &mut Scene, x: usize, y: usize, lit: u8, behind: Tone) {
    let unlit = match behind {
        Tone::Raised => Tone::Card,
        _ => Tone::Raised,
    };
    for bar in 0..4u8 {
        let height = 4 + 3 * usize::from(bar);
        let tone = match bar < lit {
            true => Tone::Text,
            false => unlit,
        };
        scene.panel(
            x + 5 * usize::from(bar),
            y + 15 - height,
            3,
            height,
            1,
            tone,
        );
    }
}

/// The hidden network the dialog took, to join; its password moves over.
fn request(mut save: Save) -> Request {
    Request {
        ssid: core::mem::take(&mut save.name).into_bytes(),
        password: core::mem::take(&mut save.password),
    }
}

#[cfg(test)]
mod tests;
