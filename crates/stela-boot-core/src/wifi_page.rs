//! The startup menu's Wi-Fi page (`stela-boot`'s `wifi_menu`), without the
//! firmware: what the page holds, how each key changes it, and the shapes
//! and text it draws, in the startup menu's look (`choice::Menu::draw`). The
//! boot side reads and writes `wifi.saved` and paints the [`Scene`]; the
//! rest is here, where the host tests reach it.
//!
//! The page lists the networks this boot may join, stela.cfg's `--wifi` one
//! first, then each saved one, each with a lock, as every network Stela
//! joins has a WPA key; then "Join other network...", which opens a small
//! dialog for a name and a password. Enter on a network has this boot join
//! it first (`--wifi-prefer`); Delete or Backspace on a saved one asks, then
//! forgets it. A password is drawn as dots unless Tab shows it, and never
//! reaches a [`Draw`] otherwise. The rows are [`Row`]s, so a section of
//! networks in range can follow the saved ones once a scan can run here.
use crate::wifi_saved::{self, Listed};
use alloc::{format, string::String, vec::Vec};
use stela_drivers::net::wifi::network::{SSID_BYTES, Secret, SecretError};

/// The most a typed password holds: a PSK's 64 hex digits.
pub const SECRET_BYTES: usize = 64;
/// A row's height, the startup menu's.
pub(crate) const ROW: usize = 36;
/// The widest the page's column grows, the startup menu's.
const COLUMN: usize = 440;
/// The dialog's height.
pub(crate) const CARD: usize = 244;

/// The startup menu's Wi-Fi line: the network this boot joins first, the
/// one the page chose (`prefer`) or else stela.cfg's `--wifi` one
/// (`configured`), and the key that changes it. With no `--wifi` the boot
/// joins nothing, and saved networks (`saved`) can still be managed. `None`
/// when this boot has neither. No password reaches it.
pub fn status_line(configured: Option<&str>, prefer: Option<&str>, saved: bool) -> Option<String> {
    let network = match (configured, prefer) {
        (Some(_), Some(prefer)) => prefer,
        (Some(configured), None) => configured,
        (None, _) if saved => "not set up",
        (None, _) => return None,
    };
    Some(format!("Wi-Fi: {network} \u{b7} press W to change"))
}

/// One network the page lists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Network {
    pub name: String,
    /// stela.cfg's `--wifi` network, whose key is `wifi.key`: it is not in
    /// `wifi.saved`, so it is never forgotten here.
    pub configured: bool,
    /// A saved key a boot can use: a line whose key is not one is skipped
    /// when the boot joins (`wifi_saved::parse`).
    pub usable: bool,
}

/// The menu's `--wifi-prefer` once the page closed, as `networks` now lists
/// them: kept only while it names a saved network with a key a boot can use,
/// so a network forgotten on the page, or one whose key is not usable, is
/// not still named as joined first.
pub fn kept_preference(prefer: Option<String>, networks: &[Network]) -> Option<String> {
    prefer.filter(|name| {
        networks
            .iter()
            .any(|network| !network.configured && network.usable && network.name == *name)
    })
}

/// The networks to list: `configured` first, then each of the saved file's
/// (`wifi_saved::listing`) once. A name that is not UTF-8 cannot be chosen
/// by name (`--wifi-prefer`), so it is left out.
pub fn networks(configured: Option<&str>, saved: Vec<Listed>) -> Vec<Network> {
    let mut networks: Vec<Network> = configured
        .map(|name| Network {
            name: String::from(name),
            configured: true,
            usable: true,
        })
        .into_iter()
        .collect();
    for listed in saved {
        let Ok(name) = String::from_utf8(listed.name) else {
            continue;
        };
        if Some(name.as_str()) != configured {
            networks.push(Network {
                name,
                configured: false,
                usable: listed.usable,
            });
        }
    }
    networks
}

/// A key, as the page reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    Home,
    End,
    Enter,
    Escape,
    Delete,
    Backspace,
    Tab,
    Character(char),
    Other,
}

/// What the boot side does after a key.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    /// Draw the page again.
    Stay,
    /// Back to the startup menu, nothing chosen.
    Leave,
    /// Join this network first this boot.
    Join(String),
    /// Forget this saved network, then [`Page::forgotten`].
    Forget(String),
    /// Save this network, then [`Page::saved`].
    Save(Save),
}

/// A network to save, its password wiped when this is dropped.
#[derive(PartialEq, Eq)]
pub struct Save {
    pub name: String,
    /// A key `Secret::parse` takes.
    pub password: Vec<u8>,
}

impl core::fmt::Debug for Save {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Save")
            .field("name", &self.name)
            .field("password", &"..")
            .finish()
    }
}

impl Drop for Save {
    fn drop(&mut self) {
        wifi_saved::wipe(&mut self.password);
    }
}

/// One of the dialog's two fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    Name,
    Password,
}

pub const EMPTY_NAME: &str = "Type the network's name.";
pub const LONG_NAME: &str = "A network name is at most 32 bytes.";
pub const EMPTY_PASSWORD: &str = "Type the network's password.";
pub const LONG_PASSWORD: &str = "A password is at most 63 characters, or 64 hex digits.";
pub const PLAIN_PASSWORD: &str = "A password uses only plain keyboard characters.";
pub const PASSWORD_LENGTH: &str = "A password is 8 to 63 characters, or 64 hex digits.";
pub const PASSWORD_HEX: &str = "A 64 character password is all hex digits, 0-9 and a-f.";
/// Why Enter on a network chooses nothing when no `--wifi` is set.
pub const NO_WIFI: &str = "This boot joins no Wi-Fi: no startup entry sets --wifi.";

/// The "Join other network" dialog: a name, then a password.
pub struct Form {
    field: Field,
    name: String,
    /// Never longer than [`SECRET_BYTES`], so it is never moved, and wiped
    /// byte by byte as it shrinks and when the dialog closes.
    password: Vec<u8>,
    /// The password is drawn as typed, not as dots: Tab.
    shown: bool,
    error: Option<&'static str>,
}

impl Form {
    pub(crate) fn new() -> Self {
        Self {
            field: Field::Name,
            name: String::new(),
            password: Vec::with_capacity(SECRET_BYTES),
            shown: false,
            error: None,
        }
    }

    pub fn field(&self) -> Field {
        self.field
    }

    pub fn shown(&self) -> bool {
        self.shown
    }

    pub fn error(&self) -> Option<&'static str> {
        self.error
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// The password's length; its bytes never leave the form but in a [`Save`].
    pub fn password_len(&self) -> usize {
        self.password.len()
    }

    fn type_character(&mut self, character: char) {
        self.error = None;
        match self.field {
            Field::Name if character.is_control() => {}
            Field::Name if self.name.len() + character.len_utf8() > SSID_BYTES => {
                self.error = Some(LONG_NAME);
            }
            Field::Name => self.name.push(character),
            // `Secret::parse` takes printable ASCII only.
            Field::Password if !(' '..='~').contains(&character) => {
                self.error = Some(PLAIN_PASSWORD);
            }
            Field::Password if self.password.len() >= SECRET_BYTES => {
                self.error = Some(LONG_PASSWORD);
            }
            Field::Password => self.password.push(character as u8),
        }
    }

    fn backspace(&mut self) {
        self.error = None;
        match self.field {
            Field::Name => {
                self.name.pop();
            }
            Field::Password => {
                if let Some(last) = self.password.last_mut() {
                    *last = 0;
                }
                self.password.pop();
            }
        }
    }

    /// Enter: the next field, or the network to save once both are usable.
    fn enter(&mut self) -> Option<Save> {
        self.error = None;
        if self.name.is_empty() {
            self.field = Field::Name;
            self.error = Some(EMPTY_NAME);
            return None;
        }
        if self.field == Field::Name {
            self.field = Field::Password;
            return None;
        }
        if let Err(error) = Secret::parse(&self.password) {
            self.error = Some(match error {
                _ if self.password.is_empty() => EMPTY_PASSWORD,
                SecretError::Length => PASSWORD_LENGTH,
                SecretError::NotAscii => PLAIN_PASSWORD,
                SecretError::BadHex => PASSWORD_HEX,
            });
            return None;
        }
        Some(Save {
            name: self.name.clone(),
            password: core::mem::take(&mut self.password),
        })
    }

    pub(crate) fn press(&mut self, key: Key) -> Option<Save> {
        match key {
            Key::Enter => return self.enter(),
            Key::Down if self.field == Field::Name => return self.enter(),
            Key::Up => self.field = Field::Name,
            Key::Tab if self.field == Field::Password => self.shown = !self.shown,
            Key::Backspace => self.backspace(),
            Key::Character(character) => self.type_character(character),
            _ => {}
        }
        None
    }
}

impl Drop for Form {
    fn drop(&mut self) {
        wifi_saved::wipe(&mut self.password);
    }
}

/// What the page shows.
pub enum Mode {
    /// The networks.
    List,
    /// "Forget this network?", for the network of that index.
    Confirm(usize),
    /// The "Join other network" dialog.
    Add(Form),
}

/// One selectable row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row {
    /// The network of that index.
    Network(usize),
    /// "Join other network...".
    JoinOther,
}

/// The page.
pub struct Page {
    networks: Vec<Network>,
    /// A startup entry sets `--wifi`, so this boot joins a network and a
    /// network can be chosen to join first.
    joins: bool,
    /// The network this boot joins first, when the page chose one before.
    prefer: Option<String>,
    selected: usize,
    mode: Mode,
    notice: Option<String>,
}

impl Page {
    /// The page over `networks` ([`networks`]), the one `prefer` names
    /// selected, else the first.
    pub fn new(networks: Vec<Network>, joins: bool, prefer: Option<&str>) -> Self {
        let selected = prefer
            .and_then(|prefer| networks.iter().position(|network| network.name == prefer))
            .unwrap_or(0);
        Self {
            networks,
            joins,
            prefer: prefer.map(String::from),
            selected,
            mode: Mode::List,
            notice: None,
        }
    }

    pub fn mode(&self) -> &Mode {
        &self.mode
    }

    pub fn selected(&self) -> Row {
        self.row(self.selected)
    }

    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    /// The selectable rows: the networks, then "Join other network...".
    fn rows(&self) -> usize {
        self.networks.len() + 1
    }

    fn row(&self, index: usize) -> Row {
        match index < self.networks.len() {
            true => Row::Network(index),
            false => Row::JoinOther,
        }
    }

    /// What `key` does.
    pub fn press(&mut self, key: Key) -> Action {
        self.notice = None;
        match &mut self.mode {
            Mode::List => self.list(key),
            Mode::Confirm(index) => {
                let index = *index;
                match key {
                    Key::Enter => {
                        self.mode = Mode::List;
                        Action::Forget(self.networks[index].name.clone())
                    }
                    Key::Escape => {
                        self.mode = Mode::List;
                        Action::Stay
                    }
                    _ => Action::Stay,
                }
            }
            Mode::Add(_) if key == Key::Escape => {
                self.mode = Mode::List;
                Action::Stay
            }
            Mode::Add(form) => match form.press(key) {
                Some(save) => {
                    self.mode = Mode::List;
                    Action::Save(save)
                }
                None => Action::Stay,
            },
        }
    }

    fn list(&mut self, key: Key) -> Action {
        let rows = self.rows();
        match key {
            Key::Up => self.selected = (self.selected + rows - 1) % rows,
            Key::Down => self.selected = (self.selected + 1) % rows,
            Key::Home => self.selected = 0,
            Key::End => self.selected = rows - 1,
            Key::Escape => return Action::Leave,
            Key::Enter => match self.selected() {
                Row::JoinOther => self.mode = Mode::Add(Form::new()),
                Row::Network(_) if !self.joins => self.notice = Some(String::from(NO_WIFI)),
                Row::Network(index) if !self.networks[index].usable => {
                    self.notice = Some(format!(
                        "{}'s saved key is not one a boot can use: forget it and join it again.",
                        self.networks[index].name
                    ));
                }
                Row::Network(index) => return Action::Join(self.networks[index].name.clone()),
            },
            Key::Delete | Key::Backspace => match self.selected() {
                Row::Network(index) if self.networks[index].configured => {
                    self.notice = Some(format!(
                        "{} is stela.cfg's network; it is not forgotten here.",
                        self.networks[index].name
                    ));
                }
                Row::Network(index) => self.mode = Mode::Confirm(index),
                Row::JoinOther => {}
            },
            _ => {}
        }
        Action::Stay
    }

    /// The networks again after one was forgotten, or why it was not.
    pub fn forgotten(
        &mut self,
        name: &str,
        result: Result<(), &'static str>,
        networks: Vec<Network>,
    ) {
        self.networks = networks;
        self.selected = self.selected.min(self.rows() - 1);
        self.notice = Some(match result {
            Ok(()) => format!("Forgot {name}."),
            Err(why) => String::from(why),
        });
    }

    /// The networks again after one was saved: it joins first this boot
    /// when the boot joins Wi-Fi at all, as the page had it before; else the
    /// page stays, with it selected.
    pub fn saved(
        &mut self,
        name: &str,
        result: Result<(), &'static str>,
        networks: Vec<Network>,
    ) -> Action {
        self.networks = networks;
        self.selected = self.selected.min(self.rows() - 1);
        match result {
            Ok(()) if self.joins => return Action::Join(String::from(name)),
            Ok(()) => {
                if let Some(index) = self.networks.iter().position(|n| n.name == name) {
                    self.selected = index;
                }
                self.notice = Some(format!("Saved {name}."));
            }
            Err(why) => self.notice = Some(String::from(why)),
        }
        Action::Stay
    }

    /// The page drawn on a screen of `geometry`.
    pub fn scene(&self, geometry: Geometry) -> Scene {
        let mut scene = Scene {
            draws: Vec::new(),
            glyph: geometry.glyph.max(1),
        };
        let layout = Layout::new(geometry);
        scene.text(layout.left + 16, layout.top, "Wi-Fi", Tone::Text);
        match &self.mode {
            Mode::List => {
                self.draw_list(&mut scene, &layout);
                if let Some(notice) = &self.notice {
                    scene.centred(layout.middle, layout.notice, notice, Tone::Orange);
                }
                scene.centred(layout.middle, layout.keys, self.keys(), Tone::Dim);
            }
            Mode::Confirm(index) => {
                self.draw_list(&mut scene, &layout);
                let name = &self.networks[*index].name;
                draw_confirm(&mut scene, &layout, name);
                scene.centred(
                    layout.middle,
                    layout.keys,
                    "Enter: forget   Esc: cancel",
                    Tone::Dim,
                );
            }
            Mode::Add(form) => {
                let about = match self.joins {
                    true => "Saved on this USB and joined first this boot.",
                    false => "Saved on this USB, in wifi.saved.",
                };
                draw_form(&mut scene, &layout, form, about);
                let keys = match form.field {
                    Field::Name => "Enter: next   Esc: cancel",
                    Field::Password => "Enter: save   Tab: show or hide   Up: name   Esc: cancel",
                };
                scene.centred(layout.middle, layout.keys, keys, Tone::Dim);
            }
        }
        scene
    }

    /// The keys the selected row takes.
    fn keys(&self) -> &'static str {
        match self.selected() {
            Row::JoinOther => "Up/Down: choose   Enter: join other network   Esc: back",
            Row::Network(_) if !self.joins => "Up/Down: choose   Delete: forget   Esc: back",
            Row::Network(index) if self.networks[index].configured => {
                "Up/Down: choose   Enter: join first   Esc: back"
            }
            Row::Network(_) => "Up/Down: choose   Enter: join first   Delete: forget   Esc: back",
        }
    }

    /// The rows that fit, from the one [`crate::screen::window`] keeps the
    /// selected row in.
    pub fn visible(&self, geometry: Geometry) -> core::ops::Range<usize> {
        let layout = Layout::new(geometry);
        let fits = layout.fits(self.networks.is_empty());
        let first = crate::screen::window(self.selected, self.rows(), fits);
        first..(first + fits).min(self.rows())
    }

    fn draw_list(&self, scene: &mut Scene, layout: &Layout) {
        let mut y = layout.caption;
        scene.text(layout.left + 16, y, "Saved networks", Tone::Dim);
        let geometry = layout.geometry;
        let visible = self.visible(geometry);
        if visible.len() < self.rows() {
            let position = format!("{} of {}", self.selected + 1, self.rows());
            scene.right(layout.right - 16, y, &position, Tone::Dim);
        }
        y = layout.rows;
        if self.networks.is_empty() {
            scene.text(layout.left + 20, y + 8, "No saved networks yet.", Tone::Dim);
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
                    let network = &self.networks[n];
                    let lock = match network.usable {
                        true => Tone::Dim,
                        false => Tone::Red,
                    };
                    scene.lock(layout.left + 20, y + 8, lock, behind);
                    scene.text(layout.left + 40, y + 8, &network.name, Tone::Text);
                    let marker = match self.prefer.as_deref() {
                        Some(prefer) if prefer == network.name => Some(("this boot", Tone::Orange)),
                        _ if network.configured => Some(("default", Tone::Dim)),
                        _ if !network.usable => Some(("key unusable", Tone::Red)),
                        _ => None,
                    };
                    if let Some((marker, tone)) = marker {
                        scene.right(layout.right - 16, y + 8, marker, tone);
                    }
                }
                Row::JoinOther => {
                    scene.plus(layout.left + 20, y + 8, Tone::Blue);
                    scene.text(layout.left + 40, y + 8, "Join other network...", Tone::Blue);
                }
            }
            y += ROW;
        }
    }
}

/// "Forget this network?", in a card over the bottom of the list.
fn draw_confirm(scene: &mut Scene, layout: &Layout, name: &str) {
    let y = layout.confirm;
    scene.panel(layout.left, y, layout.column, 64, 12, Tone::Card);
    scene.panel(layout.left + 6, y + 14, 3, 36, 1, Tone::Red);
    scene.text(
        layout.left + 20,
        y + 12,
        &format!("Forget {name}?"),
        Tone::Text,
    );
    scene.text(
        layout.left + 20,
        y + 36,
        "Enter to confirm, Esc to cancel",
        Tone::Dim,
    );
}

/// The dialog: a card with the name and password fields.
pub(crate) fn draw_form(scene: &mut Scene, layout: &Layout, form: &Form, about: &str) {
    let (left, y) = (layout.left, layout.caption);
    scene.panel(left, y, layout.column, CARD, 12, Tone::Card);
    scene.text(left + 20, y + 20, "Join other network", Tone::Text);
    scene.text(left + 20, y + 44, about, Tone::Dim);
    let field = Rect {
        x: left + 16,
        y: y + 96,
        width: layout.column.saturating_sub(32),
        height: ROW,
    };
    scene.text(left + 20, y + 76, "Network name", Tone::Dim);
    let content = Content::Plain(&form.name);
    scene.field(field, content, form.field == Field::Name);
    scene.text(left + 20, y + 144, "Password", Tone::Dim);
    if form.field == Field::Password {
        let toggle = match form.shown {
            true => "Tab: hide",
            false => "Tab: show",
        };
        scene.right(layout.right - 20, y + 144, toggle, Tone::Dim);
    }
    // Only ASCII is typed into the password.
    let password = core::str::from_utf8(&form.password).unwrap_or("");
    let content = match form.shown {
        true => Content::Plain(password),
        false => Content::Masked(form.password.len()),
    };
    let field = Rect {
        y: y + 164,
        ..field
    };
    scene.field(field, content, form.field == Field::Password);
    let (message, tone) = match (form.error, form.field) {
        (Some(error), _) => (error, Tone::Red),
        (None, Field::Name) => ("1 to 32 bytes, as the network shows it.", Tone::Dim),
        (None, Field::Password) => ("8 to 63 characters, or 64 hex digits.", Tone::Dim),
    };
    scene.text(left + 20, y + 212, message, tone);
}

/// The screen the page is drawn on, in page units (`screen::scale`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    pub width: usize,
    pub height: usize,
    /// The first row under the header.
    pub top: usize,
    /// A glyph's width.
    pub glyph: usize,
}

/// Where the page's parts go: the startup menu's column and footer.
pub(crate) struct Layout {
    pub(crate) geometry: Geometry,
    pub(crate) left: usize,
    pub(crate) right: usize,
    pub(crate) column: usize,
    pub(crate) middle: usize,
    pub(crate) top: usize,
    pub(crate) caption: usize,
    pub(crate) rows: usize,
    pub(crate) confirm: usize,
    pub(crate) notice: usize,
    pub(crate) keys: usize,
}

impl Layout {
    pub(crate) fn new(geometry: Geometry) -> Self {
        let Geometry {
            width, height, top, ..
        } = geometry;
        let column = width.saturating_sub(32).min(COLUMN);
        let left = (width - column) / 2;
        Self {
            geometry,
            left,
            right: left + column,
            column,
            middle: width / 2,
            top,
            caption: top + 32,
            rows: top + 60,
            confirm: height.saturating_sub(112),
            notice: height.saturating_sub(56),
            keys: height.saturating_sub(24),
        }
    }

    /// The rows that fit above the footer, as many as the startup menu's;
    /// one fewer with the line an empty list shows.
    pub(crate) fn fits(&self, empty: bool) -> usize {
        let fits = self.geometry.height.saturating_sub(self.rows + 120) / ROW;
        fits.saturating_sub(usize::from(empty)).max(1)
    }
}

/// A colour of the startup menu's look (`stela-boot`'s `ui`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Background,
    Card,
    Raised,
    Text,
    Dim,
    Orange,
    Blue,
    Red,
}

/// One shape.
#[derive(Debug, PartialEq, Eq)]
pub enum Draw {
    /// A rectangle with corners of `radius` rounded.
    Panel {
        x: usize,
        y: usize,
        width: usize,
        height: usize,
        radius: usize,
        tone: Tone,
    },
    /// Text from (`x`, `y`), one glyph a character.
    Text {
        x: usize,
        y: usize,
        text: String,
        tone: Tone,
    },
}

#[derive(Clone, Copy)]
pub(crate) struct Rect {
    pub(crate) x: usize,
    pub(crate) y: usize,
    pub(crate) width: usize,
    pub(crate) height: usize,
}

/// What a field holds.
pub(crate) enum Content<'a> {
    Plain(&'a str),
    /// This many characters, as dots.
    Masked(usize),
}

/// The page's shapes, over the page's background and header. Its text is
/// wiped when it is dropped: a shown password is among it.
pub struct Scene {
    pub draws: Vec<Draw>,
    glyph: usize,
}

impl Drop for Scene {
    fn drop(&mut self) {
        for draw in &mut self.draws {
            if let Draw::Text { text, .. } = draw {
                wifi_saved::wipe(&mut core::mem::take(text).into_bytes());
            }
        }
    }
}

impl Scene {
    /// An empty scene for glyphs `glyph` units wide.
    pub(crate) fn new(glyph: usize) -> Self {
        Self {
            draws: Vec::new(),
            glyph: glyph.max(1),
        }
    }

    /// The text of every [`Draw::Text`], in order.
    pub fn texts(&self) -> impl Iterator<Item = &str> {
        self.draws.iter().filter_map(|draw| match draw {
            Draw::Text { text, .. } => Some(text.as_str()),
            Draw::Panel { .. } => None,
        })
    }

    pub(crate) fn width(&self, text: &str) -> usize {
        text.chars().count() * self.glyph
    }

    pub(crate) fn panel(
        &mut self,
        x: usize,
        y: usize,
        width: usize,
        height: usize,
        radius: usize,
        tone: Tone,
    ) {
        self.draws.push(Draw::Panel {
            x,
            y,
            width,
            height,
            radius,
            tone,
        });
    }

    pub(crate) fn text(&mut self, x: usize, y: usize, text: &str, tone: Tone) {
        self.draws.push(Draw::Text {
            x,
            y,
            text: String::from(text),
            tone,
        });
    }

    pub(crate) fn right(&mut self, right: usize, y: usize, text: &str, tone: Tone) {
        let x = right.saturating_sub(self.width(text));
        self.text(x, y, text, tone);
    }

    pub(crate) fn centred(&mut self, middle: usize, y: usize, text: &str, tone: Tone) {
        let x = middle.saturating_sub(self.width(text) / 2);
        self.text(x, y, text, tone);
    }

    /// A padlock 12 units wide in a 16 unit line from (`x`, `y`), its
    /// shackle's hole and keyhole in `behind`.
    pub(crate) fn lock(&mut self, x: usize, y: usize, tone: Tone, behind: Tone) {
        self.panel(x + 2, y + 1, 8, 9, 4, tone);
        self.panel(x + 4, y + 3, 4, 7, 2, behind);
        self.panel(x, y + 6, 12, 9, 2, tone);
        self.panel(x + 5, y + 9, 2, 3, 1, behind);
    }

    /// A plus sign 12 units wide in a 16 unit line from (`x`, `y`).
    pub(crate) fn plus(&mut self, x: usize, y: usize, tone: Tone) {
        self.panel(x + 5, y + 2, 2, 12, 1, tone);
        self.panel(x, y + 7, 12, 2, 1, tone);
    }

    /// A text field: raised, ringed in orange with a caret while typed in,
    /// showing the end of what it holds when that is longer than it.
    pub(crate) fn field(&mut self, rect: Rect, content: Content<'_>, active: bool) {
        let Rect {
            x,
            y,
            width,
            height,
        } = rect;
        match active {
            true => {
                self.panel(x, y, width, height, 8, Tone::Orange);
                self.panel(x + 2, y + 2, width - 4, height - 4, 6, Tone::Raised);
            }
            false => self.panel(x, y, width, height, 8, Tone::Raised),
        }
        let capacity = width.saturating_sub(28) / self.glyph;
        let (left, top) = (x + 12, y + 10);
        let shown = match content {
            Content::Plain(text) => {
                let text = tail(text, capacity);
                self.text(left, top, text, Tone::Text);
                text.chars().count()
            }
            Content::Masked(length) => {
                let dots = length.min(capacity);
                for n in 0..dots {
                    let x = left + n * self.glyph + self.glyph / 2 - 3;
                    self.panel(x, top + 5, 6, 6, 3, Tone::Text);
                }
                dots
            }
        };
        if active {
            self.panel(left + shown * self.glyph + 1, y + 9, 2, 18, 1, Tone::Orange);
        }
    }
}

/// The last `capacity` characters of `text`.
pub fn tail(text: &str, capacity: usize) -> &str {
    let skip = text.chars().count().saturating_sub(capacity);
    match text.char_indices().nth(skip) {
        Some((start, _)) => &text[start..],
        None => "",
    }
}

#[cfg(test)]
mod tests;
