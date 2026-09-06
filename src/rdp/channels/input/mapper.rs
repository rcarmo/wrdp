//! Scancode mapping tables for RDP input translation.
//!
//! Provides RDP scancode to Linux evdev keycode mappings, including
//! standard, extended, and layout-specific overrides.

use crate::rdp::channels::input::error::{InputError, Result};
use std::collections::HashMap;

/// Linux evdev keycodes
pub(crate) mod keycodes {
    // Primary keys
    pub(crate) const KEY_ESC: u32 = 1;
    pub(crate) const KEY_1: u32 = 2;
    pub(crate) const KEY_2: u32 = 3;
    pub(crate) const KEY_3: u32 = 4;
    pub(crate) const KEY_4: u32 = 5;
    pub(crate) const KEY_5: u32 = 6;
    pub(crate) const KEY_6: u32 = 7;
    pub(crate) const KEY_7: u32 = 8;
    pub(crate) const KEY_8: u32 = 9;
    pub(crate) const KEY_9: u32 = 10;
    pub(crate) const KEY_0: u32 = 11;
    pub(crate) const KEY_MINUS: u32 = 12;
    pub(crate) const KEY_EQUAL: u32 = 13;
    pub(crate) const KEY_BACKSPACE: u32 = 14;
    pub(crate) const KEY_TAB: u32 = 15;
    pub(crate) const KEY_Q: u32 = 16;
    pub(crate) const KEY_W: u32 = 17;
    pub(crate) const KEY_E: u32 = 18;
    pub(crate) const KEY_R: u32 = 19;
    pub(crate) const KEY_T: u32 = 20;
    pub(crate) const KEY_Y: u32 = 21;
    pub(crate) const KEY_U: u32 = 22;
    pub(crate) const KEY_I: u32 = 23;
    pub(crate) const KEY_O: u32 = 24;
    pub(crate) const KEY_P: u32 = 25;
    pub(crate) const KEY_LEFTBRACE: u32 = 26;
    pub(crate) const KEY_RIGHTBRACE: u32 = 27;
    pub(crate) const KEY_ENTER: u32 = 28;
    pub(crate) const KEY_LEFTCTRL: u32 = 29;
    pub(crate) const KEY_A: u32 = 30;
    pub(crate) const KEY_S: u32 = 31;
    pub(crate) const KEY_D: u32 = 32;
    pub(crate) const KEY_F: u32 = 33;
    pub(crate) const KEY_G: u32 = 34;
    pub(crate) const KEY_H: u32 = 35;
    pub(crate) const KEY_J: u32 = 36;
    pub(crate) const KEY_K: u32 = 37;
    pub(crate) const KEY_L: u32 = 38;
    pub(crate) const KEY_SEMICOLON: u32 = 39;
    pub(crate) const KEY_APOSTROPHE: u32 = 40;
    pub(crate) const KEY_GRAVE: u32 = 41;
    pub(crate) const KEY_LEFTSHIFT: u32 = 42;
    pub(crate) const KEY_BACKSLASH: u32 = 43;
    pub(crate) const KEY_Z: u32 = 44;
    pub(crate) const KEY_X: u32 = 45;
    pub(crate) const KEY_C: u32 = 46;
    pub(crate) const KEY_V: u32 = 47;
    pub(crate) const KEY_B: u32 = 48;
    pub(crate) const KEY_N: u32 = 49;
    pub(crate) const KEY_M: u32 = 50;
    pub(crate) const KEY_COMMA: u32 = 51;
    pub(crate) const KEY_DOT: u32 = 52;
    pub(crate) const KEY_SLASH: u32 = 53;
    pub(crate) const KEY_RIGHTSHIFT: u32 = 54;
    pub(crate) const KEY_KPASTERISK: u32 = 55;
    pub(crate) const KEY_LEFTALT: u32 = 56;
    pub(crate) const KEY_SPACE: u32 = 57;
    pub(crate) const KEY_CAPSLOCK: u32 = 58;

    // Function keys
    pub(crate) const KEY_F1: u32 = 59;
    pub(crate) const KEY_F2: u32 = 60;
    pub(crate) const KEY_F3: u32 = 61;
    pub(crate) const KEY_F4: u32 = 62;
    pub(crate) const KEY_F5: u32 = 63;
    pub(crate) const KEY_F6: u32 = 64;
    pub(crate) const KEY_F7: u32 = 65;
    pub(crate) const KEY_F8: u32 = 66;
    pub(crate) const KEY_F9: u32 = 67;
    pub(crate) const KEY_F10: u32 = 68;
    pub(crate) const KEY_NUMLOCK: u32 = 69;
    pub(crate) const KEY_SCROLLLOCK: u32 = 70;

    // Numpad
    pub(crate) const KEY_KP7: u32 = 71;
    pub(crate) const KEY_KP8: u32 = 72;
    pub(crate) const KEY_KP9: u32 = 73;
    pub(crate) const KEY_KPMINUS: u32 = 74;
    pub(crate) const KEY_KP4: u32 = 75;
    pub(crate) const KEY_KP5: u32 = 76;
    pub(crate) const KEY_KP6: u32 = 77;
    pub(crate) const KEY_KPPLUS: u32 = 78;
    pub(crate) const KEY_KP1: u32 = 79;
    pub(crate) const KEY_KP2: u32 = 80;
    pub(crate) const KEY_KP3: u32 = 81;
    pub(crate) const KEY_KP0: u32 = 82;
    pub(crate) const KEY_KPDOT: u32 = 83;

    pub(crate) const KEY_102ND: u32 = 86;
    pub(crate) const KEY_F11: u32 = 87;
    pub(crate) const KEY_F12: u32 = 88;
    pub(crate) const KEY_RO: u32 = 89;
    pub(crate) const KEY_KATAKANAHIRAGANA: u32 = 90;
    pub(crate) const KEY_HENKAN: u32 = 92;
    pub(crate) const KEY_MUHENKAN: u32 = 94;
    pub(crate) const KEY_KPENTER: u32 = 96;
    pub(crate) const KEY_RIGHTCTRL: u32 = 97;
    pub(crate) const KEY_KPSLASH: u32 = 98;
    pub(crate) const KEY_SYSRQ: u32 = 99;
    pub(crate) const KEY_RIGHTALT: u32 = 100;
    pub(crate) const KEY_HOME: u32 = 102;
    pub(crate) const KEY_UP: u32 = 103;
    pub(crate) const KEY_PAGEUP: u32 = 104;
    pub(crate) const KEY_LEFT: u32 = 105;
    pub(crate) const KEY_RIGHT: u32 = 106;
    pub(crate) const KEY_END: u32 = 107;
    pub(crate) const KEY_DOWN: u32 = 108;
    pub(crate) const KEY_PAGEDOWN: u32 = 109;
    pub(crate) const KEY_INSERT: u32 = 110;
    pub(crate) const KEY_DELETE: u32 = 111;
    pub(crate) const KEY_MUTE: u32 = 113;
    pub(crate) const KEY_VOLUMEDOWN: u32 = 114;
    pub(crate) const KEY_VOLUMEUP: u32 = 115;
    pub(crate) const KEY_POWER: u32 = 116;
    pub(crate) const KEY_KPEQUAL: u32 = 117;
    pub(crate) const KEY_PAUSE: u32 = 119;
    pub(crate) const KEY_HANGEUL: u32 = 122;
    pub(crate) const KEY_HANJA: u32 = 123;
    pub(crate) const KEY_YEN: u32 = 124;
    pub(crate) const KEY_LEFTMETA: u32 = 125;
    pub(crate) const KEY_RIGHTMETA: u32 = 126;
    pub(crate) const KEY_COMPOSE: u32 = 127;
    pub(crate) const KEY_STOP: u32 = 128;
    pub(crate) const KEY_AGAIN: u32 = 129;
    pub(crate) const KEY_PROPS: u32 = 130;
    pub(crate) const KEY_UNDO: u32 = 131;
    pub(crate) const KEY_FRONT: u32 = 132;
    pub(crate) const KEY_COPY: u32 = 133;
    pub(crate) const KEY_MENU: u32 = 139;
    pub(crate) const KEY_CALC: u32 = 140;
    pub(crate) const KEY_SLEEP: u32 = 142;
    pub(crate) const KEY_WAKEUP: u32 = 143;
    pub(crate) const KEY_MAIL: u32 = 155;
    pub(crate) const KEY_BOOKMARKS: u32 = 156;
    pub(crate) const KEY_COMPUTER: u32 = 157;
    pub(crate) const KEY_BACK: u32 = 158;
    pub(crate) const KEY_FORWARD: u32 = 159;
    pub(crate) const KEY_EJECTCD: u32 = 161;
    pub(crate) const KEY_NEXTSONG: u32 = 163;
    pub(crate) const KEY_PLAYPAUSE: u32 = 164;
    pub(crate) const KEY_PREVIOUSSONG: u32 = 165;
    pub(crate) const KEY_STOPCD: u32 = 166;
    pub(crate) const KEY_REFRESH: u32 = 173;
    pub(crate) const KEY_F13: u32 = 183;
    pub(crate) const KEY_F14: u32 = 184;
    pub(crate) const KEY_F15: u32 = 185;
    pub(crate) const KEY_F16: u32 = 186;
    pub(crate) const KEY_F17: u32 = 187;
    pub(crate) const KEY_F18: u32 = 188;
    pub(crate) const KEY_F19: u32 = 189;
    pub(crate) const KEY_F20: u32 = 190;
    pub(crate) const KEY_F21: u32 = 191;
    pub(crate) const KEY_F22: u32 = 192;
    pub(crate) const KEY_F23: u32 = 193;
    pub(crate) const KEY_F24: u32 = 194;
    pub(crate) const KEY_MEDIA: u32 = 226;
    pub(crate) const KEY_SEARCH: u32 = 217;
    pub(crate) const KEY_HOMEPAGE: u32 = 172;
    pub(crate) const KEY_BREAK: u32 = 411;
    pub(crate) const KEY_PRINT: u32 = 210;
}

#[expect(clippy::wildcard_imports)]
use keycodes::*;

/// Scancode mapper for RDP-to-evdev keycode translation.
pub(crate) struct ScancodeMapper {
    /// Primary scancode map (0x00-0x7F)
    primary_map: HashMap<u16, u32>,

    /// Extended scancode map (0xE000-0xE0FF)
    extended_map: HashMap<u16, u32>,

    /// E1 prefix scancode map
    e1_map: HashMap<u32, u32>,

    /// Layout-specific overrides
    layout_overrides: HashMap<String, HashMap<u16, u32>>,

    /// Current keyboard layout
    current_layout: String,
}

impl ScancodeMapper {
    /// Create a new scancode mapper
    pub(crate) fn new() -> Self {
        let mut mapper = Self {
            primary_map: HashMap::new(),
            extended_map: HashMap::new(),
            e1_map: HashMap::new(),
            layout_overrides: HashMap::new(),
            current_layout: "us".to_string(),
        };

        mapper.initialize_mappings();
        mapper
    }

    /// Initialize all scancode mappings
    fn initialize_mappings(&mut self) {
        self.initialize_primary_map();
        self.initialize_extended_map();
        self.initialize_e1_map();
        self.load_layout_overrides();
    }

    /// Initialize primary scancode map (0x00-0x7F)
    fn initialize_primary_map(&mut self) {
        let mappings = vec![
            (0x01, KEY_ESC),
            (0x02, KEY_1),
            (0x03, KEY_2),
            (0x04, KEY_3),
            (0x05, KEY_4),
            (0x06, KEY_5),
            (0x07, KEY_6),
            (0x08, KEY_7),
            (0x09, KEY_8),
            (0x0A, KEY_9),
            (0x0B, KEY_0),
            (0x0C, KEY_MINUS),
            (0x0D, KEY_EQUAL),
            (0x0E, KEY_BACKSPACE),
            (0x0F, KEY_TAB),
            (0x10, KEY_Q),
            (0x11, KEY_W),
            (0x12, KEY_E),
            (0x13, KEY_R),
            (0x14, KEY_T),
            (0x15, KEY_Y),
            (0x16, KEY_U),
            (0x17, KEY_I),
            (0x18, KEY_O),
            (0x19, KEY_P),
            (0x1A, KEY_LEFTBRACE),
            (0x1B, KEY_RIGHTBRACE),
            (0x1C, KEY_ENTER),
            (0x1D, KEY_LEFTCTRL),
            (0x1E, KEY_A),
            (0x1F, KEY_S),
            (0x20, KEY_D),
            (0x21, KEY_F),
            (0x22, KEY_G),
            (0x23, KEY_H),
            (0x24, KEY_J),
            (0x25, KEY_K),
            (0x26, KEY_L),
            (0x27, KEY_SEMICOLON),
            (0x28, KEY_APOSTROPHE),
            (0x29, KEY_GRAVE),
            (0x2A, KEY_LEFTSHIFT),
            (0x2B, KEY_BACKSLASH),
            (0x2C, KEY_Z),
            (0x2D, KEY_X),
            (0x2E, KEY_C),
            (0x2F, KEY_V),
            (0x30, KEY_B),
            (0x31, KEY_N),
            (0x32, KEY_M),
            (0x33, KEY_COMMA),
            (0x34, KEY_DOT),
            (0x35, KEY_SLASH),
            (0x36, KEY_RIGHTSHIFT),
            (0x37, KEY_KPASTERISK),
            (0x38, KEY_LEFTALT),
            (0x39, KEY_SPACE),
            (0x3A, KEY_CAPSLOCK),
            (0x3B, KEY_F1),
            (0x3C, KEY_F2),
            (0x3D, KEY_F3),
            (0x3E, KEY_F4),
            (0x3F, KEY_F5),
            (0x40, KEY_F6),
            (0x41, KEY_F7),
            (0x42, KEY_F8),
            (0x43, KEY_F9),
            (0x44, KEY_F10),
            (0x45, KEY_NUMLOCK),
            (0x46, KEY_SCROLLLOCK),
            (0x47, KEY_KP7),
            (0x48, KEY_KP8),
            (0x49, KEY_KP9),
            (0x4A, KEY_KPMINUS),
            (0x4B, KEY_KP4),
            (0x4C, KEY_KP5),
            (0x4D, KEY_KP6),
            (0x4E, KEY_KPPLUS),
            (0x4F, KEY_KP1),
            (0x50, KEY_KP2),
            (0x51, KEY_KP3),
            (0x52, KEY_KP0),
            (0x53, KEY_KPDOT),
            (0x54, KEY_SYSRQ),
            (0x56, KEY_102ND),
            (0x57, KEY_F11),
            (0x58, KEY_F12),
            (0x59, KEY_KPEQUAL),
            (0x5A, KEY_F13),
            (0x5B, KEY_F14),
            (0x5C, KEY_F15),
            (0x5D, KEY_F16),
            (0x5E, KEY_F17),
            (0x5F, KEY_F18),
            (0x60, KEY_F19),
            (0x61, KEY_F20),
            (0x62, KEY_F21),
            (0x63, KEY_F22),
            (0x64, KEY_F23),
            (0x65, KEY_F24),
            (0x70, KEY_KATAKANAHIRAGANA),
            (0x71, KEY_MUHENKAN),
            (0x72, KEY_HENKAN),
            (0x73, KEY_RO),
            (0x74, KEY_YEN),
            (0x75, KEY_HANGEUL),
            (0x76, KEY_HANJA),
            (0x77, KEY_LEFTMETA),
            (0x78, KEY_RIGHTMETA),
            (0x79, KEY_COMPOSE),
            (0x7A, KEY_STOP),
            (0x7B, KEY_AGAIN),
            (0x7C, KEY_PROPS),
            (0x7D, KEY_UNDO),
            (0x7E, KEY_FRONT),
            (0x7F, KEY_COPY),
        ];

        for (scancode, keycode) in mappings {
            self.primary_map.insert(scancode, keycode);
        }
    }

    /// Initialize extended scancode map (E0 prefix)
    fn initialize_extended_map(&mut self) {
        let mappings = vec![
            (0xE01C, KEY_KPENTER),
            (0xE01D, KEY_RIGHTCTRL),
            (0xE020, KEY_MUTE),
            (0xE021, KEY_CALC),
            (0xE022, KEY_PLAYPAUSE),
            (0xE024, KEY_STOPCD),
            (0xE02E, KEY_VOLUMEDOWN),
            (0xE030, KEY_VOLUMEUP),
            (0xE032, KEY_HOMEPAGE),
            (0xE035, KEY_KPSLASH),
            (0xE037, KEY_PRINT),
            (0xE038, KEY_RIGHTALT),
            (0xE045, KEY_PAUSE),
            (0xE047, KEY_HOME),
            (0xE048, KEY_UP),
            (0xE049, KEY_PAGEUP),
            (0xE04B, KEY_LEFT),
            (0xE04D, KEY_RIGHT),
            (0xE04F, KEY_END),
            (0xE050, KEY_DOWN),
            (0xE051, KEY_PAGEDOWN),
            (0xE052, KEY_INSERT),
            (0xE053, KEY_DELETE),
            (0xE05B, KEY_LEFTMETA),
            (0xE05C, KEY_RIGHTMETA),
            (0xE05D, KEY_MENU),
            (0xE05E, KEY_POWER),
            (0xE05F, KEY_SLEEP),
            (0xE063, KEY_WAKEUP),
            (0xE065, KEY_SEARCH),
            (0xE066, KEY_BOOKMARKS),
            (0xE067, KEY_REFRESH),
            (0xE068, KEY_STOP),
            (0xE069, KEY_FORWARD),
            (0xE06A, KEY_BACK),
            (0xE06B, KEY_COMPUTER),
            (0xE06C, KEY_MAIL),
            (0xE06D, KEY_MEDIA),
            (0xE010, KEY_PREVIOUSSONG),
            (0xE019, KEY_NEXTSONG),
            (0xE02C, KEY_EJECTCD),
        ];

        for (scancode, keycode) in mappings {
            self.extended_map.insert(scancode, keycode);
        }
    }

    /// Initialize E1 prefix scancode map
    fn initialize_e1_map(&mut self) {
        self.e1_map.insert(0xE11D45, KEY_PAUSE);
        self.e1_map.insert(0xE11D46, KEY_BREAK);
    }

    /// Load layout-specific override mappings.
    ///
    /// Remaps physical scancode positions for layouts that differ from the
    /// default mapping used by the virtual keyboard path.
    fn load_layout_overrides(&mut self) {
        // German QWERTZ: Y and Z are swapped
        let mut de = HashMap::new();
        de.insert(0x15, KEY_Z); // Y position → Z
        de.insert(0x2C, KEY_Y); // Z position → Y
        self.layout_overrides.insert("de".to_string(), de);

        // French AZERTY: A↔Q and W↔Z swapped, M moved
        let mut fr = HashMap::new();
        fr.insert(0x10, KEY_A); // Q position → A
        fr.insert(0x1E, KEY_Q); // A position → Q
        fr.insert(0x11, KEY_Z); // W position → Z
        fr.insert(0x2C, KEY_W); // Z position → W
        fr.insert(0x27, KEY_M); // semicolon position → M
        fr.insert(0x32, KEY_COMMA); // M position → comma
        self.layout_overrides.insert("fr".to_string(), fr);

        // UK layout: nearly identical to US, only the backslash/102nd key differs
        let mut uk = HashMap::new();
        uk.insert(0x2B, KEY_102ND); // backslash position → 102nd key (# on UK)
        self.layout_overrides.insert("uk".to_string(), uk.clone());
        self.layout_overrides.insert("gb".to_string(), uk);

        // Spanish layout: same physical arrangement as US QWERTY
        // Differences are handled by XKB (accent keys, ñ via dead keys)
        // No scancode overrides needed, but register it as a known layout
        self.layout_overrides
            .insert("es".to_string(), HashMap::new());

        // Portuguese layout: same physical arrangement as US QWERTY
        self.layout_overrides
            .insert("pt".to_string(), HashMap::new());

        // Italian layout: same physical arrangement as US QWERTY
        self.layout_overrides
            .insert("it".to_string(), HashMap::new());

        // Belgian AZERTY: same as French AZERTY for physical keys
        let mut be = HashMap::new();
        be.insert(0x10, KEY_A);
        be.insert(0x1E, KEY_Q);
        be.insert(0x11, KEY_Z);
        be.insert(0x2C, KEY_W);
        be.insert(0x27, KEY_M);
        be.insert(0x32, KEY_COMMA);
        self.layout_overrides.insert("be".to_string(), be);

        // Swiss German/French: QWERTZ base (same Y↔Z swap as German)
        let mut ch = HashMap::new();
        ch.insert(0x15, KEY_Z);
        ch.insert(0x2C, KEY_Y);
        self.layout_overrides.insert("ch".to_string(), ch);

        // Dvorak: comprehensive letter rearrangement
        let mut dvorak = HashMap::new();
        // Top row: ' , . p y f g c r l
        dvorak.insert(0x10, KEY_APOSTROPHE); // Q → '
        dvorak.insert(0x11, KEY_COMMA); // W → ,
        dvorak.insert(0x12, KEY_DOT); // E → .
        dvorak.insert(0x13, KEY_P); // R → P
        dvorak.insert(0x14, KEY_Y); // T → Y
        dvorak.insert(0x15, KEY_F); // Y → F
        dvorak.insert(0x16, KEY_G); // U → G
        dvorak.insert(0x17, KEY_C); // I → C
        dvorak.insert(0x18, KEY_R); // O → R
        dvorak.insert(0x19, KEY_L); // P → L
        // Home row: a o e u i d h t n s
        dvorak.insert(0x1E, KEY_A); // A → A (same)
        dvorak.insert(0x1F, KEY_O); // S → O
        dvorak.insert(0x20, KEY_E); // D → E
        dvorak.insert(0x21, KEY_U); // F → U
        dvorak.insert(0x22, KEY_I); // G → I
        dvorak.insert(0x23, KEY_D); // H → D
        dvorak.insert(0x24, KEY_H); // J → H
        dvorak.insert(0x25, KEY_T); // K → T
        dvorak.insert(0x26, KEY_N); // L → N
        dvorak.insert(0x27, KEY_S); // ; → S
        // Bottom row: ; q j k x b m w v z
        dvorak.insert(0x2C, KEY_SEMICOLON); // Z → ;
        dvorak.insert(0x2D, KEY_Q); // X → Q
        dvorak.insert(0x2E, KEY_J); // C → J
        dvorak.insert(0x2F, KEY_K); // V → K
        dvorak.insert(0x30, KEY_X); // B → X
        dvorak.insert(0x31, KEY_B); // N → B
        dvorak.insert(0x32, KEY_M); // M → M (same)
        dvorak.insert(0x33, KEY_W); // , → W
        dvorak.insert(0x34, KEY_V); // . → V
        dvorak.insert(0x35, KEY_Z); // / → Z
        self.layout_overrides.insert("dvorak".to_string(), dvorak);

        // Colemak: partial rearrangement from QWERTY
        let mut colemak = HashMap::new();
        // Changes from QWERTY: e→f, r→p, t→g, y→j, u→l, i→u, o→y, p→;
        // s→r, d→s, f→t, g→d, j→n, k→e, l→i, ;→o
        // n→k
        colemak.insert(0x12, KEY_F); // E → F
        colemak.insert(0x13, KEY_P); // R → P
        colemak.insert(0x14, KEY_G); // T → G
        colemak.insert(0x15, KEY_J); // Y → J
        colemak.insert(0x16, KEY_L); // U → L
        colemak.insert(0x17, KEY_U); // I → U
        colemak.insert(0x18, KEY_Y); // O → Y
        colemak.insert(0x19, KEY_SEMICOLON); // P → ;
        colemak.insert(0x1F, KEY_R); // S → R
        colemak.insert(0x20, KEY_S); // D → S
        colemak.insert(0x21, KEY_T); // F → T
        colemak.insert(0x22, KEY_D); // G → D
        colemak.insert(0x24, KEY_N); // J → N
        colemak.insert(0x25, KEY_E); // K → E
        colemak.insert(0x26, KEY_I); // L → I
        colemak.insert(0x27, KEY_O); // ; → O
        colemak.insert(0x31, KEY_K); // N → K
        self.layout_overrides.insert("colemak".to_string(), colemak);
    }

    /// Translate RDP scancode to Linux evdev keycode
    pub(crate) fn translate_scancode(
        &self,
        scancode: u32,
        extended: bool,
        e1_prefix: bool,
    ) -> Result<u32> {
        if e1_prefix {
            // Handle E1 prefix scancodes
            self.e1_map
                .get(&scancode)
                .copied()
                .ok_or(InputError::UnknownScancode(scancode as u16))
        } else if extended {
            // Handle E0 prefix (extended) scancodes
            let extended_scan = 0xE000 | (scancode as u16 & 0xFF);
            self.extended_map
                .get(&extended_scan)
                .or_else(|| self.primary_map.get(&(scancode as u16)))
                .copied()
                .ok_or(InputError::UnknownScancode(extended_scan))
        } else {
            // Check for layout-specific overrides first
            if let Some(overrides) = self.layout_overrides.get(&self.current_layout)
                && let Some(keycode) = overrides.get(&(scancode as u16))
            {
                return Ok(*keycode);
            }
            // Standard scancode translation
            self.primary_map
                .get(&(scancode as u16))
                .copied()
                .ok_or(InputError::UnknownScancode(scancode as u16))
        }
    }

    /// Set keyboard layout
    pub(crate) fn set_layout(&mut self, layout: &str) {
        self.current_layout = layout.to_string();
    }

    /// Get current keyboard layout
    pub(crate) fn layout(&self) -> &str {
        &self.current_layout
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scancode_mapper_creation() {
        let mapper = ScancodeMapper::new();
        assert_eq!(mapper.layout(), "us");
    }

    #[test]
    fn test_primary_scancode_mapping() {
        let mapper = ScancodeMapper::new();

        // Test letter keys
        assert_eq!(
            mapper.translate_scancode(0x1E, false, false).unwrap(),
            KEY_A
        );
        assert_eq!(
            mapper.translate_scancode(0x2C, false, false).unwrap(),
            KEY_Z
        );

        // Test number keys
        assert_eq!(
            mapper.translate_scancode(0x02, false, false).unwrap(),
            KEY_1
        );
        assert_eq!(
            mapper.translate_scancode(0x0B, false, false).unwrap(),
            KEY_0
        );

        // Test function keys
        assert_eq!(
            mapper.translate_scancode(0x3B, false, false).unwrap(),
            KEY_F1
        );
        assert_eq!(
            mapper.translate_scancode(0x58, false, false).unwrap(),
            KEY_F12
        );
    }

    #[test]
    fn test_extended_scancode_mapping() {
        let mapper = ScancodeMapper::new();

        // Test navigation keys
        assert_eq!(
            mapper.translate_scancode(0x47, true, false).unwrap(),
            KEY_HOME
        );
        assert_eq!(
            mapper.translate_scancode(0x4F, true, false).unwrap(),
            KEY_END
        );
        assert_eq!(
            mapper.translate_scancode(0x48, true, false).unwrap(),
            KEY_UP
        );
        assert_eq!(
            mapper.translate_scancode(0x50, true, false).unwrap(),
            KEY_DOWN
        );
        assert_eq!(
            mapper.translate_scancode(0x4B, true, false).unwrap(),
            KEY_LEFT
        );
        assert_eq!(
            mapper.translate_scancode(0x4D, true, false).unwrap(),
            KEY_RIGHT
        );

        // Test media keys
        assert_eq!(
            mapper.translate_scancode(0x22, true, false).unwrap(),
            KEY_PLAYPAUSE
        );
        assert_eq!(
            mapper.translate_scancode(0x24, true, false).unwrap(),
            KEY_STOPCD
        );
    }

    #[test]
    fn test_layout_override() {
        let mut mapper = ScancodeMapper::new();

        // US layout: Y key
        assert_eq!(
            mapper.translate_scancode(0x15, false, false).unwrap(),
            KEY_Y
        );

        // German layout: Y → Z
        mapper.set_layout("de");
        assert_eq!(
            mapper.translate_scancode(0x15, false, false).unwrap(),
            KEY_Z
        );

        // French layout
        mapper.set_layout("fr");
        assert_eq!(
            mapper.translate_scancode(0x10, false, false).unwrap(),
            KEY_A
        );
    }

    #[test]
    fn test_unknown_scancode() {
        let mapper = ScancodeMapper::new();

        // Test unmapped scancode
        let result = mapper.translate_scancode(0xFF, false, false);
        assert!(result.is_err());
        match result {
            Err(InputError::UnknownScancode(_)) => {}
            _ => panic!("Expected UnknownScancode error"),
        }
    }

    #[test]
    fn test_function_keys_f13_to_f24() {
        let mapper = ScancodeMapper::new();

        assert_eq!(
            mapper.translate_scancode(0x5A, false, false).unwrap(),
            KEY_F13
        );
        assert_eq!(
            mapper.translate_scancode(0x65, false, false).unwrap(),
            KEY_F24
        );
    }

    #[test]
    fn test_multimedia_keys() {
        let mapper = ScancodeMapper::new();

        assert_eq!(
            mapper.translate_scancode(0x20, true, false).unwrap(),
            KEY_MUTE
        );
        assert_eq!(
            mapper.translate_scancode(0x2E, true, false).unwrap(),
            KEY_VOLUMEDOWN
        );
        assert_eq!(
            mapper.translate_scancode(0x30, true, false).unwrap(),
            KEY_VOLUMEUP
        );
    }

    #[test]
    fn test_japanese_keys() {
        let mapper = ScancodeMapper::new();

        assert_eq!(
            mapper.translate_scancode(0x70, false, false).unwrap(),
            KEY_KATAKANAHIRAGANA
        );
        assert_eq!(
            mapper.translate_scancode(0x71, false, false).unwrap(),
            KEY_MUHENKAN
        );
        assert_eq!(
            mapper.translate_scancode(0x72, false, false).unwrap(),
            KEY_HENKAN
        );
    }

    #[test]
    fn test_korean_keys() {
        let mapper = ScancodeMapper::new();

        assert_eq!(
            mapper.translate_scancode(0x75, false, false).unwrap(),
            KEY_HANGEUL
        );
        assert_eq!(
            mapper.translate_scancode(0x76, false, false).unwrap(),
            KEY_HANJA
        );
    }
}
