//! RDP listener, connection binding, and handler ownership.

#![expect(
    unsafe_code,
    reason = "poll(2) checks a systemd socket-activation descriptor without taking ownership"
)]

mod display_handler;
pub(crate) mod event_multiplexer;
mod input_handler;
pub mod production;

pub use crate::rdp::channels::graphics::egfx::channel::{
    EgfxChannelFactory, EgfxChannelSender, EgfxCodecPolicy, HandlerState, NegotiatedEgfxMode,
    SharedHandlerState,
};
pub use display_handler::DisplayChannelHandler;
pub use input_handler::InputChannelHandler;

/// Return true when systemd supplied a listener but no connection is queued.
///
/// A socket unit may start WRDP for reasons other than an incoming connection.
/// Checking fd 3 before session creation avoids starting a compositor merely
/// because the service was activated.
pub fn systemd_socket_activation_without_pending_connection() -> bool {
    const SD_LISTEN_FDS_START: i32 = 3;

    let listen_fds = match std::env::var("LISTEN_FDS") {
        Ok(value) => value.parse::<i32>().unwrap_or(0),
        Err(_) => return false,
    };
    if listen_fds <= 0 {
        return false;
    }
    if let Ok(listen_pid) = std::env::var("LISTEN_PID")
        && let Ok(listen_pid) = listen_pid.parse::<u32>()
        && listen_pid != std::process::id()
    {
        return false;
    }

    let mut descriptor = libc::pollfd {
        fd: SD_LISTEN_FDS_START,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: `descriptor` points to one initialized pollfd and timeout zero
    // makes this a non-blocking readiness check. poll does not own fd 3.
    unsafe { libc::poll(&raw mut descriptor, 1, 0) == 0 }
}
