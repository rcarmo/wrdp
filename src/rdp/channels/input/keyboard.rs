//! Keyboard event handling for the RDP input channel.
//!
//! Handles scancode translation, modifier state tracking, repeat handling,
//! and layout selection for key events.

use crate::rdp::channels::input::error::Result;
use crate::rdp::channels::input::mapper::ScancodeMapper;
use std::collections::HashSet;
use std::time::Instant;
use tracing::debug;

/// Keyboard modifiers
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct KeyModifiers {
    /// Left or right Shift pressed
    pub shift: bool,
    /// Left or right Ctrl pressed
    pub ctrl: bool,
    /// Left or right Alt pressed
    pub alt: bool,
    /// Left or right Meta/Super/Windows key pressed
    pub meta: bool,
    /// Caps Lock active
    pub caps_lock: bool,
    /// Num Lock active
    pub num_lock: bool,
    /// Scroll Lock active
    pub scroll_lock: bool,
}

/// Keyboard event types
#[derive(Debug, Clone)]
pub enum KeyboardEvent {
    /// Duplicate state from overlapping input transports; do not inject.
    Ignored { keycode: u32 },

    /// Key pressed
    KeyDown {
        /// Linux evdev keycode
        keycode: u32,
        /// RDP scancode
        scancode: u16,
        /// Current modifiers
        modifiers: KeyModifiers,
        /// Event timestamp
        timestamp: Instant,
    },

    /// Key released
    KeyUp {
        /// Linux evdev keycode
        keycode: u32,
        /// RDP scancode
        scancode: u16,
        /// Current modifiers
        modifiers: KeyModifiers,
        /// Event timestamp
        timestamp: Instant,
    },

    /// Key repeat
    KeyRepeat {
        /// Linux evdev keycode
        keycode: u32,
        /// RDP scancode
        scancode: u16,
        /// Current modifiers
        modifiers: KeyModifiers,
        /// Event timestamp
        timestamp: Instant,
    },
}

/// Keyboard event handler state.
pub struct KeyboardHandler {
    /// Scancode mapper
    mapper: ScancodeMapper,

    /// Currently pressed keys
    pressed_keys: HashSet<u32>,

    /// Current modifiers
    modifiers: KeyModifiers,

    /// Last accepted event timestamp for each key.
    last_key_times: std::collections::HashMap<u32, Instant>,

    /// Keys that have passed the initial repeat delay.
    repeating_keys: HashSet<u32>,

    /// Key repeat delay (milliseconds).
    repeat_delay_ms: u64,

    /// Key repeat rate (milliseconds between repeats)
    repeat_rate_ms: u64,
}

impl KeyboardHandler {
    /// Create a new keyboard handler
    pub fn new() -> Self {
        Self {
            mapper: ScancodeMapper::new(),
            pressed_keys: HashSet::new(),
            modifiers: KeyModifiers::default(),
            last_key_times: std::collections::HashMap::new(),
            repeating_keys: HashSet::new(),
            repeat_delay_ms: 500,
            repeat_rate_ms: 33,
        }
    }

    /// Process a key-down event from RDP input.
    pub fn handle_key_down(
        &mut self,
        scancode: u16,
        extended: bool,
        e1_prefix: bool,
    ) -> Result<KeyboardEvent> {
        // Translate scancode to keycode
        let keycode = self
            .mapper
            .translate_scancode(scancode as u32, extended, e1_prefix)?;

        let timestamp = Instant::now();

        // Check if this is a repeat (key already pressed)
        let is_repeat = self.pressed_keys.contains(&keycode);

        if is_repeat {
            let last = self
                .last_key_times
                .get(&keycode)
                .copied()
                .unwrap_or(timestamp);
            let elapsed = timestamp.duration_since(last).as_millis() as u64;
            if !self.repeating_keys.contains(&keycode) {
                if elapsed < self.repeat_delay_ms {
                    return Ok(KeyboardEvent::Ignored { keycode });
                }
                self.repeating_keys.insert(keycode);
            } else if elapsed < self.repeat_rate_ms {
                return Ok(KeyboardEvent::Ignored { keycode });
            }
        }

        // Update pressed keys
        self.pressed_keys.insert(keycode);
        self.last_key_times.insert(keycode, timestamp);

        // Update modifiers
        self.update_modifiers(keycode, true);

        if is_repeat {
            Ok(KeyboardEvent::KeyRepeat {
                keycode,
                scancode,
                modifiers: self.modifiers,
                timestamp,
            })
        } else {
            Ok(KeyboardEvent::KeyDown {
                keycode,
                scancode,
                modifiers: self.modifiers,
                timestamp,
            })
        }
    }

    /// Process a key-up event from RDP input.
    pub fn handle_key_up(
        &mut self,
        scancode: u16,
        extended: bool,
        e1_prefix: bool,
    ) -> Result<KeyboardEvent> {
        // Translate scancode to keycode
        let keycode = self
            .mapper
            .translate_scancode(scancode as u32, extended, e1_prefix)?;

        let timestamp = Instant::now();

        if !self.pressed_keys.contains(&keycode) {
            return Ok(KeyboardEvent::Ignored { keycode });
        }

        // Remove from pressed keys
        self.pressed_keys.remove(&keycode);
        self.last_key_times.remove(&keycode);
        self.repeating_keys.remove(&keycode);

        // Update modifiers
        self.update_modifiers(keycode, false);

        Ok(KeyboardEvent::KeyUp {
            keycode,
            scancode,
            modifiers: self.modifiers,
            timestamp,
        })
    }

    /// Update modifier states based on key event
    fn update_modifiers(&mut self, keycode: u32, pressed: bool) {
        #[expect(clippy::wildcard_imports)]
        use crate::rdp::channels::input::mapper::keycodes::*;

        match keycode {
            KEY_LEFTSHIFT | KEY_RIGHTSHIFT => {
                if pressed {
                    self.modifiers.shift = true;
                } else {
                    // Only clear if neither shift is pressed
                    self.modifiers.shift =
                        self.is_key_pressed(KEY_LEFTSHIFT) || self.is_key_pressed(KEY_RIGHTSHIFT);
                }
            }
            KEY_LEFTCTRL | KEY_RIGHTCTRL => {
                if pressed {
                    self.modifiers.ctrl = true;
                } else {
                    self.modifiers.ctrl =
                        self.is_key_pressed(KEY_LEFTCTRL) || self.is_key_pressed(KEY_RIGHTCTRL);
                }
            }
            KEY_LEFTALT | KEY_RIGHTALT => {
                if pressed {
                    self.modifiers.alt = true;
                } else {
                    self.modifiers.alt =
                        self.is_key_pressed(KEY_LEFTALT) || self.is_key_pressed(KEY_RIGHTALT);
                }
            }
            KEY_LEFTMETA | KEY_RIGHTMETA => {
                if pressed {
                    self.modifiers.meta = true;
                } else {
                    self.modifiers.meta =
                        self.is_key_pressed(KEY_LEFTMETA) || self.is_key_pressed(KEY_RIGHTMETA);
                }
            }
            KEY_CAPSLOCK => {
                if pressed {
                    self.modifiers.caps_lock = !self.modifiers.caps_lock;
                }
            }
            KEY_NUMLOCK => {
                if pressed {
                    self.modifiers.num_lock = !self.modifiers.num_lock;
                }
            }
            KEY_SCROLLLOCK if pressed => {
                self.modifiers.scroll_lock = !self.modifiers.scroll_lock;
            }
            _ => {}
        }
    }

    /// Synchronize client lock-key state and return evdev keys that must be
    /// toggled on the compositor virtual keyboard.
    pub fn synchronize_locks(
        &mut self,
        caps_lock: bool,
        num_lock: bool,
        scroll_lock: bool,
    ) -> Vec<u32> {
        use crate::rdp::channels::input::mapper::keycodes::{
            KEY_CAPSLOCK, KEY_NUMLOCK, KEY_SCROLLLOCK,
        };
        let mut toggles = Vec::new();
        for (current, desired, keycode) in [
            (&mut self.modifiers.caps_lock, caps_lock, KEY_CAPSLOCK),
            (&mut self.modifiers.num_lock, num_lock, KEY_NUMLOCK),
            (&mut self.modifiers.scroll_lock, scroll_lock, KEY_SCROLLLOCK),
        ] {
            if *current != desired {
                *current = desired;
                toggles.push(keycode);
            }
        }
        toggles
    }

    /// Check if a key is currently pressed
    pub fn is_key_pressed(&self, keycode: u32) -> bool {
        self.pressed_keys.contains(&keycode)
    }

    /// Get current modifiers
    pub fn modifiers(&self) -> KeyModifiers {
        self.modifiers
    }

    /// Set keyboard layout
    pub fn set_layout(&mut self, layout: &str) {
        self.mapper.set_layout(layout);
        debug!("Keyboard layout changed");
    }

    /// Get current keyboard layout
    pub fn layout(&self) -> &str {
        self.mapper.layout()
    }

    /// Set initial key repeat delay.
    pub fn set_repeat_delay(&mut self, delay_ms: u64) {
        self.repeat_delay_ms = delay_ms;
    }

    /// Set key repeat rate
    pub fn set_repeat_rate(&mut self, rate_ms: u64) {
        self.repeat_rate_ms = rate_ms;
    }

    /// Reset keyboard state (release all keys)
    pub fn reset(&mut self) {
        self.pressed_keys.clear();
        self.last_key_times.clear();
        self.repeating_keys.clear();
        self.modifiers = KeyModifiers::default();
        debug!("Keyboard state reset");
    }

    /// Get number of currently pressed keys
    pub fn pressed_key_count(&self) -> usize {
        self.pressed_keys.len()
    }

    /// Get all currently pressed keys
    pub fn get_pressed_keys(&self) -> Vec<u32> {
        self.pressed_keys.iter().copied().collect()
    }
}

impl Default for KeyboardHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_synchronization_returns_only_changed_keys() {
        use crate::rdp::channels::input::mapper::keycodes::{KEY_CAPSLOCK, KEY_NUMLOCK};
        let mut keyboard = KeyboardHandler::new();
        assert_eq!(
            keyboard.synchronize_locks(true, true, false),
            vec![KEY_CAPSLOCK, KEY_NUMLOCK]
        );
        assert!(keyboard.synchronize_locks(true, true, false).is_empty());
        assert_eq!(
            keyboard.synchronize_locks(false, true, false),
            vec![KEY_CAPSLOCK]
        );
    }

    #[test]
    fn test_keyboard_handler_creation() {
        let handler = KeyboardHandler::new();
        assert_eq!(handler.pressed_key_count(), 0);
        assert!(!handler.modifiers().shift);
    }

    #[test]
    fn test_key_press_release() {
        let mut handler = KeyboardHandler::new();

        // Press A key (scancode 0x1E)
        let event = handler.handle_key_down(0x1E, false, false).unwrap();

        match event {
            KeyboardEvent::KeyDown { keycode, .. } => {
                assert!(keycode > 0);
                assert!(handler.is_key_pressed(keycode));
            }
            _ => panic!("Expected KeyDown event"),
        }

        assert_eq!(handler.pressed_key_count(), 1);

        // Release A key
        let event = handler.handle_key_up(0x1E, false, false).unwrap();

        match event {
            KeyboardEvent::KeyUp { keycode, .. } => {
                assert!(!handler.is_key_pressed(keycode));
            }
            _ => panic!("Expected KeyUp event"),
        }

        assert_eq!(handler.pressed_key_count(), 0);
    }

    #[test]
    fn test_modifier_tracking() {
        let mut handler = KeyboardHandler::new();

        // Press left shift (scancode 0x2A)
        handler.handle_key_down(0x2A, false, false).unwrap();
        assert!(handler.modifiers().shift);

        // Press left ctrl (scancode 0x1D)
        handler.handle_key_down(0x1D, false, false).unwrap();
        assert!(handler.modifiers().ctrl);

        // Release left shift
        handler.handle_key_up(0x2A, false, false).unwrap();
        assert!(!handler.modifiers().shift);
        assert!(handler.modifiers().ctrl);

        // Release left ctrl
        handler.handle_key_up(0x1D, false, false).unwrap();
        assert!(!handler.modifiers().ctrl);
    }

    #[test]
    fn test_caps_lock_toggle() {
        let mut handler = KeyboardHandler::new();

        assert!(!handler.modifiers().caps_lock);

        // Press Caps Lock (scancode 0x3A)
        handler.handle_key_down(0x3A, false, false).unwrap();
        assert!(handler.modifiers().caps_lock);

        // Release Caps Lock
        handler.handle_key_up(0x3A, false, false).unwrap();
        assert!(handler.modifiers().caps_lock); // Should stay on

        // Press again to toggle off
        handler.handle_key_down(0x3A, false, false).unwrap();
        assert!(!handler.modifiers().caps_lock);
    }

    #[test]
    fn test_multiple_modifiers() {
        let mut handler = KeyboardHandler::new();

        // Press Shift + Ctrl + Alt
        handler.handle_key_down(0x2A, false, false).unwrap(); // Left Shift
        handler.handle_key_down(0x1D, false, false).unwrap(); // Left Ctrl
        handler.handle_key_down(0x38, false, false).unwrap(); // Left Alt

        let mods = handler.modifiers();
        assert!(mods.shift);
        assert!(mods.ctrl);
        assert!(mods.alt);
    }

    #[test]
    fn test_both_shifts() {
        let mut handler = KeyboardHandler::new();

        // Press left shift
        handler.handle_key_down(0x2A, false, false).unwrap();
        assert!(handler.modifiers().shift);

        // Press right shift too
        handler.handle_key_down(0x36, false, false).unwrap();
        assert!(handler.modifiers().shift);

        // Release left shift
        handler.handle_key_up(0x2A, false, false).unwrap();
        assert!(handler.modifiers().shift); // Should still be on (right shift still pressed)

        // Release right shift
        handler.handle_key_up(0x36, false, false).unwrap();
        assert!(!handler.modifiers().shift);
    }

    #[test]
    fn test_extended_key() {
        let mut handler = KeyboardHandler::new();

        // Press right ctrl (extended scancode 0xE01D)
        let event = handler.handle_key_down(0x1D, true, false).unwrap();

        match event {
            KeyboardEvent::KeyDown { keycode, .. } => {
                assert!(keycode > 0);
            }
            _ => panic!("Expected KeyDown event"),
        }

        assert!(handler.modifiers().ctrl);
    }

    #[test]
    fn test_layout_change() {
        let mut handler = KeyboardHandler::new();

        assert_eq!(handler.layout(), "us");

        handler.set_layout("de");
        assert_eq!(handler.layout(), "de");
    }

    #[test]
    fn test_reset() {
        let mut handler = KeyboardHandler::new();

        // Press several keys
        handler.handle_key_down(0x1E, false, false).unwrap(); // A
        handler.handle_key_down(0x2A, false, false).unwrap(); // Shift
        handler.handle_key_down(0x1D, false, false).unwrap(); // Ctrl

        assert!(handler.pressed_key_count() > 0);
        assert!(handler.modifiers().shift);

        // Reset
        handler.reset();

        assert_eq!(handler.pressed_key_count(), 0);
        assert!(!handler.modifiers().shift);
        assert!(!handler.modifiers().ctrl);
    }

    #[test]
    fn test_get_pressed_keys() {
        let mut handler = KeyboardHandler::new();

        handler.handle_key_down(0x1E, false, false).unwrap(); // A
        handler.handle_key_down(0x1F, false, false).unwrap(); // S

        let pressed = handler.get_pressed_keys();
        assert_eq!(pressed.len(), 2);
    }

    #[test]
    fn duplicate_transport_down_is_not_a_repeat() {
        let mut handler = KeyboardHandler::new();
        handler.set_repeat_delay(5);
        handler.set_repeat_rate(3);
        assert!(matches!(
            handler.handle_key_down(0x1E, false, false).unwrap(),
            KeyboardEvent::KeyDown { .. }
        ));
        assert!(matches!(
            handler.handle_key_down(0x1E, false, false).unwrap(),
            KeyboardEvent::Ignored { .. }
        ));
        std::thread::sleep(std::time::Duration::from_millis(6));
        assert!(matches!(
            handler.handle_key_down(0x1E, false, false).unwrap(),
            KeyboardEvent::KeyRepeat { .. }
        ));
        assert!(matches!(
            handler.handle_key_up(0x1E, false, false).unwrap(),
            KeyboardEvent::KeyUp { .. }
        ));
        assert!(matches!(
            handler.handle_key_up(0x1E, false, false).unwrap(),
            KeyboardEvent::Ignored { .. }
        ));
    }

    #[test]
    fn test_repeat_rate() {
        let mut handler = KeyboardHandler::new();

        handler.set_repeat_delay(100);
        handler.set_repeat_rate(50);
        assert_eq!(handler.repeat_delay_ms, 100);
        assert_eq!(handler.repeat_rate_ms, 50);
    }

    #[test]
    fn test_unknown_scancode() {
        let mut handler = KeyboardHandler::new();

        // Try invalid scancode
        let result = handler.handle_key_down(0xFF, false, false);
        assert!(result.is_err());
    }

    #[test]
    fn test_function_keys() {
        let mut handler = KeyboardHandler::new();

        // F1 (scancode 0x3B)
        let event = handler.handle_key_down(0x3B, false, false).unwrap();
        match event {
            KeyboardEvent::KeyDown { keycode, .. } => {
                assert!(keycode > 0);
            }
            _ => panic!("Expected KeyDown event"),
        }

        // F12 (scancode 0x58)
        let event = handler.handle_key_down(0x58, false, false).unwrap();
        match event {
            KeyboardEvent::KeyDown { keycode, .. } => {
                assert!(keycode > 0);
            }
            _ => panic!("Expected KeyDown event"),
        }
    }
}
