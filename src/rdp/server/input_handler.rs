//! RDP input bridge.
//!
//! IronRDP delivers keyboard and pointer callbacks synchronously, while the
//! managed compositor session accepts asynchronous input injection. This module
//! queues those callbacks in order, translates keyboard
//! and pointer details, and forwards them to the active session handle. Android
//! pointer workarounds and CJK clipboard-paste fallback are kept here because
//! both are driven by incoming client input events.

use std::sync::{
    Arc, RwLock as StdRwLock,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

use ironrdp_pdu::pointer::PointerPositionAttribute;
use ironrdp_server::{
    DisplayUpdate, KeyboardEvent as IronKeyboardEvent, MouseEvent as IronMouseEvent, RGBAPointer,
    RdpServerInputHandler,
};
use tokio::sync::{Mutex, RwLock, mpsc};
use tracing::{debug, error, info, trace, warn};

/// Accumulates non-ASCII Unicode input units for clipboard-paste fallback.
///
/// RDP sends CJK input as a stream of UnicodePressed events. Since keysym
/// injection fails for many CJK characters on KDE, we buffer them and flush
/// via write_text + Ctrl+V when a non-Unicode event (keycode) arrives.
struct CjkPasteBuffer {
    buf: String,
    pending_high_surrogate: Option<u16>,
}

impl CjkPasteBuffer {
    fn new() -> Self {
        Self {
            buf: String::new(),
            pending_high_surrogate: None,
        }
    }

    fn push_char(&mut self, c: char) {
        self.buf.push(c);
    }

    /// Decode a UTF-16 surrogate pair and push the resulting char.
    /// Returns the decoded char if the pair is complete, None if `high` was stored
    /// waiting for a low surrogate, or None if the pair is invalid.
    fn push_surrogate_pair(&mut self, high: u16, low: u16) -> Option<char> {
        if !(0xD800..=0xDBFF).contains(&high) || !(0xDC00..=0xDFFF).contains(&low) {
            self.pending_high_surrogate = None;
            return None;
        }
        let code_point = 0x10000 + (u32::from(high - 0xD800) << 10) + u32::from(low - 0xDC00);
        let c = char::from_u32(code_point)?;
        self.buf.push(c);
        self.pending_high_surrogate = None;
        Some(c)
    }

    fn is_empty(&self) -> bool {
        self.buf.is_empty() && self.pending_high_surrogate.is_none()
    }

    /// Drain buffered text. Returns None if nothing was accumulated.
    fn take_text(&mut self) -> Option<String> {
        self.pending_high_surrogate = None;
        if self.buf.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut self.buf))
        }
    }
}

use crate::rdp::channels::clipboard::provider::ClipboardProvider;
use crate::rdp::channels::graphics::egfx::channel::HandlerState;
use crate::rdp::{
    channels::input::{
        CoordinateTransformer, InputError, KeyboardHandler, MonitorInfo, MouseButton, MouseEvent,
        MouseHandler,
    },
    session::{
        backend::StreamInfo,
        supervision::{SessionStatusEvent, SessionStatusReporter},
    },
};

/// Map a Unicode code point to an evdev keycode and whether Shift is needed.
/// Covers printable ASCII (0x20-0x7E) on US QWERTY layout.
fn unicode_to_evdev(cp: u16) -> Option<(u32, bool)> {
    // evdev keycodes from wrdp-input::mapper::keycodes
    const KEY_SPACE: u32 = 57;
    const KEY_1: u32 = 2;
    const KEY_2: u32 = 3;
    const KEY_3: u32 = 4;
    const KEY_4: u32 = 5;
    const KEY_5: u32 = 6;
    const KEY_6: u32 = 7;
    const KEY_7: u32 = 8;
    const KEY_8: u32 = 9;
    const KEY_9: u32 = 10;
    const KEY_0: u32 = 11;
    const KEY_MINUS: u32 = 12;
    const KEY_EQUAL: u32 = 13;
    const KEY_TAB: u32 = 15;
    const KEY_Q: u32 = 16;
    const KEY_W: u32 = 17;
    const KEY_E: u32 = 18;
    const KEY_R: u32 = 19;
    const KEY_T: u32 = 20;
    const KEY_Y: u32 = 21;
    const KEY_U: u32 = 22;
    const KEY_I: u32 = 23;
    const KEY_O: u32 = 24;
    const KEY_P: u32 = 25;
    const KEY_LEFTBRACE: u32 = 26;
    const KEY_RIGHTBRACE: u32 = 27;
    const KEY_ENTER: u32 = 28;
    const KEY_A: u32 = 30;
    const KEY_S: u32 = 31;
    const KEY_D: u32 = 32;
    const KEY_F: u32 = 33;
    const KEY_G: u32 = 34;
    const KEY_H: u32 = 35;
    const KEY_J: u32 = 36;
    const KEY_K: u32 = 37;
    const KEY_L: u32 = 38;
    const KEY_SEMICOLON: u32 = 39;
    const KEY_APOSTROPHE: u32 = 40;
    const KEY_GRAVE: u32 = 41;
    const KEY_BACKSLASH: u32 = 43;
    const KEY_Z: u32 = 44;
    const KEY_X: u32 = 45;
    const KEY_C: u32 = 46;
    const KEY_V: u32 = 47;
    const KEY_B: u32 = 48;
    const KEY_N: u32 = 49;
    const KEY_M: u32 = 50;
    const KEY_COMMA: u32 = 51;
    const KEY_DOT: u32 = 52;
    const KEY_SLASH: u32 = 53;

    // (evdev_keycode, needs_shift)
    match cp {
        // Whitespace
        0x20 => Some((KEY_SPACE, false)),        // ' '
        0x09 => Some((KEY_TAB, false)),          // Tab
        0x0A | 0x0D => Some((KEY_ENTER, false)), // Newline / CR

        // Digits
        0x30 => Some((KEY_0, false)), // '0'
        0x31 => Some((KEY_1, false)), // '1'
        0x32 => Some((KEY_2, false)), // '2'
        0x33 => Some((KEY_3, false)), // '3'
        0x34 => Some((KEY_4, false)), // '4'
        0x35 => Some((KEY_5, false)), // '5'
        0x36 => Some((KEY_6, false)), // '6'
        0x37 => Some((KEY_7, false)), // '7'
        0x38 => Some((KEY_8, false)), // '8'
        0x39 => Some((KEY_9, false)), // '9'

        // Lowercase letters
        0x61 => Some((KEY_A, false)), // 'a'
        0x62 => Some((KEY_B, false)),
        0x63 => Some((KEY_C, false)),
        0x64 => Some((KEY_D, false)),
        0x65 => Some((KEY_E, false)),
        0x66 => Some((KEY_F, false)),
        0x67 => Some((KEY_G, false)),
        0x68 => Some((KEY_H, false)),
        0x69 => Some((KEY_I, false)),
        0x6A => Some((KEY_J, false)),
        0x6B => Some((KEY_K, false)),
        0x6C => Some((KEY_L, false)),
        0x6D => Some((KEY_M, false)),
        0x6E => Some((KEY_N, false)),
        0x6F => Some((KEY_O, false)),
        0x70 => Some((KEY_P, false)),
        0x71 => Some((KEY_Q, false)),
        0x72 => Some((KEY_R, false)),
        0x73 => Some((KEY_S, false)),
        0x74 => Some((KEY_T, false)),
        0x75 => Some((KEY_U, false)),
        0x76 => Some((KEY_V, false)),
        0x77 => Some((KEY_W, false)),
        0x78 => Some((KEY_X, false)),
        0x79 => Some((KEY_Y, false)),
        0x7A => Some((KEY_Z, false)), // 'z'

        // Uppercase letters (same keys, with Shift)
        0x41 => Some((KEY_A, true)), // 'A'
        0x42 => Some((KEY_B, true)),
        0x43 => Some((KEY_C, true)),
        0x44 => Some((KEY_D, true)),
        0x45 => Some((KEY_E, true)),
        0x46 => Some((KEY_F, true)),
        0x47 => Some((KEY_G, true)),
        0x48 => Some((KEY_H, true)),
        0x49 => Some((KEY_I, true)),
        0x4A => Some((KEY_J, true)),
        0x4B => Some((KEY_K, true)),
        0x4C => Some((KEY_L, true)),
        0x4D => Some((KEY_M, true)),
        0x4E => Some((KEY_N, true)),
        0x4F => Some((KEY_O, true)),
        0x50 => Some((KEY_P, true)),
        0x51 => Some((KEY_Q, true)),
        0x52 => Some((KEY_R, true)),
        0x53 => Some((KEY_S, true)),
        0x54 => Some((KEY_T, true)),
        0x55 => Some((KEY_U, true)),
        0x56 => Some((KEY_V, true)),
        0x57 => Some((KEY_W, true)),
        0x58 => Some((KEY_X, true)),
        0x59 => Some((KEY_Y, true)),
        0x5A => Some((KEY_Z, true)), // 'Z'

        // Symbols (unshifted)
        0x2D => Some((KEY_MINUS, false)),      // '-'
        0x3D => Some((KEY_EQUAL, false)),      // '='
        0x5B => Some((KEY_LEFTBRACE, false)),  // '['
        0x5D => Some((KEY_RIGHTBRACE, false)), // ']'
        0x5C => Some((KEY_BACKSLASH, false)),  // '\'
        0x3B => Some((KEY_SEMICOLON, false)),  // ';'
        0x27 => Some((KEY_APOSTROPHE, false)), // '\''
        0x60 => Some((KEY_GRAVE, false)),      // '`'
        0x2C => Some((KEY_COMMA, false)),      // ','
        0x2E => Some((KEY_DOT, false)),        // '.'
        0x2F => Some((KEY_SLASH, false)),      // '/'

        // Symbols (shifted)
        0x21 => Some((KEY_1, true)),          // '!'
        0x40 => Some((KEY_2, true)),          // '@'
        0x23 => Some((KEY_3, true)),          // '#'
        0x24 => Some((KEY_4, true)),          // '$'
        0x25 => Some((KEY_5, true)),          // '%'
        0x5E => Some((KEY_6, true)),          // '^'
        0x26 => Some((KEY_7, true)),          // '&'
        0x2A => Some((KEY_8, true)),          // '*'
        0x28 => Some((KEY_9, true)),          // '('
        0x29 => Some((KEY_0, true)),          // ')'
        0x5F => Some((KEY_MINUS, true)),      // '_'
        0x2B => Some((KEY_EQUAL, true)),      // '+'
        0x7B => Some((KEY_LEFTBRACE, true)),  // '{'
        0x7D => Some((KEY_RIGHTBRACE, true)), // '}'
        0x7C => Some((KEY_BACKSLASH, true)),  // '|'
        0x3A => Some((KEY_SEMICOLON, true)),  // ':'
        0x22 => Some((KEY_APOSTROPHE, true)), // '"'
        0x7E => Some((KEY_GRAVE, true)),      // '~'
        0x3C => Some((KEY_COMMA, true)),      // '<'
        0x3E => Some((KEY_DOT, true)),        // '>'
        0x3F => Some((KEY_SLASH, true)),      // '?'

        _ => None,
    }
}

/// Convert an RDP Unicode input code unit into an XKB keysym.
///
/// X11/XKB represents Unicode characters outside Latin-1 as `0x01000000 | codepoint`.
/// RDP Unicode input delivers UTF-16 code units; the current IronRDP server API exposes
/// each unit as `u16`, so supplementary-plane characters that require surrogate pairs
/// cannot be represented as a single keysym here.
fn unicode_to_keysym(cp: u16) -> Option<i32> {
    match cp {
        0xD800..=0xDFFF => None,
        0x0000..=0x001F | 0x007F..=0x009F => None,
        0x0020..=0x00FF => Some(i32::from(cp)),
        _ => Some((0x0100_0000u32 | u32::from(cp)) as i32),
    }
}

fn portal_err(e: impl std::fmt::Display) -> InputError {
    InputError::PortalError(e.to_string())
}

/// wrdp input handler.
///
/// Bridges IronRDP input events to compositor-side input injection through the
/// active session handle.
///
/// IronRDP callbacks are synchronous while injection calls are async, so this
/// module uses ordered forwarding to keep input responsive.
/// Input event for multiplexing
#[derive(Debug)]
pub enum InputEvent {
    /// Keyboard event from RDP client
    Keyboard(IronKeyboardEvent),
    /// Mouse event from RDP client
    Mouse(IronMouseEvent),
}

fn input_event_is_droppable(event: &InputEvent) -> bool {
    match event {
        InputEvent::Keyboard(
            IronKeyboardEvent::Released { .. } | IronKeyboardEvent::UnicodeReleased(_),
        ) => false,
        InputEvent::Keyboard(IronKeyboardEvent::Synchronize(_)) => false,
        InputEvent::Keyboard(_) => true,
        InputEvent::Mouse(
            IronMouseEvent::Button { pressed: false, .. }
            | IronMouseEvent::ButtonRel { pressed: false, .. },
        ) => false,
        InputEvent::Mouse(_) => true,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueuePressureAction {
    Drop,
    FailConnection,
}

fn queue_pressure_action(event: &InputEvent) -> QueuePressureAction {
    if input_event_is_droppable(event) {
        QueuePressureAction::Drop
    } else {
        QueuePressureAction::FailConnection
    }
}

/// Number of consecutive injection failures that promotes a transient fault to
/// a permanent one. Below this the worker keeps retrying and logs on powers of
/// two; a single success resets the count and recovers.
const INPUT_FAILURE_THRESHOLD: u64 = 32;

/// Whether a run of consecutive injection failures has reached the permanent
/// threshold, meaning the worker must report `InputFailed` and exit.
///
/// Pure so the threshold policy can be unit-tested without a live backend.
fn injection_failures_are_terminal(consecutive_failures: u64) -> bool {
    consecutive_failures >= INPUT_FAILURE_THRESHOLD
}

/// Report a terminal input failure to supervision at most once.
///
/// Shared by the queue-overflow path and the injection-failure path so both
/// converge on a single permanent `InputFailed` event. Only fires once a
/// reporter is attached and the connection is already marked terminal.
fn report_input_failed_once(
    terminal_overflow: &AtomicBool,
    overflow_reported: &AtomicBool,
    health_reporter: &StdRwLock<Option<SessionStatusReporter>>,
    reason: &str,
) {
    if !terminal_overflow.load(Ordering::Acquire) || overflow_reported.load(Ordering::Acquire) {
        return;
    }
    let reporter = health_reporter
        .read()
        .ok()
        .and_then(|reporter| reporter.clone());
    if let Some(reporter) = reporter
        && overflow_reported
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    {
        reporter.report(SessionStatusEvent::InputFailed {
            reason: reason.to_string(),
            permanent: true,
        });
    }
}

/// Apply a primary resize without destroying the rest of the desktop topology.
///
/// The monitor's desktop dimensions describe the RDP-visible region, while its
/// stream dimensions describe the corresponding capture/input target. Positions,
/// identities, non-primary monitors, and non-primary streams remain unchanged.
fn update_primary_topology(
    monitors: &mut [MonitorInfo],
    streams: &mut [StreamInfo],
    primary_stream_id: u32,
    rdp_width: u32,
    rdp_height: u32,
    stream_width: u32,
    stream_height: u32,
) -> Result<(), InputError> {
    let primary_monitor = monitors
        .iter()
        .position(|monitor| monitor.is_primary)
        .ok_or_else(|| {
            InputError::InvalidMonitorConfig("no primary monitor configured".to_string())
        })?;
    let primary_stream = streams
        .iter()
        .position(|stream| stream.node_id == primary_stream_id)
        .ok_or(InputError::MonitorNotFound(primary_stream_id))?;

    monitors[primary_monitor].width = rdp_width;
    monitors[primary_monitor].height = rdp_height;
    monitors[primary_monitor].stream_width = stream_width;
    monitors[primary_monitor].stream_height = stream_height;
    streams[primary_stream].width = stream_width;
    streams[primary_stream].height = stream_height;

    Ok(())
}

/// Input handler that bridges IronRDP events to runtime input injection.
///
/// Receives keyboard and mouse events from RDP clients and injects them into
/// the managed Wayland compositor session.
pub struct InputChannelHandler {
    /// Session handle for input injection through the managed compositor backend
    session_handle: Arc<dyn crate::rdp::session::SessionHandle>,

    /// Keyboard event handler (pub for multiplexer access)
    pub keyboard_handler: Arc<Mutex<KeyboardHandler>>,

    /// Mouse event handler (pub for multiplexer access)
    pub mouse_handler: Arc<Mutex<MouseHandler>>,

    /// Coordinate transformer for multi-monitor support (pub for multiplexer access)
    pub coordinate_transformer: Arc<Mutex<CoordinateTransformer>>,

    /// Authoritative monitor topology used to rebuild the coordinate transformer.
    monitors: Arc<Mutex<Vec<MonitorInfo>>>,

    /// Primary stream node ID used for pointer/input injection routing.
    primary_stream_id: u32,

    /// Ordered, bounded input event queue.
    input_tx: mpsc::Sender<InputEvent>,

    /// A lost release or synchronization event makes the connection unsafe.
    terminal_overflow: Arc<AtomicBool>,
    overflow_reported: Arc<AtomicBool>,
    health_reporter: Arc<StdRwLock<Option<SessionStatusReporter>>>,

    /// Display update channel used only for Android RD Client pointer workaround PDUs.
    pointer_update_tx: Option<Arc<Mutex<mpsc::Sender<DisplayUpdate>>>>,

    /// Shared EGFX capability state used to gate Android-only pointer workaround PDUs.
    gfx_handler_state: Option<Arc<RwLock<Option<HandlerState>>>>,

    /// Whether the Android workaround cursor bitmap was already sent this connection.
    pointer_shape_sent: Arc<AtomicBool>,

    /// Bounded diagnostics proving the client delivered input without logging content.
    first_keyboard_event: Arc<AtomicBool>,
    first_mouse_event: Arc<AtomicBool>,
    first_mouse_button_event: Arc<AtomicBool>,
}

impl InputChannelHandler {
    #[expect(
        clippy::too_many_arguments,
        reason = "explicit protocol and pixel-plane parameters preserve existing call contracts"
    )]
    pub fn new(
        session_handle: Arc<dyn crate::rdp::session::SessionHandle>,
        monitors: Vec<MonitorInfo>,
        primary_stream_id: u32,
        legacy_input_tx: mpsc::UnboundedSender<InputEvent>,
        pointer_update_tx: Option<Arc<Mutex<mpsc::Sender<DisplayUpdate>>>>,
        gfx_handler_state: Option<Arc<RwLock<Option<HandlerState>>>>,
        legacy_input_rx: mpsc::UnboundedReceiver<InputEvent>,
        mut shutdown_rx: tokio::sync::broadcast::Receiver<()>,
        keyboard_layout: &str,
        cjk_paste_enabled: bool,
        clipboard_provider: Option<Arc<dyn ClipboardProvider>>,
    ) -> Result<Self, InputError> {
        let mut keyboard = KeyboardHandler::new();
        keyboard.set_layout(if keyboard_layout == "auto" {
            "us"
        } else {
            keyboard_layout
        });
        let keyboard_handler = Arc::new(Mutex::new(keyboard));
        let mouse_handler = Arc::new(Mutex::new(MouseHandler::new()));

        let coordinate_transformer =
            Arc::new(Mutex::new(CoordinateTransformer::new(monitors.clone())?));
        let monitors = Arc::new(Mutex::new(monitors));

        // Keep the existing constructor ABI while making the callback-facing
        // queue bounded. The legacy pair is never used for event transport.
        drop(legacy_input_tx);
        drop(legacy_input_rx);
        const INPUT_QUEUE_CAPACITY: usize = 4096;
        let (input_tx, mut input_rx) = mpsc::channel(INPUT_QUEUE_CAPACITY);
        let terminal_overflow = Arc::new(AtomicBool::new(false));
        let overflow_reported = Arc::new(AtomicBool::new(false));
        let health_reporter: Arc<StdRwLock<Option<SessionStatusReporter>>> =
            Arc::new(StdRwLock::new(None));
        let terminal_overflow_task = Arc::clone(&terminal_overflow);
        let overflow_reported_task = Arc::clone(&overflow_reported);
        let health_reporter_task = Arc::clone(&health_reporter);

        debug!(
            "Input handler using PipeWire stream node ID: {}",
            primary_stream_id
        );

        // Receive synchronous callbacks in FIFO order and inject them asynchronously.
        let session_handle_clone = Arc::clone(&session_handle);
        let keyboard_clone = Arc::clone(&keyboard_handler);
        let mouse_clone = Arc::clone(&mouse_handler);
        let coord_clone = Arc::clone(&coordinate_transformer);
        let cjk_enabled_task = cjk_paste_enabled;
        let clipboard_provider_task = clipboard_provider.clone();
        let pointer_update_tx_task = pointer_update_tx.clone();
        let gfx_handler_state_task = gfx_handler_state.clone();
        let pointer_shape_sent = Arc::new(AtomicBool::new(false));
        let pointer_shape_sent_task = Arc::clone(&pointer_shape_sent);

        tokio::spawn(async move {
            let mut cjk_buffer = CjkPasteBuffer::new();
            let consecutive_mouse_errors = AtomicU64::new(0);
            let consecutive_kbd_errors = AtomicU64::new(0);

            loop {
                tokio::select! {
                    event = input_rx.recv() => {
                        let Some(event) = event else { break };
                        if terminal_overflow_task.load(Ordering::Acquire) {
                            break;
                        }
                        let result = match event {
                            InputEvent::Keyboard(event) => {
                                Self::handle_keyboard_event_impl(
                                    &session_handle_clone,
                                    &keyboard_clone,
                                    event,
                                    &mut cjk_buffer,
                                    cjk_enabled_task,
                                    clipboard_provider_task.as_ref(),
                                ).await.map_err(|error| ("keyboard", error))
                            }
                            InputEvent::Mouse(event) => {
                                Self::handle_mouse_event_impl(
                                    &session_handle_clone,
                                    &mouse_clone,
                                    &coord_clone,
                                    event,
                                    primary_stream_id,
                                    pointer_update_tx_task.as_ref(),
                                    gfx_handler_state_task.as_ref(),
                                    &pointer_shape_sent_task,
                                ).await.map_err(|error| ("mouse", error))
                            }
                        };
                        match result {
                            Ok(()) => {
                                consecutive_mouse_errors.store(0, Ordering::Relaxed);
                                consecutive_kbd_errors.store(0, Ordering::Relaxed);
                            }
                            Err((kind, error)) => {
                                let counter = if kind == "mouse" {
                                    &consecutive_mouse_errors
                                } else {
                                    &consecutive_kbd_errors
                                };
                                let count = counter.fetch_add(1, Ordering::Relaxed) + 1;
                                if count == 1 || count.is_power_of_two() {
                                    warn!(kind, count, %error, "Portal input injection failed");
                                }
                                if injection_failures_are_terminal(count) {
                                    error!(
                                        kind,
                                        count,
                                        "Input injection failed permanently; failing connection"
                                    );
                                    terminal_overflow_task.store(true, Ordering::Release);
                                    report_input_failed_once(
                                        &terminal_overflow_task,
                                        &overflow_reported_task,
                                        &health_reporter_task,
                                        "input injection failed on every attempt past the permanent-failure threshold",
                                    );
                                    break;
                                }
                            }
                        }
                    }
                    _ = shutdown_rx.recv() => break,
                }
            }
            info!("Input event worker stopped");
        });
        info!("Ordered input event worker started");

        Ok(Self {
            session_handle,
            keyboard_handler,
            mouse_handler,
            coordinate_transformer,
            monitors,
            primary_stream_id,
            input_tx,
            terminal_overflow,
            overflow_reported,
            health_reporter,
            pointer_update_tx,
            gfx_handler_state,
            pointer_shape_sent,
            first_keyboard_event: Arc::new(AtomicBool::new(false)),
            first_mouse_event: Arc::new(AtomicBool::new(false)),
            first_mouse_button_event: Arc::new(AtomicBool::new(false)),
        })
    }

    fn enqueue_input(&self, event: InputEvent, label: &'static str) {
        if self.terminal_overflow.load(Ordering::Acquire) {
            trace!(label, "Ignoring input after terminal queue failure");
            return;
        }

        match self.input_tx.try_send(event) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(event)) => {
                match queue_pressure_action(&event) {
                    QueuePressureAction::Drop => {
                        trace!(label, "Dropping coalescible input under queue pressure");
                    }
                    QueuePressureAction::FailConnection => self.fail_input_connection(
                        "bounded input queue overflowed before a non-droppable release or synchronize event",
                    ),
                }
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.fail_input_connection("bounded input queue closed while the connection was active");
            }
        }
    }

    fn fail_input_connection(&self, reason: &'static str) {
        if !self.terminal_overflow.swap(true, Ordering::AcqRel) {
            error!(reason, "Input queue entered a terminal state");
        }
        self.report_terminal_overflow_if_needed(reason);
    }

    fn report_terminal_overflow_if_needed(&self, reason: &str) {
        report_input_failed_once(
            &self.terminal_overflow,
            &self.overflow_reported,
            &self.health_reporter,
            reason,
        );
    }

    /// Wire input queue failures to session supervision.
    pub fn set_health_reporter(&self, reporter: SessionStatusReporter) {
        if let Ok(mut current) = self.health_reporter.write() {
            *current = Some(reporter);
        } else {
            error!("Input health reporter lock is poisoned");
            return;
        }

        self.report_terminal_overflow_if_needed(
            "input event queue entered a terminal state before supervision was attached",
        );
    }

    /// Whether this connection lost a release/synchronize event or its queue closed.
    pub fn terminal_overflowed(&self) -> bool {
        self.terminal_overflow.load(Ordering::Acquire)
    }

    /// Update coordinate transformer when monitor configuration changes
    ///
    /// This should be called when the RDP client requests a different resolution
    /// or when monitor configuration changes.
    pub async fn update_monitors(&self, monitors: Vec<MonitorInfo>) -> Result<(), InputError> {
        let transformer = CoordinateTransformer::new(monitors.clone())?;
        *self.monitors.lock().await = monitors;
        *self.coordinate_transformer.lock().await = transformer;
        debug!("Updated monitor configuration");
        Ok(())
    }

    /// Update the single primary stream geometry used by direct-capture sessions.
    ///
    /// Portal-generic direct capture can report the real frame size only once
    /// frames start flowing. Keep both the input coordinate transformer and the
    /// session's stream metadata in sync with that live size, otherwise mouse
    /// coordinates are normalized against stale dimensions and land offset or
    /// off-screen.
    pub async fn update_primary_stream_geometry(
        &self,
        width: u32,
        height: u32,
    ) -> Result<(), InputError> {
        self.update_primary_stream_mapping(width, height, width, height)
            .await
    }

    /// Update direct-capture input mapping when the RDP desktop size differs
    /// from the captured stream size.
    ///
    /// `rdp_width`/`rdp_height` are the coordinate space sent by the client.
    /// `stream_width`/`stream_height` are the compositor/capture coordinate
    /// space that the Wayland virtual pointer expects. Keeping these separate
    /// is what makes client-side dynamic resize and pointer injection line up.
    pub async fn update_primary_stream_mapping(
        &self,
        rdp_width: u32,
        rdp_height: u32,
        stream_width: u32,
        stream_height: u32,
    ) -> Result<(), InputError> {
        let rdp_width = rdp_width.max(1);
        let rdp_height = rdp_height.max(1);
        let stream_width = stream_width.max(1);
        let stream_height = stream_height.max(1);
        let mut monitors = self.monitors.lock().await.clone();
        let mut streams = self.session_handle.streams();

        update_primary_topology(
            &mut monitors,
            &mut streams,
            self.primary_stream_id,
            rdp_width,
            rdp_height,
            stream_width,
            stream_height,
        )?;

        self.update_monitors(monitors).await?;
        self.session_handle.set_streams(streams);

        info!(
            "Updated direct input mapping: rdp {}x{} -> stream {}x{} for stream {}",
            rdp_width, rdp_height, stream_width, stream_height, self.primary_stream_id
        );
        Ok(())
    }

    /// Handle keyboard event implementation (static for batching task)
    async fn handle_keyboard_event_impl(
        session_handle: &Arc<dyn crate::rdp::session::SessionHandle>,
        keyboard_handler: &Arc<Mutex<KeyboardHandler>>,
        event: IronKeyboardEvent,
        cjk_buffer: &mut CjkPasteBuffer,
        cjk_paste_enabled: bool,
        clipboard_provider: Option<&Arc<dyn ClipboardProvider>>,
    ) -> Result<(), InputError> {
        let mut keyboard = keyboard_handler.lock().await;

        match event {
            IronKeyboardEvent::Pressed { code, extended } => {
                // Flush any buffered CJK text before a regular keycode event
                if !cjk_buffer.is_empty() {
                    drop(keyboard);
                    Self::flush_cjk_buffer(session_handle, cjk_buffer, clipboard_provider).await;
                    keyboard = keyboard_handler.lock().await;
                }

                let kbd_event = keyboard.handle_key_down(code as u16, extended, false)?;

                let keycode = match kbd_event {
                    crate::rdp::channels::input::KeyboardEvent::KeyDown { keycode, .. }
                    | crate::rdp::channels::input::KeyboardEvent::KeyRepeat { keycode, .. } => {
                        keycode
                    }
                    crate::rdp::channels::input::KeyboardEvent::Ignored { .. } => return Ok(()),
                    crate::rdp::channels::input::KeyboardEvent::KeyUp { keycode, .. } => {
                        warn!("handle_key_down returned KeyUp; using translated keycode");
                        keycode
                    }
                    #[expect(
                        unreachable_patterns,
                        reason = "defensive: future KeyboardEvent variants"
                    )]
                    _ => {
                        error!("handle_key_down returned an unexpected event type");
                        return Err(InputError::InvalidKeyEvent(
                            "Unexpected event type".to_string(),
                        ));
                    }
                };

                session_handle
                    .notify_keyboard_keycode(keycode as i32, true)
                    .await
                    .map_err(portal_err)?;
            }

            IronKeyboardEvent::Released { code, extended } => {
                let kbd_event = keyboard.handle_key_up(code as u16, extended, false)?;

                let keycode = match kbd_event {
                    crate::rdp::channels::input::KeyboardEvent::KeyUp { keycode, .. } => keycode,
                    crate::rdp::channels::input::KeyboardEvent::Ignored { .. } => return Ok(()),
                    _ => {
                        return Err(InputError::InvalidKeyEvent(
                            "Unexpected event type".to_string(),
                        ));
                    }
                };

                session_handle
                    .notify_keyboard_keycode(keycode as i32, false)
                    .await
                    .map_err(portal_err)?;
            }

            IronKeyboardEvent::UnicodePressed(unicode) => {
                if let Some((keycode, needs_shift)) = unicode_to_evdev(unicode) {
                    // KEY_LEFTSHIFT = 42
                    if needs_shift {
                        session_handle
                            .notify_keyboard_keycode(42, true)
                            .await
                            .map_err(portal_err)?;
                    }
                    session_handle
                        .notify_keyboard_keycode(keycode as i32, true)
                        .await
                        .map_err(portal_err)?;
                } else if cjk_paste_enabled {
                    // Some portal-backed environments cannot turn CJK Unicode keysyms into
                    // physical keycodes, so do not try keysym injection for non-ASCII
                    // Unicode here. Buffer the committed text and paste it immediately.
                    if (0xD800..=0xDBFF).contains(&unicode) {
                        cjk_buffer.pending_high_surrogate = Some(unicode);
                    } else if (0xDC00..=0xDFFF).contains(&unicode) {
                        if let Some(high) = cjk_buffer.pending_high_surrogate.take()
                            && cjk_buffer.push_surrogate_pair(high, unicode).is_some()
                        {
                            drop(keyboard);
                            Self::flush_cjk_buffer(session_handle, cjk_buffer, clipboard_provider)
                                .await;
                        }
                    } else if let Some(c) = char::from_u32(u32::from(unicode)) {
                        cjk_buffer.push_char(c);
                        drop(keyboard);
                        Self::flush_cjk_buffer(session_handle, cjk_buffer, clipboard_provider)
                            .await;
                    }
                } else if let Some(keysym) = unicode_to_keysym(unicode) {
                    session_handle
                        .notify_keyboard_keysym(keysym, true)
                        .await
                        .map_err(portal_err)?;
                }
            }

            IronKeyboardEvent::UnicodeReleased(unicode) => {
                // Skip release events for chars that were buffered into the CJK buffer
                // (they have no corresponding keycode to release)
                if unicode_to_evdev(unicode).is_some() {
                    if let Some((keycode, needs_shift)) = unicode_to_evdev(unicode) {
                        session_handle
                            .notify_keyboard_keycode(keycode as i32, false)
                            .await
                            .map_err(portal_err)?;
                        if needs_shift {
                            session_handle
                                .notify_keyboard_keycode(42, false)
                                .await
                                .map_err(portal_err)?;
                        }
                    }
                } else if let Some(keysym) = unicode_to_keysym(unicode) {
                    session_handle
                        .notify_keyboard_keysym(keysym, false)
                        .await
                        .map_err(portal_err)?;
                } else {
                    // Buffered CJK character — no release event needed
                }
            }

            IronKeyboardEvent::Synchronize(flags) => {
                use ironrdp_pdu::input::fast_path::SynchronizeFlags;
                let toggles = keyboard.synchronize_locks(
                    flags.contains(SynchronizeFlags::CAPS_LOCK),
                    flags.contains(SynchronizeFlags::NUM_LOCK),
                    flags.contains(SynchronizeFlags::SCROLL_LOCK),
                );
                drop(keyboard);
                for keycode in toggles {
                    session_handle
                        .notify_keyboard_keycode(keycode as i32, true)
                        .await
                        .map_err(portal_err)?;
                    session_handle
                        .notify_keyboard_keycode(keycode as i32, false)
                        .await
                        .map_err(portal_err)?;
                }
            }
        }

        Ok(())
    }

    /// Flush buffered CJK text via clipboard write + synthetic Ctrl+V.
    ///
    /// Saves and restores the original clipboard content around the paste.
    async fn flush_cjk_buffer(
        session_handle: &Arc<dyn crate::rdp::session::SessionHandle>,
        cjk_buffer: &mut CjkPasteBuffer,
        clipboard_provider: Option<&Arc<dyn ClipboardProvider>>,
    ) {
        let Some(text) = cjk_buffer.take_text() else {
            return;
        };
        let char_count = text.chars().count();

        if let Some(provider) = clipboard_provider {
            // Save current clipboard content so we can restore it after the paste.
            let saved_clipboard = provider.read_data("text/plain").await.ok();

            if let Err(e) = provider.write_text(&text).await {
                warn!("CJK paste fallback: clipboard write failed: {e}");
            }

            // Allow clipboard to propagate before sending Ctrl+V
            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

            // Ctrl+V: KEY_LEFTCTRL=29, KEY_V=47
            let send_key = |keycode: i32, pressed: bool| {
                let sh = Arc::clone(session_handle);
                async move {
                    if let Err(e) = sh.notify_keyboard_keycode(keycode, pressed).await {
                        warn!("CJK paste fallback: keycode inject failed: {e}");
                    }
                }
            };
            send_key(29, true).await;
            send_key(47, true).await;
            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
            send_key(47, false).await;
            send_key(29, false).await;

            // Wait for Ctrl+V to be processed before restoring clipboard
            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

            // Restore original clipboard content
            if let Some(saved) = saved_clipboard
                && let Ok(saved_text) = String::from_utf8(saved)
            {
                if let Err(e) = provider.write_text(&saved_text).await {
                    warn!("CJK paste: clipboard restore failed: {e}");
                } else {
                    debug!("CJK paste: clipboard content restored");
                }
            }
        }

        info!("CJK paste fallback: flushed {char_count} chars via clipboard");
    }

    fn create_android_arrow_cursor() -> RGBAPointer {
        const W: u16 = 32;
        const H: u16 = 32;
        const HOT_X: u16 = 4;
        const HOT_Y: u16 = 27;
        let mut data = vec![0u8; usize::from(W) * usize::from(H) * 4];

        // Draw a simple Breeze-like left pointer, stored vertically flipped for
        // Microsoft RD Client on Android. Windows clients must not receive this
        // bitmap, because they render it with normal orientation.
        for y in 0..24u16 {
            for x in 0..=y.min(14) {
                let border = x == 0 || x == y.min(14) || y == 23;
                let src_y = H - 1 - y;
                let idx = (usize::from(src_y) * usize::from(W) + usize::from(x)) * 4;
                let (r, g, b, a) = if border {
                    (0, 0, 0, 255)
                } else {
                    (255, 255, 255, 255)
                };
                data[idx] = r;
                data[idx + 1] = g;
                data[idx + 2] = b;
                data[idx + 3] = a;
            }
        }

        RGBAPointer {
            cache_index: 0,
            width: W,
            height: H,
            hot_x: HOT_X,
            hot_y: HOT_Y,
            data,
        }
    }

    async fn needs_android_pointer_updates(
        gfx_handler_state: Option<&Arc<RwLock<Option<HandlerState>>>>,
    ) -> bool {
        let Some(state) = gfx_handler_state else {
            return false;
        };
        state
            .read()
            .await
            .as_ref()
            .is_some_and(|s| s.needs_android_pointer_updates)
    }

    async fn send_android_pointer_shape_once(
        pointer_update_tx: Option<&Arc<Mutex<mpsc::Sender<DisplayUpdate>>>>,
        gfx_handler_state: Option<&Arc<RwLock<Option<HandlerState>>>>,
        pointer_shape_sent: &Arc<AtomicBool>,
    ) {
        if !Self::needs_android_pointer_updates(gfx_handler_state).await {
            return;
        }
        if pointer_shape_sent
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }

        let Some(update_tx) = pointer_update_tx else {
            return;
        };
        let sender = update_tx.lock().await;
        if let Err(err) = sender.try_send(DisplayUpdate::RGBAPointer(
            Self::create_android_arrow_cursor(),
        )) {
            trace!("Dropping Android pointer shape update: {err}");
            pointer_shape_sent.store(false, Ordering::Release);
        } else {
            debug!("Sent Android RD Client pointer shape workaround");
        }
    }

    async fn send_android_pointer_position_update(
        pointer_update_tx: Option<&Arc<Mutex<mpsc::Sender<DisplayUpdate>>>>,
        gfx_handler_state: Option<&Arc<RwLock<Option<HandlerState>>>>,
        x: u16,
        y: u16,
    ) {
        if !Self::needs_android_pointer_updates(gfx_handler_state).await {
            return;
        }
        let Some(update_tx) = pointer_update_tx else {
            return;
        };

        let update = DisplayUpdate::PointerPosition(PointerPositionAttribute { x, y });
        let sender = update_tx.lock().await;
        if let Err(err) = sender.try_send(update) {
            trace!("Dropping Android pointer position update: {err}");
        }
    }

    /// Handle mouse event with full error handling and logging
    /// Handle mouse event implementation (static for batching task)
    #[expect(
        clippy::too_many_arguments,
        reason = "explicit protocol and pixel-plane parameters preserve existing call contracts"
    )]
    async fn handle_mouse_event_impl(
        session_handle: &Arc<dyn crate::rdp::session::SessionHandle>,
        mouse_handler: &Arc<Mutex<MouseHandler>>,
        coordinate_transformer: &Arc<Mutex<CoordinateTransformer>>,
        event: IronMouseEvent,
        stream_id: u32,
        pointer_update_tx: Option<&Arc<Mutex<mpsc::Sender<DisplayUpdate>>>>,
        gfx_handler_state: Option<&Arc<RwLock<Option<HandlerState>>>>,
        pointer_shape_sent: &Arc<AtomicBool>,
    ) -> Result<(), InputError> {
        let mut mouse = mouse_handler.lock().await;
        let mut transformer = coordinate_transformer.lock().await;

        match event {
            IronMouseEvent::Move { x, y } => {
                let mouse_event =
                    mouse.handle_absolute_move(x as u32, y as u32, &mut transformer)?;

                let (stream_x, stream_y) = match mouse_event {
                    crate::rdp::channels::input::MouseEvent::Move { x, y, .. } => (x, y),
                    _ => {
                        return Err(InputError::InvalidMouseEvent(
                            "Unexpected event type".to_string(),
                        ));
                    }
                };

                Self::send_android_pointer_shape_once(
                    pointer_update_tx,
                    gfx_handler_state,
                    pointer_shape_sent,
                )
                .await;
                Self::send_android_pointer_position_update(
                    pointer_update_tx,
                    gfx_handler_state,
                    x,
                    y,
                )
                .await;

                session_handle
                    .notify_pointer_motion_absolute(stream_id, stream_x, stream_y)
                    .await
                    .map_err(portal_err)?;
            }

            IronMouseEvent::RelMove { x, y } => {
                let mouse_event = mouse.handle_relative_move(x, y, &mut transformer)?;

                let (stream_x, stream_y) = match mouse_event {
                    crate::rdp::channels::input::MouseEvent::Move { x, y, .. } => (x, y),
                    _ => {
                        return Err(InputError::InvalidMouseEvent(
                            "Unexpected event type".to_string(),
                        ));
                    }
                };

                // We converted relative to absolute already
                let pointer_x = stream_x.clamp(0.0, f64::from(u16::MAX)).round() as u16;
                let pointer_y = stream_y.clamp(0.0, f64::from(u16::MAX)).round() as u16;
                Self::send_android_pointer_shape_once(
                    pointer_update_tx,
                    gfx_handler_state,
                    pointer_shape_sent,
                )
                .await;
                Self::send_android_pointer_position_update(
                    pointer_update_tx,
                    gfx_handler_state,
                    pointer_x,
                    pointer_y,
                )
                .await;

                session_handle
                    .notify_pointer_motion_absolute(stream_id, stream_x, stream_y)
                    .await
                    .map_err(portal_err)?;
            }

            IronMouseEvent::Button {
                x,
                y,
                button,
                pressed,
            } => {
                let movement =
                    mouse.handle_absolute_move(u32::from(x), u32::from(y), &mut transformer)?;
                if let MouseEvent::Move {
                    x: stream_x,
                    y: stream_y,
                    ..
                } = movement
                {
                    session_handle
                        .notify_pointer_motion_absolute(stream_id, stream_x, stream_y)
                        .await
                        .map_err(portal_err)?;
                }
                let button = match button {
                    ironrdp_server::MouseButton::Left => MouseButton::Left,
                    ironrdp_server::MouseButton::Right => MouseButton::Right,
                    ironrdp_server::MouseButton::Middle => MouseButton::Middle,
                    ironrdp_server::MouseButton::X1 => MouseButton::Extra1,
                    ironrdp_server::MouseButton::X2 => MouseButton::Extra2,
                    _ => return Ok(()),
                };
                if !mouse.accept_button_transition(button, pressed) {
                    return Ok(());
                }
                if pressed {
                    mouse.handle_button_down(button)?;
                } else {
                    mouse.handle_button_up(button)?;
                }
                session_handle
                    .notify_pointer_button(button.to_linux_button() as i32, pressed)
                    .await
                    .map_err(portal_err)?;
                Self::send_android_pointer_position_update(
                    pointer_update_tx,
                    gfx_handler_state,
                    x,
                    y,
                )
                .await;
                if pressed {
                    Self::send_android_pointer_shape_once(
                        pointer_update_tx,
                        gfx_handler_state,
                        pointer_shape_sent,
                    )
                    .await;
                }
            }
            IronMouseEvent::ButtonRel {
                x,
                y,
                button,
                pressed,
            } => {
                let movement = mouse.handle_relative_move(x, y, &mut transformer)?;
                if let MouseEvent::Move {
                    x: stream_x,
                    y: stream_y,
                    ..
                } = movement
                {
                    session_handle
                        .notify_pointer_motion_absolute(stream_id, stream_x, stream_y)
                        .await
                        .map_err(portal_err)?;
                    Self::send_android_pointer_position_update(
                        pointer_update_tx,
                        gfx_handler_state,
                        stream_x.clamp(0.0, f64::from(u16::MAX)).round() as u16,
                        stream_y.clamp(0.0, f64::from(u16::MAX)).round() as u16,
                    )
                    .await;
                }
                let button = match button {
                    ironrdp_server::MouseButton::Left => MouseButton::Left,
                    ironrdp_server::MouseButton::Right => MouseButton::Right,
                    ironrdp_server::MouseButton::Middle => MouseButton::Middle,
                    ironrdp_server::MouseButton::X1 => MouseButton::Extra1,
                    ironrdp_server::MouseButton::X2 => MouseButton::Extra2,
                    _ => return Ok(()),
                };
                if !mouse.accept_button_transition(button, pressed) {
                    return Ok(());
                }
                if pressed {
                    mouse.handle_button_down(button)?;
                } else {
                    mouse.handle_button_up(button)?;
                }
                session_handle
                    .notify_pointer_button(button.to_linux_button() as i32, pressed)
                    .await
                    .map_err(portal_err)?;
                if pressed {
                    Self::send_android_pointer_shape_once(
                        pointer_update_tx,
                        gfx_handler_state,
                        pointer_shape_sent,
                    )
                    .await;
                }
            }
            IronMouseEvent::VerticalScroll { value } => {
                let event = mouse.handle_scroll(0, value as i32)?;
                if let crate::rdp::channels::input::MouseEvent::Scroll {
                    delta_x, delta_y, ..
                } = event
                    && (delta_x != 0 || delta_y != 0)
                {
                    session_handle
                        .notify_pointer_axis(f64::from(delta_x) * 15.0, f64::from(delta_y) * 15.0)
                        .await
                        .map_err(portal_err)?;
                    session_handle
                        .notify_pointer_axis(0.0, 0.0)
                        .await
                        .map_err(portal_err)?;
                }
            }

            IronMouseEvent::Scroll { x, y } => {
                let event = mouse.handle_scroll(x, y)?;
                if let crate::rdp::channels::input::MouseEvent::Scroll {
                    delta_x, delta_y, ..
                } = event
                    && (delta_x != 0 || delta_y != 0)
                {
                    session_handle
                        .notify_pointer_axis(f64::from(delta_x) * 15.0, f64::from(delta_y) * 15.0)
                        .await
                        .map_err(portal_err)?;
                    session_handle
                        .notify_pointer_axis(0.0, 0.0)
                        .await
                        .map_err(portal_err)?;
                }
            }
            _ => {}
        }

        Ok(())
    }
}

impl RdpServerInputHandler for InputChannelHandler {
    fn keyboard(&mut self, event: IronKeyboardEvent) {
        if !self.first_keyboard_event.swap(true, Ordering::Relaxed) {
            info!("First RDP keyboard event received");
        }
        trace!("⌨️  Input multiplexer: routing keyboard to queue");
        self.enqueue_input(InputEvent::Keyboard(event), "keyboard");
    }

    fn mouse(&mut self, event: IronMouseEvent) {
        if !self.first_mouse_event.swap(true, Ordering::Relaxed) {
            info!("First RDP mouse event received");
        }
        if matches!(
            event,
            IronMouseEvent::Button { pressed: true, .. }
                | IronMouseEvent::ButtonRel { pressed: true, .. }
        ) && !self.first_mouse_button_event.swap(true, Ordering::Relaxed)
        {
            info!("First RDP pointer button event received");
        }
        trace!("🖱️  Input multiplexer: routing mouse to queue");
        self.enqueue_input(InputEvent::Mouse(event), "mouse");
    }
}

/// RdpServer needs ownership but we want to share state
impl Clone for InputChannelHandler {
    fn clone(&self) -> Self {
        Self {
            session_handle: Arc::clone(&self.session_handle),
            keyboard_handler: Arc::clone(&self.keyboard_handler),
            mouse_handler: Arc::clone(&self.mouse_handler),
            coordinate_transformer: Arc::clone(&self.coordinate_transformer),
            monitors: Arc::clone(&self.monitors),
            primary_stream_id: self.primary_stream_id,
            input_tx: self.input_tx.clone(),
            terminal_overflow: Arc::clone(&self.terminal_overflow),
            overflow_reported: Arc::clone(&self.overflow_reported),
            health_reporter: Arc::clone(&self.health_reporter),
            pointer_update_tx: self.pointer_update_tx.clone(),
            gfx_handler_state: self.gfx_handler_state.clone(),
            pointer_shape_sent: Arc::clone(&self.pointer_shape_sent),
            first_keyboard_event: Arc::clone(&self.first_keyboard_event),
            first_mouse_event: Arc::clone(&self.first_mouse_event),
            first_mouse_button_event: Arc::clone(&self.first_mouse_button_event),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_to_keysym_maps_bmp_cjk_to_xkb_unicode_keysym() {
        assert_eq!(unicode_to_keysym('中' as u16), Some(0x0100_4E2D));
        assert_eq!(unicode_to_keysym('文' as u16), Some(0x0100_6587));
    }

    #[test]
    fn unicode_to_keysym_keeps_latin1_keysyms_direct() {
        assert_eq!(unicode_to_keysym('é' as u16), Some(0x00E9));
    }

    #[test]
    fn unicode_to_keysym_rejects_surrogate_code_units() {
        assert_eq!(unicode_to_keysym(0xD83D), None);
        assert_eq!(unicode_to_keysym(0xDE00), None);
    }

    #[test]
    fn test_input_handler_clone() {
        // Verify clone compiles and works
        // Full tests require portal mocking
    }

    #[test]
    fn mouse_release_is_critical_at_any_position() {
        for button in [
            ironrdp_server::MouseButton::Left,
            ironrdp_server::MouseButton::Right,
            ironrdp_server::MouseButton::Middle,
            ironrdp_server::MouseButton::X1,
            ironrdp_server::MouseButton::X2,
        ] {
            assert_eq!(
                queue_pressure_action(&InputEvent::Mouse(IronMouseEvent::Button {
                    x: 731,
                    y: 419,
                    button,
                    pressed: false
                })),
                QueuePressureAction::FailConnection
            );
            assert_eq!(
                queue_pressure_action(&InputEvent::Mouse(IronMouseEvent::ButtonRel {
                    x: -3,
                    y: 7,
                    button,
                    pressed: false
                })),
                QueuePressureAction::FailConnection
            );
        }
    }

    #[test]
    fn queue_pressure_fails_for_release_and_synchronize_events() {
        use ironrdp_pdu::input::fast_path::SynchronizeFlags;

        assert_eq!(
            queue_pressure_action(&InputEvent::Keyboard(IronKeyboardEvent::Released {
                code: 0x1e,
                extended: false,
            })),
            QueuePressureAction::FailConnection
        );
        assert_eq!(
            queue_pressure_action(&InputEvent::Keyboard(IronKeyboardEvent::Synchronize(
                SynchronizeFlags::empty(),
            ))),
            QueuePressureAction::FailConnection
        );
        assert_eq!(
            queue_pressure_action(&InputEvent::Mouse(IronMouseEvent::Button {
                x: 0,
                y: 0,
                button: ironrdp_server::MouseButton::Left,
                pressed: false
            })),
            QueuePressureAction::FailConnection
        );
        assert_eq!(
            queue_pressure_action(&InputEvent::Mouse(IronMouseEvent::Move { x: 1, y: 1 })),
            QueuePressureAction::Drop
        );
        assert_eq!(
            queue_pressure_action(&InputEvent::Keyboard(IronKeyboardEvent::Pressed {
                code: 0x1e,
                extended: false,
            })),
            QueuePressureAction::Drop
        );
    }

    #[test]
    fn primary_mapping_update_preserves_monitor_and_stream_topology() {
        let mut monitors = vec![
            MonitorInfo {
                id: 7,
                name: "primary".to_string(),
                x: 100,
                y: 200,
                width: 1920,
                height: 1080,
                dpi: 110.0,
                scale_factor: 1.25,
                stream_x: 10,
                stream_y: 20,
                stream_width: 1920,
                stream_height: 1080,
                is_primary: true,
            },
            MonitorInfo {
                id: 8,
                name: "secondary".to_string(),
                x: 2020,
                y: 200,
                width: 1280,
                height: 1024,
                dpi: 96.0,
                scale_factor: 1.0,
                stream_x: 1930,
                stream_y: 20,
                stream_width: 1280,
                stream_height: 1024,
                is_primary: false,
            },
        ];
        let mut streams = vec![
            StreamInfo::new(71, 1920, 1080, 100, 200),
            StreamInfo::new(72, 1280, 1024, 2020, 200),
        ];

        assert!(
            update_primary_topology(&mut monitors, &mut streams, 71, 1600, 900, 2560, 1440).is_ok()
        );

        assert_eq!(monitors.len(), 2);
        assert_eq!(monitors[0].id, 7);
        assert_eq!(monitors[0].name, "primary");
        assert_eq!((monitors[0].x, monitors[0].y), (100, 200));
        assert_eq!((monitors[0].width, monitors[0].height), (1600, 900));
        assert_eq!((monitors[0].stream_x, monitors[0].stream_y), (10, 20));
        assert_eq!(
            (monitors[0].stream_width, monitors[0].stream_height),
            (2560, 1440)
        );
        assert_eq!(monitors[0].dpi, 110.0);
        assert_eq!(monitors[0].scale_factor, 1.25);
        assert!(monitors[0].is_primary);

        assert_eq!(monitors[1].id, 8);
        assert_eq!(monitors[1].name, "secondary");
        assert_eq!((monitors[1].x, monitors[1].y), (2020, 200));
        assert_eq!((monitors[1].width, monitors[1].height), (1280, 1024));
        assert_eq!((monitors[1].stream_x, monitors[1].stream_y), (1930, 20));
        assert_eq!(
            (monitors[1].stream_width, monitors[1].stream_height),
            (1280, 1024)
        );
        assert!(!monitors[1].is_primary);

        assert_eq!(streams.len(), 2);
        assert_eq!(streams[0].node_id, 71);
        assert_eq!((streams[0].width, streams[0].height), (2560, 1440));
        assert_eq!((streams[0].position_x, streams[0].position_y), (100, 200));
        assert_eq!(streams[1].node_id, 72);
        assert_eq!((streams[1].width, streams[1].height), (1280, 1024));
        assert_eq!((streams[1].position_x, streams[1].position_y), (2020, 200));
    }

    #[test]
    fn test_cjk_buffer_basic() {
        let mut buf = CjkPasteBuffer::new();
        buf.push_char('中');
        buf.push_char('文');
        buf.push_char('字');
        assert!(!buf.is_empty());
        assert_eq!(buf.take_text(), Some("中文字".to_string()));
        assert!(buf.is_empty());
        assert_eq!(buf.take_text(), None);
    }

    #[test]
    fn test_surrogate_pair() {
        let mut buf = CjkPasteBuffer::new();
        // U+1F600 GRINNING FACE: high=0xD83D, low=0xDE00
        let result = buf.push_surrogate_pair(0xD83D, 0xDE00);
        assert_eq!(result, Some('😀'));
        assert_eq!(buf.take_text(), Some("😀".to_string()));
    }

    #[test]
    fn test_buffer_empty_returns_none() {
        let mut buf = CjkPasteBuffer::new();
        assert!(buf.is_empty());
        assert_eq!(buf.take_text(), None);
    }

    #[test]
    fn injection_failure_threshold_is_terminal_only_at_limit() {
        // Transient runs below the threshold keep retrying.
        assert!(!injection_failures_are_terminal(0));
        assert!(!injection_failures_are_terminal(1));
        assert!(!injection_failures_are_terminal(
            INPUT_FAILURE_THRESHOLD - 1
        ));
        // At and beyond the threshold the failure is permanent.
        assert!(injection_failures_are_terminal(INPUT_FAILURE_THRESHOLD));
        assert!(injection_failures_are_terminal(
            INPUT_FAILURE_THRESHOLD + 100
        ));
    }

    #[test]
    fn report_input_failed_once_emits_permanent_failure_a_single_time() {
        use crate::rdp::session::supervision::SessionSupervisor;

        let (shutdown_tx, _) = tokio::sync::broadcast::channel(1);
        let (_, reporter, _) = SessionSupervisor::new(shutdown_tx.subscribe());

        let terminal_overflow = AtomicBool::new(false);
        let overflow_reported = AtomicBool::new(false);
        let health_reporter = StdRwLock::new(Some(reporter));

        // Not terminal yet: nothing is reported and the slot stays open.
        report_input_failed_once(
            &terminal_overflow,
            &overflow_reported,
            &health_reporter,
            "still healthy",
        );
        assert!(!overflow_reported.load(Ordering::Acquire));

        // Once terminal, the first call reports and the second is suppressed.
        terminal_overflow.store(true, Ordering::Release);
        report_input_failed_once(
            &terminal_overflow,
            &overflow_reported,
            &health_reporter,
            "permanent",
        );
        assert!(overflow_reported.load(Ordering::Acquire));
        report_input_failed_once(
            &terminal_overflow,
            &overflow_reported,
            &health_reporter,
            "permanent again",
        );
        assert!(overflow_reported.load(Ordering::Acquire));
    }
}
