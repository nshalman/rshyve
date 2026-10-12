// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copied from Propolis, Copyright Oxide Computer Company,
// used under MPL-2.0. Upstream: lib/propolis/src/hw/ps2/keyboard/mod.rs
// https://github.com/oxidecomputer/propolis

mod scan_code_1;
mod scan_code_2;

use std::convert::TryFrom;

use super::keysym::{self, AsciiChar, KeySym::*};
use anyhow::{anyhow, Result};

use scan_code_1::*;
use scan_code_2::*;

use super::ctrl::PS2ScanCodeSet;

#[derive(Debug, Clone, Copy)]
pub struct KeyEvent {
    pub keysym_raw: u32,
    pub is_pressed: bool,
}

impl KeyEvent {
    // `KeySym`'s `TryFrom<u32>` deliberately uses `()` for an unrecognized
    // X11 keysym; callers only distinguish recognized from unrecognized.
    #[allow(clippy::result_unit_err)]
    pub fn keysym(&self) -> std::result::Result<keysym::KeySym, ()> {
        keysym::KeySym::try_from(self.keysym_raw)
    }
}

/// A struct that contains all information necessary to construct a scan code
/// for multiple scan code sets, as scan codes from all sets have a similar
/// structure.
///
/// Keyboards send a "make" code when a key is pressed, and a "break" code when
/// a key is released.
///
/// A key's make code is generally 1 byte, which here is referred to as the
/// "base value" of the scan code. Some keys are represented by the "extended"
/// set of codes, which have a prefix code indicating to the controller that the
/// next byte is also relevant. A handful of keys, for backwards compatibility
/// reasons, are represented by multiple scan codes.
///
/// So possible make codes are:
/// - 1 byte: [base value]
/// - 2 bytes: [extended prefix] + [base value]
/// - multibyte sequences composed of 2 or more make codes
///
/// A key's break code varies by scan code set. For Set 1, the highest bit of
/// the make code base value is set. For Set 2, an additional release code is
/// sent ahead of the make code, but after the extended prefix if one is used.
///
/// So possible break codes for Scan Code Set 1 are:
/// - 1 byte: [base value | release flag]
/// - 2 bytes: [extended prefix] + [base value | release flag]
/// - multibyte sequences composed of 2 or more break codes
///
/// And possible break codes for Scan Code Set 2 are:
/// - 2 bytes: [release code] + [base value]
/// - 3 bytes: [extended prefix] + [release code] + [base value]
/// - multibyte sequences composed of 2 or more break codes
///
/// Exceptions:
/// - In Set 2, the pause key does not have a break code.
///
#[derive(Debug)]
pub struct ScanCodeBase {
    base_val: u8,
    prefix: Option<Vec<u8>>,
}

// A representation of a key event that allows us to map keysyms, which are a
// much richer diversity of keys, to representation in scan codes, the set of
// which is more limited.
#[derive(Debug)]
pub struct KeyEventRep {
    pub keysym_raw: u32,
    pub is_pressed: bool,
    pub scan_code_1: ScanCodeBase,
    pub scan_code_2: ScanCodeBase,
}

impl KeyEventRep {
    /// Convert the given key to its scan code within a given scan code set.
    pub(crate) fn to_scan_code(&self, sc_set: PS2ScanCodeSet) -> Vec<u8> {
        let mut bytes = Vec::new();
        let sc = match sc_set {
            PS2ScanCodeSet::Set1 => &self.scan_code_1,
            PS2ScanCodeSet::Set2 => &self.scan_code_2,
        };

        if let Some(prefix) = &sc.prefix {
            bytes.extend(prefix);
        }

        if self.is_pressed {
            bytes.push(sc.base_val);
        } else {
            match sc_set {
                PS2ScanCodeSet::Set1 => {
                    bytes.push(sc.base_val | SC1_RELEASE_FLAG)
                }
                PS2ScanCodeSet::Set2 => {
                    bytes.push(SC2_RELEASE_CODE);
                    bytes.push(sc.base_val);
                }
            }
        }

        bytes
    }
}

/// Scan Code Set 1 base values and their Set 2 twins, one row per
/// distinct Set 1 value. The 8042's translation is byte by byte, so an
/// extended key's base value maps the same way as the keypad key that
/// shares it; the E0 prefix passes through.
const SET1_TO_SET2: &[(u8, u8)] = &[
    (SC1_ESCAPE, SC2_ESCAPE),
    (SC1_1_EXCLAMATION, SC2_1_EXCLAMATION),
    (SC1_2_AT, SC2_2_AT),
    (SC1_3_HASH, SC2_3_HASH),
    (SC1_4_DOLLAR, SC2_4_DOLLAR),
    (SC1_5_PERCENT, SC2_5_PERCENT),
    (SC1_6_CARET, SC2_6_CARET),
    (SC1_7_AMPERSAND, SC2_7_AMPERSAND),
    (SC1_8_ASTERISK, SC2_8_ASTERISK),
    (SC1_9_LPAREN, SC2_9_LPAREN),
    (SC1_0_RPAREN, SC2_0_RPAREN),
    (SC1_DASH_UNDERSCORE, SC2_DASH_UNDERSCORE),
    (SC1_EQUALS_PLUS, SC2_EQUALS_PLUS),
    (SC1_BACKSPACE, SC2_BACKSPACE),
    (SC1_TAB, SC2_TAB),
    (SC1_Q, SC2_Q),
    (SC1_W, SC2_W),
    (SC1_E, SC2_E),
    (SC1_R, SC2_R),
    (SC1_T, SC2_T),
    (SC1_Y, SC2_Y),
    (SC1_U, SC2_U),
    (SC1_I, SC2_I),
    (SC1_O, SC2_O),
    (SC1_P, SC2_P),
    (SC1_LBRACKET_LCURLY, SC2_LBRACKET_LCURLY),
    (SC1_RBRACKET_RCURLY, SC2_RBRACKET_RCURLY),
    (SC1_ENTER, SC2_ENTER),
    (SC1_CTRL_LEFT, SC2_CTRL_LEFT),
    (SC1_A, SC2_A),
    (SC1_S, SC2_S),
    (SC1_D, SC2_D),
    (SC1_F, SC2_F),
    (SC1_G, SC2_G),
    (SC1_H, SC2_H),
    (SC1_J, SC2_J),
    (SC1_K, SC2_K),
    (SC1_L, SC2_L),
    (SC1_SEMICOLON_COLON, SC2_SEMICOLON_COLON),
    (SC1_QUOTE_DBLQUOTE, SC2_QUOTE_DBLQUOTE),
    (SC1_BACKTICK_TILDE, SC2_BACKTICK_TILDE),
    (SC1_SHIFT_LEFT, SC2_SHIFT_LEFT),
    (SC1_BACKSLASH_PIPE, SC2_BACKSLASH_PIPE),
    (SC1_Z, SC2_Z),
    (SC1_X, SC2_X),
    (SC1_C, SC2_C),
    (SC1_V, SC2_V),
    (SC1_B, SC2_B),
    (SC1_N, SC2_N),
    (SC1_M, SC2_M),
    (SC1_COMMA_LESSTHAN, SC2_COMMA_LESSTHAN),
    (SC1_PERIOD_GREATERTHAN, SC2_PERIOD_GREATERTHAN),
    (SC1_KP_SLASH, SC2_KP_SLASH),
    (SC1_SHIFT_RIGHT, SC2_SHIFT_RIGHT),
    (SC1_PRINTSCREEN, SC2_PRINTSCREEN),
    (SC1_ALT_RIGHT, SC2_ALT_RIGHT),
    (SC1_SPACE, SC2_SPACE),
    (SC1_CAPS_LOCK, SC2_CAPS_LOCK),
    (SC1_F1, SC2_F1),
    (SC1_F2, SC2_F2),
    (SC1_F3, SC2_F3),
    (SC1_F4, SC2_F4),
    (SC1_F5, SC2_F5),
    (SC1_F6, SC2_F6),
    (SC1_F7, SC2_F7),
    (SC1_F8, SC2_F8),
    (SC1_F9, SC2_F9),
    (SC1_F10, SC2_F10),
    (SC1_NUM_LOCK, SC2_NUM_LOCK),
    (SC1_SCROLL_LOCK, SC2_SCROLL_LOCK),
    (SC1_HOME, SC2_HOME),
    (SC1_8_UP, SC2_8_UP),
    (SC1_PGUP, SC2_PGUP),
    (SC1_KP_MINUS, SC2_KP_MINUS),
    (SC1_LEFT, SC2_LEFT),
    (SC1_5_CENTER, SC2_5_CENTER),
    (SC1_RIGHT, SC2_RIGHT),
    (SC1_KP_PLUS, SC2_KP_PLUS),
    (SC1_1_END, SC2_1_END),
    (SC1_DOWN, SC2_DOWN),
    (SC1_3_PGDN, SC2_3_PGDN),
    (SC1_INSERT, SC2_INSERT),
    (SC1_DELETE, SC2_DELETE),
    (SC1_F11, SC2_F11),
    (SC1_F12, SC2_F12),
    (SC1_SUPER_LEFT, SC2_SUPER_LEFT),
    (SC1_SUPER_RIGHT, SC2_SUPER_RIGHT),
    // No keysym here names these, but an RDP client sends them.
    (SC1_ISO_BACKSLASH, SC2_ISO_BACKSLASH),
    (SC1_MENU, SC2_MENU),
];

/// The Set 2 base value for a Set 1 base value, if the tables know it.
pub(crate) fn set2_base(set1: u8) -> Option<u8> {
    SET1_TO_SET2
        .iter()
        .find(|(one, _)| *one == set1)
        .map(|(_, two)| *two)
}

/// The bytes a Scan Code Set 1 key event is in `set`, or `None` for a
/// Set 1 value the tables do not know.
pub(crate) fn scancode_bytes(
    code: u8,
    extended: bool,
    is_pressed: bool,
    set: PS2ScanCodeSet,
) -> Option<Vec<u8>> {
    let base = match set {
        PS2ScanCodeSet::Set1 => code,
        PS2ScanCodeSet::Set2 => set2_base(code)?,
    };
    let rep = KeyEventRep {
        keysym_raw: 0,
        is_pressed,
        scan_code_1: ScanCodeBase {
            base_val: code,
            prefix: extended.then(|| vec![SC1_EXTENDED_PREFIX_0]),
        },
        scan_code_2: ScanCodeBase {
            base_val: base,
            prefix: extended.then(|| vec![SC2_EXTENDED_PREFIX_0]),
        },
    };
    Some(rep.to_scan_code(set))
}

impl TryFrom<KeyEvent> for KeyEventRep {
    type Error = anyhow::Error;

    fn try_from(keyevent: KeyEvent) -> Result<Self, Self::Error> {
        let keysym = keyevent.keysym().map_err(|()| {
            anyhow!("unrecognized keysym value: 0x{:x}", keyevent.keysym_raw)
        })?;
        let (base_val_1, base_val_2) = match keysym {
            Ascii(ascii_char) => match ascii_char {
                AsciiChar::BackSpace => (SC1_BACKSPACE, SC2_BACKSPACE),
                AsciiChar::Tab => (SC1_TAB, SC2_TAB),
                AsciiChar::ESC => (SC1_ESCAPE, SC2_ESCAPE),
                AsciiChar::Space => (SC1_SPACE, SC2_SPACE),
                AsciiChar::Apostrophe | AsciiChar::Quotation => {
                    (SC1_QUOTE_DBLQUOTE, SC2_QUOTE_DBLQUOTE)
                }
                AsciiChar::_1 | AsciiChar::Exclamation => {
                    (SC1_1_EXCLAMATION, SC2_1_EXCLAMATION)
                }
                AsciiChar::_2 | AsciiChar::At => (SC1_2_AT, SC2_2_AT),
                AsciiChar::_3 | AsciiChar::Hash => (SC1_3_HASH, SC2_3_HASH),
                AsciiChar::_4 | AsciiChar::Dollar => {
                    (SC1_4_DOLLAR, SC2_4_DOLLAR)
                }
                AsciiChar::_5 | AsciiChar::Percent => {
                    (SC1_5_PERCENT, SC2_5_PERCENT)
                }
                AsciiChar::_6 | AsciiChar::Caret => (SC1_6_CARET, SC2_6_CARET),
                AsciiChar::_7 | AsciiChar::Ampersand => {
                    (SC1_7_AMPERSAND, SC2_7_AMPERSAND)
                }
                AsciiChar::_9 | AsciiChar::ParenOpen => {
                    (SC1_9_LPAREN, SC2_9_LPAREN)
                }
                AsciiChar::_0 | AsciiChar::ParenClose => {
                    (SC1_0_RPAREN, SC2_0_RPAREN)
                }
                AsciiChar::_8 | AsciiChar::Asterisk => {
                    (SC1_8_ASTERISK, SC2_8_ASTERISK)
                }
                AsciiChar::Equal | AsciiChar::Plus => {
                    (SC1_EQUALS_PLUS, SC2_EQUALS_PLUS)
                }
                AsciiChar::Comma | AsciiChar::LessThan => {
                    (SC1_COMMA_LESSTHAN, SC2_COMMA_LESSTHAN)
                }
                AsciiChar::Minus | AsciiChar::UnderScore => {
                    (SC1_DASH_UNDERSCORE, SC2_DASH_UNDERSCORE)
                }
                AsciiChar::Dot | AsciiChar::GreaterThan => {
                    (SC1_PERIOD_GREATERTHAN, SC2_PERIOD_GREATERTHAN)
                }
                AsciiChar::Slash | AsciiChar::Question => {
                    (SC1_SLASH_QUESTIONMARK, SC2_SLASH_QUESTIONMARK)
                }
                AsciiChar::Semicolon | AsciiChar::Colon => {
                    (SC1_SEMICOLON_COLON, SC2_SEMICOLON_COLON)
                }
                AsciiChar::A | AsciiChar::a => (SC1_A, SC2_A),
                AsciiChar::B | AsciiChar::b => (SC1_B, SC2_B),
                AsciiChar::C | AsciiChar::c => (SC1_C, SC2_C),
                AsciiChar::D | AsciiChar::d => (SC1_D, SC2_D),
                AsciiChar::E | AsciiChar::e => (SC1_E, SC2_E),
                AsciiChar::F | AsciiChar::f => (SC1_F, SC2_F),
                AsciiChar::G | AsciiChar::g => (SC1_G, SC2_G),
                AsciiChar::H | AsciiChar::h => (SC1_H, SC2_H),
                AsciiChar::I | AsciiChar::i => (SC1_I, SC2_I),
                AsciiChar::J | AsciiChar::j => (SC1_J, SC2_J),
                AsciiChar::K | AsciiChar::k => (SC1_K, SC2_K),
                AsciiChar::L | AsciiChar::l => (SC1_L, SC2_L),
                AsciiChar::M | AsciiChar::m => (SC1_M, SC2_M),
                AsciiChar::N | AsciiChar::n => (SC1_N, SC2_N),
                AsciiChar::O | AsciiChar::o => (SC1_O, SC2_O),
                AsciiChar::P | AsciiChar::p => (SC1_P, SC2_P),
                AsciiChar::Q | AsciiChar::q => (SC1_Q, SC2_Q),
                AsciiChar::R | AsciiChar::r => (SC1_R, SC2_R),
                AsciiChar::S | AsciiChar::s => (SC1_S, SC2_S),
                AsciiChar::T | AsciiChar::t => (SC1_T, SC2_T),
                AsciiChar::U | AsciiChar::u => (SC1_U, SC2_U),
                AsciiChar::V | AsciiChar::v => (SC1_V, SC2_V),
                AsciiChar::W | AsciiChar::w => (SC1_W, SC2_W),
                AsciiChar::X | AsciiChar::x => (SC1_X, SC2_X),
                AsciiChar::Y | AsciiChar::y => (SC1_Y, SC2_Y),
                AsciiChar::Z | AsciiChar::z => (SC1_Z, SC2_Z),
                AsciiChar::BracketOpen | AsciiChar::CurlyBraceOpen => {
                    (SC1_LBRACKET_LCURLY, SC2_LBRACKET_LCURLY)
                }
                AsciiChar::BackSlash | AsciiChar::VerticalBar => {
                    (SC1_BACKSLASH_PIPE, SC2_BACKSLASH_PIPE)
                }
                AsciiChar::BracketClose | AsciiChar::CurlyBraceClose => {
                    (SC1_RBRACKET_RCURLY, SC2_RBRACKET_RCURLY)
                }
                AsciiChar::Grave | AsciiChar::Tilde => {
                    (SC1_BACKTICK_TILDE, SC2_BACKTICK_TILDE)
                }
                AsciiChar::DEL => (SC1_DELETE, SC2_DELETE),

                // values that are unrepresentable, or we are explicitly ignoring
                AsciiChar::Null
                | AsciiChar::SOH
                | AsciiChar::SOX
                | AsciiChar::ETX
                | AsciiChar::EOT
                | AsciiChar::ENQ
                | AsciiChar::ACK
                | AsciiChar::Bell
                | AsciiChar::LineFeed
                | AsciiChar::VT
                | AsciiChar::FF
                | AsciiChar::CarriageReturn
                | AsciiChar::SI
                | AsciiChar::SO
                | AsciiChar::DLE
                | AsciiChar::DC1
                | AsciiChar::DC2
                | AsciiChar::DC3
                | AsciiChar::DC4
                | AsciiChar::NAK
                | AsciiChar::SYN
                | AsciiChar::ETB
                | AsciiChar::CAN
                | AsciiChar::EM
                | AsciiChar::SUB
                | AsciiChar::FS
                | AsciiChar::GS
                | AsciiChar::RS
                | AsciiChar::US => (0x0, 0x0),
            },
            Backspace => (SC1_BACKSPACE, SC2_BACKSPACE),
            Tab => (SC1_TAB, SC2_TAB),
            ReturnOrEnter => (SC1_ENTER, SC2_ENTER),
            Escape => (SC1_ESCAPE, SC2_ESCAPE),
            Insert => (SC1_INSERT, SC2_INSERT),
            Delete => (SC1_DELETE, SC2_DELETE),
            Home => (SC1_HOME, SC2_HOME),
            End => (SC1_END, SC2_END),
            PageUp => (SC1_PGUP, SC2_PGUP),
            PageDown => (SC1_PGDN, SC2_PGDN),
            Print => (SC1_PRINTSCREEN, SC2_PRINTSCREEN),
            CapsLock => (SC1_CAPS_LOCK, SC2_CAPS_LOCK),
            SuperLeft => (SC1_SUPER_LEFT, SC2_SUPER_LEFT),
            SuperRight => (SC1_SUPER_RIGHT, SC2_SUPER_RIGHT),

            Left => (SC1_LEFT, SC2_LEFT),
            Up => (SC1_UP, SC2_UP),
            Right => (SC1_RIGHT, SC2_RIGHT),
            Down => (SC1_DOWN, SC2_DOWN),

            FunctionKey(1) => (SC1_F1, SC2_F1),
            FunctionKey(2) => (SC1_F2, SC2_F2),
            FunctionKey(3) => (SC1_F3, SC2_F3),
            FunctionKey(4) => (SC1_F4, SC2_F4),
            FunctionKey(5) => (SC1_F5, SC2_F5),
            FunctionKey(6) => (SC1_F6, SC2_F6),
            FunctionKey(7) => (SC1_F7, SC2_F7),
            FunctionKey(8) => (SC1_F8, SC2_F8),
            FunctionKey(9) => (SC1_F9, SC2_F9),
            FunctionKey(10) => (SC1_F10, SC2_F10),
            FunctionKey(11) => (SC1_F11, SC2_F11),
            FunctionKey(12) => (SC1_F12, SC2_F12),

            ShiftLeft => (SC1_SHIFT_LEFT, SC2_SHIFT_LEFT),
            ShiftRight => (SC1_SHIFT_RIGHT, SC2_SHIFT_RIGHT),
            ControlLeft => (SC1_CTRL_LEFT, SC2_CTRL_LEFT),
            ControlRight => (SC1_CTRL_RIGHT, SC2_CTRL_RIGHT),
            AltLeft => (SC1_ALT_LEFT, SC2_ALT_LEFT),
            AltRight => (SC1_ALT_RIGHT, SC2_ALT_RIGHT),
            ScrollLock => (SC1_SCROLL_LOCK, SC2_SCROLL_LOCK),
            NumLock => (SC1_NUM_LOCK, SC2_NUM_LOCK),

            KeypadSlash => (SC1_KP_SLASH, SC2_KP_SLASH),
            KeypadAsterisk => (SC1_KP_ASTERISK, SC2_KP_ASTERISK),
            KeypadMinus => (SC1_KP_MINUS, SC2_KP_MINUS),
            KeypadPlus => (SC1_KP_PLUS, SC2_KP_PLUS),
            KeypadEnter => (SC1_KP_ENTER, SC2_KP_ENTER),
            KeypadPeriod | KeypadDelete => {
                (SC1_KP_DEL_PERIOD, SC2_KP_DEL_PERIOD)
            }
            Keypad0 | KeypadInsert => (SC1_0_INSERT, SC2_0_INSERT),
            Keypad1 | KeypadEnd => (SC1_1_END, SC2_1_END),
            Keypad2 | KeypadDown => (SC1_2_DOWN, SC2_2_DOWN),
            Keypad3 | KeypadPgDown => (SC1_3_PGDN, SC2_3_PGDN),
            Keypad4 | KeypadLeft => (SC1_4_LEFT, SC2_4_LEFT),
            Keypad5 | KeypadEmpty => (SC1_5_CENTER, SC2_5_CENTER),
            Keypad6 | KeypadRight => (SC1_6_RIGHT, SC2_6_RIGHT),
            Keypad7 | KeypadHome => (SC1_7_HOME, SC2_7_HOME),
            Keypad8 | KeypadUp => (SC1_8_UP, SC2_8_UP),
            Keypad9 | KeypadPgUp => (SC1_9_PGUP, SC2_9_PGUP),

            // Keys we're choosing to drop explicitly for now
            FunctionKey(_) | Pause | Menu => (0x0, 0x0),
        };

        if matches!((base_val_1, base_val_2), (0x0, 0x0)) {
            return Err(anyhow!(
                "unrecognized keysym value: 0x{:x}",
                keyevent.keysym_raw
            ));
        }

        let prefix_1 = match keysym {
            AltRight | ControlRight | Home | Insert | End | PageUp
            | PageDown | KeypadSlash | KeypadEnter | SuperLeft | SuperRight
            | Left | Right | Up | Down => Some(vec![SC1_EXTENDED_PREFIX_0]),
            _ => None,
        };

        let prefix_2 = match keysym {
            AltRight | ControlRight | Home | Insert | End | PageUp
            | PageDown | KeypadSlash | KeypadEnter | SuperLeft | SuperRight
            | Left | Right | Up | Down => Some(vec![SC2_EXTENDED_PREFIX_0]),
            _ => None,
        };

        let sc1 = ScanCodeBase {
            base_val: base_val_1,
            prefix: prefix_1,
        };
        let sc2 = ScanCodeBase {
            base_val: base_val_2,
            prefix: prefix_2,
        };

        Ok(Self {
            keysym_raw: keyevent.keysym_raw,
            is_pressed: keyevent.is_pressed,
            scan_code_1: sc1,
            scan_code_2: sc2,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Set 2 guest gets the key a Set 1 code names: every key the
    /// keysym table knows gives the same Set 2 value both ways.
    #[test]
    fn every_known_key_has_the_same_set_2_value_both_ways() {
        let mut checked = 0;
        for keysym_raw in 0..=0xFFFF {
            let event = KeyEvent {
                keysym_raw,
                is_pressed: true,
            };
            let Ok(rep) = KeyEventRep::try_from(event) else {
                continue;
            };
            assert_eq!(
                set2_base(rep.scan_code_1.base_val),
                Some(rep.scan_code_2.base_val),
                "keysym {keysym_raw:#x}"
            );
            checked += 1;
        }
        assert!(checked > 80, "only {checked} keysyms known");
    }

    /// Keys an RDP client sends that have no keysym here: the key left
    /// of Z on an ISO keyboard, and Menu.
    #[test]
    fn the_iso_key_and_menu_reach_a_set_2_guest() {
        let set2 = PS2ScanCodeSet::Set2;
        assert_eq!(
            scancode_bytes(SC1_ISO_BACKSLASH, false, true, set2),
            Some(vec![0x61])
        );
        assert_eq!(
            scancode_bytes(SC1_MENU, true, true, set2),
            Some(vec![0xE0, 0x2F])
        );
        assert_eq!(
            scancode_bytes(SC1_MENU, true, false, set2),
            Some(vec![0xE0, 0xF0, 0x2F]),
            "and its release"
        );
    }

    /// A code the tables do not know reaches no Set 2 guest, rather
    /// than as some other key.
    #[test]
    fn an_unknown_code_is_dropped_for_a_set_2_guest() {
        assert_eq!(
            scancode_bytes(0x7F, false, true, PS2ScanCodeSet::Set2),
            None
        );
        assert_eq!(
            scancode_bytes(0x7F, false, true, PS2ScanCodeSet::Set1),
            Some(vec![0x7F]),
            "Set 1 passes it as it came"
        );
    }
}
