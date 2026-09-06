//! Mouse event handling for the RDP input channel.
//!
//! Handles pointer movement, button state transitions, and wheel scrolling
//! with coordinate transformation and button mapping.

use crate::rdp::channels::input::coordinates::CoordinateTransformer;
use crate::rdp::channels::input::error::Result;
use std::time::Instant;
/// Mouse button identifiers
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MouseButton {
    /// Left mouse button
    Left,
    /// Right mouse button
    Right,
    /// Middle mouse button
    Middle,
    /// Extra button 1 (side button)
    Extra1,
    /// Extra button 2 (side button)
    Extra2,
}

impl MouseButton {
    /// Convert to Linux button code
    pub fn to_linux_button(self) -> u32 {
        match self {
            MouseButton::Left => 0x110,   // BTN_LEFT
            MouseButton::Right => 0x111,  // BTN_RIGHT
            MouseButton::Middle => 0x112, // BTN_MIDDLE
            MouseButton::Extra1 => 0x113, // BTN_SIDE
            MouseButton::Extra2 => 0x114, // BTN_EXTRA
        }
    }

    /// Convert from RDP button flags
    pub fn from_rdp_button(button: u16) -> Option<Self> {
        match button {
            0x1000 => Some(MouseButton::Left),
            0x2000 => Some(MouseButton::Right),
            0x4000 => Some(MouseButton::Middle),
            0x0080 => Some(MouseButton::Extra1),
            0x0100 => Some(MouseButton::Extra2),
            _ => None,
        }
    }
}

/// Mouse event types
#[derive(Debug, Clone)]
pub enum MouseEvent {
    /// Mouse moved to absolute position
    Move {
        /// X coordinate
        x: f64,
        /// Y coordinate
        y: f64,
        /// Event timestamp
        timestamp: Instant,
    },

    /// Mouse button pressed
    ButtonDown {
        /// Button that was pressed
        button: MouseButton,
        /// Event timestamp
        timestamp: Instant,
    },

    /// Mouse button released
    ButtonUp {
        /// Button that was released
        button: MouseButton,
        /// Event timestamp
        timestamp: Instant,
    },

    /// Mouse wheel scrolled
    Scroll {
        /// Horizontal scroll delta
        delta_x: i32,
        /// Vertical scroll delta
        delta_y: i32,
        /// Event timestamp
        timestamp: Instant,
    },
}

/// Mouse event handler state.
pub struct MouseHandler {
    /// Current mouse position (stream coordinates)
    current_x: f64,
    current_y: f64,

    /// Button states
    button_states: [bool; 5],

    /// Last event timestamp
    last_event_time: Option<Instant>,

    /// Release timestamps used to suppress a delayed duplicate transport path.
    last_button_release: [Option<Instant>; 5],

    /// Enable high-precision scrolling
    high_precision_scroll: bool,

    /// Scroll accumulator for high-precision scrolling
    scroll_accum_x: f64,
    scroll_accum_y: f64,
}

impl MouseHandler {
    /// Create a new mouse handler
    pub fn new() -> Self {
        Self {
            current_x: 0.0,
            current_y: 0.0,
            button_states: [false; 5],
            last_event_time: None,
            last_button_release: [None; 5],
            high_precision_scroll: true,
            scroll_accum_x: 0.0,
            scroll_accum_y: 0.0,
        }
    }

    /// Process absolute pointer movement from RDP input.
    pub fn handle_absolute_move(
        &mut self,
        rdp_x: u32,
        rdp_y: u32,
        transformer: &mut CoordinateTransformer,
    ) -> Result<MouseEvent> {
        let (stream_x, stream_y) = transformer.rdp_to_stream(rdp_x, rdp_y)?;

        // Clamp to bounds
        let (stream_x, stream_y) = transformer.clamp_to_bounds(stream_x, stream_y);

        self.current_x = stream_x;
        self.current_y = stream_y;

        let timestamp = Instant::now();
        self.last_event_time = Some(timestamp);

        Ok(MouseEvent::Move {
            x: stream_x,
            y: stream_y,
            timestamp,
        })
    }

    /// Process relative pointer movement from RDP input.
    pub fn handle_relative_move(
        &mut self,
        delta_x: i32,
        delta_y: i32,
        transformer: &mut CoordinateTransformer,
    ) -> Result<MouseEvent> {
        let (stream_x, stream_y) = transformer.apply_relative_movement(delta_x, delta_y)?;

        // Clamp to bounds
        let (stream_x, stream_y) = transformer.clamp_to_bounds(stream_x, stream_y);

        self.current_x = stream_x;
        self.current_y = stream_y;

        let timestamp = Instant::now();
        self.last_event_time = Some(timestamp);

        Ok(MouseEvent::Move {
            x: stream_x,
            y: stream_y,
            timestamp,
        })
    }

    /// Accept only real button-state transitions. Clients that send both core
    /// and Advanced Input events can otherwise duplicate a physical click.
    pub fn accept_button_transition(&mut self, button: MouseButton, pressed: bool) -> bool {
        self.accept_button_transition_at(button, pressed, Instant::now())
    }

    fn accept_button_transition_at(
        &mut self,
        button: MouseButton,
        pressed: bool,
        now: Instant,
    ) -> bool {
        const DUPLICATE_PATH_WINDOW: std::time::Duration = std::time::Duration::from_millis(8);
        let index = Self::button_to_index(button);
        if self.button_states[index] == pressed {
            return false;
        }
        if pressed
            && self.last_button_release[index]
                .is_some_and(|released| now.duration_since(released) < DUPLICATE_PATH_WINDOW)
        {
            return false;
        }
        self.button_states[index] = pressed;
        if !pressed {
            self.last_button_release[index] = Some(now);
        }
        true
    }

    /// Process mouse button press
    pub fn handle_button_down(&mut self, button: MouseButton) -> Result<MouseEvent> {
        let timestamp = Instant::now();
        self.last_event_time = Some(timestamp);

        Ok(MouseEvent::ButtonDown { button, timestamp })
    }

    /// Process mouse button release
    pub fn handle_button_up(&mut self, button: MouseButton) -> Result<MouseEvent> {
        let timestamp = Instant::now();
        self.last_event_time = Some(timestamp);

        Ok(MouseEvent::ButtonUp { button, timestamp })
    }

    /// Process wheel-scroll input.
    pub fn handle_scroll(&mut self, delta_x: i32, delta_y: i32) -> Result<MouseEvent> {
        let timestamp = Instant::now();
        self.last_event_time = Some(timestamp);

        let (final_delta_x, final_delta_y) = if self.high_precision_scroll {
            // Accumulate fractional scrolling
            self.scroll_accum_x += delta_x as f64 / 120.0;
            self.scroll_accum_y += delta_y as f64 / 120.0;

            let x = self.scroll_accum_x.trunc() as i32;
            let y = self.scroll_accum_y.trunc() as i32;

            self.scroll_accum_x -= x as f64;
            self.scroll_accum_y -= y as f64;

            (x, y)
        } else {
            // Standard scrolling
            (delta_x / 120, delta_y / 120)
        };

        Ok(MouseEvent::Scroll {
            delta_x: final_delta_x,
            delta_y: final_delta_y,
            timestamp,
        })
    }

    /// Get current mouse position
    pub fn current_position(&self) -> (f64, f64) {
        (self.current_x, self.current_y)
    }

    pub fn pressed_buttons(&self) -> Vec<MouseButton> {
        [
            MouseButton::Left,
            MouseButton::Right,
            MouseButton::Middle,
            MouseButton::Extra1,
            MouseButton::Extra2,
        ]
        .into_iter()
        .filter(|button| self.is_button_pressed(*button))
        .collect()
    }

    /// Check if button is currently pressed
    pub fn is_button_pressed(&self, button: MouseButton) -> bool {
        let index = Self::button_to_index(button);
        self.button_states[index]
    }

    /// Get time since last event
    pub fn time_since_last_event(&self) -> Option<std::time::Duration> {
        self.last_event_time.map(|t| t.elapsed())
    }

    /// Set high-precision scrolling enabled
    pub fn set_high_precision_scroll(&mut self, enabled: bool) {
        self.high_precision_scroll = enabled;
        if !enabled {
            self.scroll_accum_x = 0.0;
            self.scroll_accum_y = 0.0;
        }
    }

    /// Convert button to array index
    fn button_to_index(button: MouseButton) -> usize {
        match button {
            MouseButton::Left => 0,
            MouseButton::Right => 1,
            MouseButton::Middle => 2,
            MouseButton::Extra1 => 3,
            MouseButton::Extra2 => 4,
        }
    }

    /// Reset mouse state
    pub fn reset(&mut self) {
        self.button_states = [false; 5];
        self.last_button_release = [None; 5];
        self.scroll_accum_x = 0.0;
        self.scroll_accum_y = 0.0;
    }
}

impl Default for MouseHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rdp::channels::input::coordinates::MonitorInfo;

    fn create_test_transformer() -> CoordinateTransformer {
        let monitor = MonitorInfo {
            id: 1,
            name: "Primary".to_string(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            dpi: 96.0,
            scale_factor: 1.0,
            stream_x: 0,
            stream_y: 0,
            stream_width: 1920,
            stream_height: 1080,
            is_primary: true,
        };

        CoordinateTransformer::new(vec![monitor]).unwrap()
    }

    #[test]
    fn duplicate_button_states_are_suppressed() {
        let mut mouse = MouseHandler::new();
        let start = Instant::now();
        assert!(mouse.accept_button_transition_at(MouseButton::Left, true, start));
        assert!(!mouse.accept_button_transition_at(MouseButton::Left, true, start));
        assert!(mouse.accept_button_transition_at(
            MouseButton::Left,
            false,
            start + std::time::Duration::from_millis(1)
        ));
        assert!(!mouse.accept_button_transition_at(
            MouseButton::Left,
            true,
            start + std::time::Duration::from_millis(2)
        ));
        assert!(!mouse.accept_button_transition_at(
            MouseButton::Left,
            false,
            start + std::time::Duration::from_millis(3)
        ));
        assert!(mouse.accept_button_transition_at(
            MouseButton::Left,
            true,
            start + std::time::Duration::from_millis(20)
        ));
    }

    #[test]
    fn test_mouse_handler_creation() {
        let handler = MouseHandler::new();
        let (x, y) = handler.current_position();
        assert_eq!(x, 0.0);
        assert_eq!(y, 0.0);
    }

    #[test]
    fn test_absolute_move() {
        let mut handler = MouseHandler::new();
        let mut transformer = create_test_transformer();

        let event = handler
            .handle_absolute_move(960, 540, &mut transformer)
            .unwrap();

        match event {
            MouseEvent::Move { x, y, .. } => {
                assert!(x > 0.0);
                assert!(y > 0.0);
            }
            _ => panic!("Expected Move event"),
        }

        let (x, y) = handler.current_position();
        assert!(x > 0.0);
        assert!(y > 0.0);
    }

    #[test]
    fn test_relative_move() {
        let mut handler = MouseHandler::new();
        let mut transformer = create_test_transformer();

        let event = handler
            .handle_relative_move(10, 10, &mut transformer)
            .unwrap();

        match event {
            MouseEvent::Move { .. } => {}
            _ => panic!("Expected Move event"),
        }
    }

    #[test]
    fn test_button_press_release() {
        let mut handler = MouseHandler::new();

        // Press left button through the same transition gate used in production.
        assert!(handler.accept_button_transition(MouseButton::Left, true));
        let event = handler.handle_button_down(MouseButton::Left).unwrap();
        match event {
            MouseEvent::ButtonDown { button, .. } => {
                assert_eq!(button, MouseButton::Left);
            }
            _ => panic!("Expected ButtonDown event"),
        }

        assert!(handler.is_button_pressed(MouseButton::Left));

        // Release left button through the same transition gate used in production.
        assert!(handler.accept_button_transition(MouseButton::Left, false));
        let event = handler.handle_button_up(MouseButton::Left).unwrap();
        match event {
            MouseEvent::ButtonUp { button, .. } => {
                assert_eq!(button, MouseButton::Left);
            }
            _ => panic!("Expected ButtonUp event"),
        }

        assert!(!handler.is_button_pressed(MouseButton::Left));
    }

    #[test]
    fn test_scroll_event() {
        let mut handler = MouseHandler::new();

        let event = handler.handle_scroll(0, 120).unwrap();

        match event {
            MouseEvent::Scroll { delta_y, .. } => {
                assert_eq!(delta_y, 1);
            }
            _ => panic!("Expected Scroll event"),
        }
    }

    #[test]
    fn test_high_precision_scroll() {
        let mut handler = MouseHandler::new();
        handler.set_high_precision_scroll(true);

        // Send small scroll increments
        for _ in 0..10 {
            let _ = handler.handle_scroll(0, 12); // 1/10 of a standard scroll unit
        }

        // Should accumulate to one full scroll unit
        let event = handler.handle_scroll(0, 0).unwrap();
        match event {
            MouseEvent::Scroll { delta_y, .. } => {
                assert_eq!(delta_y, 0); // Accumulated but not yet reached threshold
            }
            _ => panic!("Expected Scroll event"),
        }
    }

    #[test]
    fn test_mouse_button_to_linux() {
        assert_eq!(MouseButton::Left.to_linux_button(), 0x110);
        assert_eq!(MouseButton::Right.to_linux_button(), 0x111);
        assert_eq!(MouseButton::Middle.to_linux_button(), 0x112);
        assert_eq!(MouseButton::Extra1.to_linux_button(), 0x113);
        assert_eq!(MouseButton::Extra2.to_linux_button(), 0x114);
    }

    #[test]
    fn test_mouse_button_from_rdp() {
        assert_eq!(
            MouseButton::from_rdp_button(0x1000),
            Some(MouseButton::Left)
        );
        assert_eq!(
            MouseButton::from_rdp_button(0x2000),
            Some(MouseButton::Right)
        );
        assert_eq!(
            MouseButton::from_rdp_button(0x4000),
            Some(MouseButton::Middle)
        );
        assert_eq!(
            MouseButton::from_rdp_button(0x0080),
            Some(MouseButton::Extra1)
        );
        assert_eq!(
            MouseButton::from_rdp_button(0x0100),
            Some(MouseButton::Extra2)
        );
        assert_eq!(MouseButton::from_rdp_button(0x9999), None);
    }

    #[test]
    fn test_multiple_button_states() {
        let mut handler = MouseHandler::new();

        assert!(handler.accept_button_transition(MouseButton::Left, true));
        handler.handle_button_down(MouseButton::Left).unwrap();
        assert!(handler.accept_button_transition(MouseButton::Right, true));
        handler.handle_button_down(MouseButton::Right).unwrap();

        assert!(handler.is_button_pressed(MouseButton::Left));
        assert!(handler.is_button_pressed(MouseButton::Right));
        assert!(!handler.is_button_pressed(MouseButton::Middle));

        assert!(handler.accept_button_transition(MouseButton::Left, false));
        handler.handle_button_up(MouseButton::Left).unwrap();

        assert!(!handler.is_button_pressed(MouseButton::Left));
        assert!(handler.is_button_pressed(MouseButton::Right));
    }

    #[test]
    fn test_mouse_reset() {
        let mut handler = MouseHandler::new();

        assert!(handler.accept_button_transition(MouseButton::Left, true));
        handler.handle_button_down(MouseButton::Left).unwrap();
        assert!(handler.accept_button_transition(MouseButton::Right, true));
        handler.handle_button_down(MouseButton::Right).unwrap();
        handler.scroll_accum_x = 5.0;
        handler.scroll_accum_y = 5.0;

        handler.reset();

        assert!(!handler.is_button_pressed(MouseButton::Left));
        assert!(!handler.is_button_pressed(MouseButton::Right));
        assert_eq!(handler.scroll_accum_x, 0.0);
        assert_eq!(handler.scroll_accum_y, 0.0);
    }

    #[test]
    fn test_time_since_last_event() {
        let mut handler = MouseHandler::new();

        assert!(handler.time_since_last_event().is_none());

        handler.handle_button_down(MouseButton::Left).unwrap();

        assert!(handler.time_since_last_event().is_some());
        assert!(handler.time_since_last_event().unwrap().as_millis() < 100);
    }
}
