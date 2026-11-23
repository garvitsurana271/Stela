//! Keys from a host keyboard after the firmware exits, for the Wi-Fi
//! picker (`wifi_picker`): the HID Keyboard page usages the input drivers
//! give (`stela_drivers::input::Input::Key`) as the startup menu's
//! [`Key`]s, on a US layout. Shift and Caps Lock change the characters;
//! with Ctrl, Alt or GUI held a character key is [`Key::Other`], so a
//! shortcut never types into a password. Releases give nothing, and no key
//! repeats: a HID report has no repeat, and the picker needs none.
use crate::wifi_page::Key;

/// Left Control (E0h) through Right GUI (E7h).
const LEFT_CONTROL: u8 = 0xe0;
const LEFT_SHIFT: u8 = 0xe1;
const LEFT_ALT: u8 = 0xe2;
const RIGHT_SHIFT: u8 = 0xe5;
const RIGHT_ALT: u8 = 0xe6;
const RIGHT_GUI: u8 = 0xe7;
const CAPS_LOCK: u8 = 0x39;

/// The characters of usages 1Eh..38h on a US layout, unshifted and shifted;
/// a zero is a usage that types nothing (Enter, Escape, Backspace, Tab and
/// the non-US `#` key, 32h).
const ROW: [(u8, u8); 27] = [
    (b'1', b'!'),
    (b'2', b'@'),
    (b'3', b'#'),
    (b'4', b'$'),
    (b'5', b'%'),
    (b'6', b'^'),
    (b'7', b'&'),
    (b'8', b'*'),
    (b'9', b'('),
    (b'0', b')'),
    (0, 0),
    (0, 0),
    (0, 0),
    (0, 0),
    (b' ', b' '),
    (b'-', b'_'),
    (b'=', b'+'),
    (b'[', b'{'),
    (b']', b'}'),
    (b'\\', b'|'),
    (0, 0),
    (b';', b':'),
    (b'\'', b'"'),
    (b'`', b'~'),
    (b',', b'<'),
    (b'.', b'>'),
    (b'/', b'?'),
];

/// One keyboard's modifiers, as its presses and releases leave them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Keyboard {
    /// The modifiers held, bit `n` for usage `E0h + n`.
    modifiers: u8,
    caps: bool,
}

impl Keyboard {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether Option (Alt) is held: the key that asks for the picker even
    /// when a saved network is in range.
    pub fn held_option(&self) -> bool {
        self.held(LEFT_ALT) || self.held(RIGHT_ALT)
    }

    fn held(&self, usage: u8) -> bool {
        self.modifiers & (1 << (usage - LEFT_CONTROL)) != 0
    }

    fn shifted(&self) -> bool {
        self.held(LEFT_SHIFT) || self.held(RIGHT_SHIFT)
    }

    /// Ctrl, Alt or GUI, on either side.
    fn chorded(&self) -> bool {
        (LEFT_CONTROL..=RIGHT_GUI)
            .filter(|&usage| usage != LEFT_SHIFT && usage != RIGHT_SHIFT)
            .any(|usage| self.held(usage))
    }

    /// The key `usage` gives as it is `pressed` or released: `None` for a
    /// release, a modifier, or a usage the page does not read.
    pub fn input(&mut self, usage: u8, pressed: bool) -> Option<Key> {
        if (LEFT_CONTROL..=RIGHT_GUI).contains(&usage) {
            let bit = 1 << (usage - LEFT_CONTROL);
            match pressed {
                true => self.modifiers |= bit,
                false => self.modifiers &= !bit,
            }
            return None;
        }
        if !pressed {
            return None;
        }
        if usage == CAPS_LOCK {
            self.caps = !self.caps;
            return None;
        }
        let key = match usage {
            0x28 | 0x58 => Key::Enter,
            0x29 => Key::Escape,
            0x2a => Key::Backspace,
            0x2b => Key::Tab,
            0x4a => Key::Home,
            0x4c => Key::Delete,
            0x4d => Key::End,
            0x51 => Key::Down,
            0x52 => Key::Up,
            // Left and Right move nothing on the page.
            0x4f | 0x50 => Key::Other,
            _ => match self.character(usage) {
                Some(_) if self.chorded() => Key::Other,
                Some(character) => Key::Character(character),
                None => return None,
            },
        };
        Some(key)
    }

    /// The character `usage` types with the modifiers held.
    fn character(&self, usage: u8) -> Option<char> {
        let shift = self.shifted();
        let byte = match usage {
            // a..z: Caps Lock turns Shift around, for letters only.
            0x04..=0x1d => {
                let letter = b'a' + (usage - 0x04);
                match shift != self.caps {
                    true => letter.to_ascii_uppercase(),
                    false => letter,
                }
            }
            0x1e..=0x38 => {
                let (plain, shifted) = ROW[usize::from(usage - 0x1e)];
                match shift {
                    true => shifted,
                    false => plain,
                }
            }
            _ => 0,
        };
        (byte != 0).then_some(char::from(byte))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{string::String, vec::Vec};

    fn tap(keyboard: &mut Keyboard, usage: u8) -> Option<Key> {
        let key = keyboard.input(usage, true);
        assert_eq!(keyboard.input(usage, false), None);
        key
    }

    fn typed(keyboard: &mut Keyboard, usages: &[u8]) -> String {
        usages
            .iter()
            .filter_map(|&usage| match tap(keyboard, usage) {
                Some(Key::Character(character)) => Some(character),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn letters_digits_and_punctuation_type_as_a_us_keyboard_types_them() {
        let mut keyboard = Keyboard::new();
        // h e l l o space 1 2 - = [ ] \ ; ' ` , . /
        let usages = [
            0x0b, 0x08, 0x0f, 0x0f, 0x12, 0x2c, 0x1e, 0x1f, 0x2d, 0x2e, 0x2f, 0x30, 0x31, 0x33,
            0x34, 0x35, 0x36, 0x37, 0x38,
        ];
        assert_eq!(typed(&mut keyboard, &usages), "hello 12-=[]\\;'`,./");
        assert_eq!(typed(&mut keyboard, &[0x04, 0x1d, 0x27]), "az0");
    }

    #[test]
    fn shift_on_either_side_gives_the_upper_characters() {
        for shift in [LEFT_SHIFT, RIGHT_SHIFT] {
            let mut keyboard = Keyboard::new();
            keyboard.input(shift, true);
            let usages: Vec<u8> = (0x1e..=0x27).chain([0x04, 0x2d, 0x38, 0x34]).collect();
            assert_eq!(typed(&mut keyboard, &usages), "!@#$%^&*()A_?\"");
            keyboard.input(shift, false);
            assert_eq!(typed(&mut keyboard, &[0x04]), "a");
        }
    }

    #[test]
    fn caps_lock_turns_letters_only_and_shift_turns_it_back() {
        let mut keyboard = Keyboard::new();
        assert_eq!(tap(&mut keyboard, CAPS_LOCK), None);
        assert_eq!(typed(&mut keyboard, &[0x04, 0x1e]), "A1");
        keyboard.input(LEFT_SHIFT, true);
        assert_eq!(typed(&mut keyboard, &[0x04, 0x1e]), "a!");
        keyboard.input(LEFT_SHIFT, false);
        tap(&mut keyboard, CAPS_LOCK);
        assert_eq!(typed(&mut keyboard, &[0x04]), "a");
    }

    #[test]
    fn the_page_keys_come_from_their_usages() {
        let mut keyboard = Keyboard::new();
        let keys: Vec<Option<Key>> = [0x28, 0x58, 0x29, 0x2a, 0x2b, 0x4a, 0x4c, 0x4d, 0x51, 0x52]
            .iter()
            .map(|&usage| tap(&mut keyboard, usage))
            .collect();
        assert_eq!(
            keys,
            [
                Some(Key::Enter),
                Some(Key::Enter),
                Some(Key::Escape),
                Some(Key::Backspace),
                Some(Key::Tab),
                Some(Key::Home),
                Some(Key::Delete),
                Some(Key::End),
                Some(Key::Down),
                Some(Key::Up),
            ]
        );
        assert_eq!(tap(&mut keyboard, 0x50), Some(Key::Other));
        // F1, Print Screen, the non-US `#`, and a usage past the page's.
        for usage in [0x3a, 0x46, 0x32, 0x90] {
            assert_eq!(tap(&mut keyboard, usage), None);
        }
    }

    #[test]
    fn a_chord_never_types_a_character_and_releases_give_nothing() {
        for modifier in [LEFT_CONTROL, LEFT_ALT, RIGHT_ALT, 0xe3, RIGHT_GUI] {
            let mut keyboard = Keyboard::new();
            assert_eq!(keyboard.input(modifier, true), None);
            assert_eq!(tap(&mut keyboard, 0x04), Some(Key::Other));
            // Enter still works with a modifier held.
            assert_eq!(tap(&mut keyboard, 0x28), Some(Key::Enter));
            assert_eq!(keyboard.input(modifier, false), None);
            assert_eq!(tap(&mut keyboard, 0x04), Some(Key::Character('a')));
        }
    }

    #[test]
    fn option_held_is_told_apart_from_other_modifiers() {
        let mut keyboard = Keyboard::new();
        assert!(!keyboard.held_option());
        keyboard.input(LEFT_CONTROL, true);
        assert!(!keyboard.held_option());
        keyboard.input(RIGHT_ALT, true);
        assert!(keyboard.held_option());
        keyboard.input(RIGHT_ALT, false);
        assert!(!keyboard.held_option());
        keyboard.input(LEFT_ALT, true);
        assert!(keyboard.held_option());
    }
}
