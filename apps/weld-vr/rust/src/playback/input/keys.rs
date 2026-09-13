//! Godot physical positions to Linux evdev codes, never Unicode/text input.
use godot::{global::Key, obj::EngineEnum};
use weld_client::LinuxKeycode;

pub(in crate::playback) fn physical_key(code: i64, location: i64) -> Option<LinuxKeycode> {
    let key = Key::try_from_ord(i32::try_from(code).ok()?)?;
    let right = location == 2;
    Some(LinuxKeycode(match key {
        Key::ESCAPE => 1,
        Key::KEY_1 => 2,
        Key::KEY_2 => 3,
        Key::KEY_3 => 4,
        Key::KEY_4 => 5,
        Key::KEY_5 => 6,
        Key::KEY_6 => 7,
        Key::KEY_7 => 8,
        Key::KEY_8 => 9,
        Key::KEY_9 => 10,
        Key::KEY_0 => 11,
        Key::MINUS => 12,
        Key::EQUAL => 13,
        Key::BACKSPACE => 14,
        Key::TAB | Key::BACKTAB => 15,
        Key::Q => 16,
        Key::W => 17,
        Key::E => 18,
        Key::R => 19,
        Key::T => 20,
        Key::Y => 21,
        Key::U => 22,
        Key::I => 23,
        Key::O => 24,
        Key::P => 25,
        Key::BRACKETLEFT => 26,
        Key::BRACKETRIGHT => 27,
        Key::ENTER => 28,
        Key::CTRL => {
            if right {
                97
            } else {
                29
            }
        }
        Key::A => 30,
        Key::S => 31,
        Key::D => 32,
        Key::F => 33,
        Key::G => 34,
        Key::H => 35,
        Key::J => 36,
        Key::K => 37,
        Key::L => 38,
        Key::SEMICOLON => 39,
        Key::APOSTROPHE => 40,
        Key::QUOTELEFT => 41,
        Key::SHIFT => {
            if right {
                54
            } else {
                42
            }
        }
        Key::BACKSLASH => 43,
        Key::Z => 44,
        Key::X => 45,
        Key::C => 46,
        Key::V => 47,
        Key::B => 48,
        Key::N => 49,
        Key::M => 50,
        Key::COMMA => 51,
        Key::PERIOD => 52,
        Key::SLASH => 53,
        Key::KP_MULTIPLY => 55,
        Key::ALT => {
            if right {
                100
            } else {
                56
            }
        }
        Key::SPACE => 57,
        Key::CAPSLOCK => 58,
        Key::F1 => 59,
        Key::F2 => 60,
        Key::F3 => 61,
        Key::F4 => 62,
        Key::F5 => 63,
        Key::F6 => 64,
        Key::F7 => 65,
        Key::F8 => 66,
        Key::F9 => 67,
        Key::F10 => 68,
        Key::NUMLOCK => 69,
        Key::SCROLLLOCK => 70,
        Key::KP_7 => 71,
        Key::KP_8 => 72,
        Key::KP_9 => 73,
        Key::KP_SUBTRACT => 74,
        Key::KP_4 => 75,
        Key::KP_5 => 76,
        Key::KP_6 => 77,
        Key::KP_ADD => 78,
        Key::KP_1 => 79,
        Key::KP_2 => 80,
        Key::KP_3 => 81,
        Key::KP_0 => 82,
        Key::KP_PERIOD => 83,
        Key::LESS => 86,
        Key::F11 => 87,
        Key::F12 => 88,
        Key::KP_ENTER => 96,
        Key::KP_DIVIDE => 98,
        Key::PRINT | Key::SYSREQ => 99,
        Key::HOME => 102,
        Key::UP => 103,
        Key::PAGEUP => 104,
        Key::LEFT => 105,
        Key::RIGHT => 106,
        Key::END => 107,
        Key::DOWN => 108,
        Key::PAGEDOWN => 109,
        Key::INSERT => 110,
        Key::DELETE => 111,
        Key::PAUSE => 119,
        Key::META => {
            if right {
                126
            } else {
                125
            }
        }
        Key::MENU => 127,
        Key::F13 => 183,
        Key::F14 => 184,
        Key::F15 => 185,
        Key::F16 => 186,
        Key::F17 => 187,
        Key::F18 => 188,
        Key::F19 => 189,
        Key::F20 => 190,
        Key::F21 => 191,
        Key::F22 => 192,
        Key::F23 => 193,
        Key::F24 => 194,
        _ => return None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn physical_positions_navigation_keypad_and_modifier_sides() {
        for (key, location, code) in [
            (Key::A, 0, 30),
            (Key::ENTER, 0, 28),
            (Key::TAB, 0, 15),
            (Key::UP, 0, 103),
            (Key::KP_ENTER, 0, 96),
            (Key::KP_1, 0, 79),
            (Key::F12, 0, 88),
            (Key::F24, 0, 194),
            (Key::SHIFT, 1, 42),
            (Key::SHIFT, 2, 54),
            (Key::CTRL, 1, 29),
            (Key::CTRL, 2, 97),
            (Key::ALT, 2, 100),
            (Key::META, 2, 126),
        ] {
            assert_eq!(
                physical_key(i64::from(key.ord()), location),
                Some(LinuxKeycode(code))
            );
        }
        assert_eq!(physical_key(-1, 0), None);
        assert_eq!(physical_key(0, 0), None);
        assert_eq!(physical_key(i64::MAX, 0), None);
    }
}
