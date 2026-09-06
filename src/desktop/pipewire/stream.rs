//! PipeWire stream types.
//!
//! Defines stream configuration/state/metrics structures used by runtime stream
//! orchestration. The thread-owned stream implementation lives in `pw_thread.rs`.

use libspa::param::video::VideoFormat;
use pipewire::spa::utils::Fraction;
use pipewire::stream::StreamState;

use std::sync::Mutex;

use crate::desktop::pipewire::format::PixelFormat;

/// Stream configuration.
#[derive(Debug, Clone)]
pub struct StreamConfig {
    /// Stream name
    pub name: String,

    /// Target width
    pub width: u32,

    /// Target height
    pub height: u32,

    /// Target framerate
    pub framerate: u32,

    /// Use DMA-BUF if available
    pub use_dmabuf: bool,

    /// Number of buffers
    pub buffer_count: u32,

    /// Preferred format
    pub preferred_format: Option<PixelFormat>,
}

impl StreamConfig {
    /// Create default configuration
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            width: 1920,
            height: 1080,
            framerate: 30,
            use_dmabuf: true,
            buffer_count: 3,
            preferred_format: Some(PixelFormat::BGRA),
        }
    }

    /// Set resolution
    pub fn with_resolution(mut self, width: u32, height: u32) -> Self {
        self.width = width;
        self.height = height;
        self
    }

    /// Set framerate
    pub fn with_framerate(mut self, fps: u32) -> Self {
        self.framerate = fps;
        self
    }

    /// Set DMA-BUF preference
    pub fn with_dmabuf(mut self, use_dmabuf: bool) -> Self {
        self.use_dmabuf = use_dmabuf;
        self
    }

    /// Set buffer count
    pub fn with_buffer_count(mut self, count: u32) -> Self {
        self.buffer_count = count;
        self
    }
}

/// PipeWire stream state
///
/// Faithfully mirrors [`pipewire::stream::StreamState`] so consumers get
/// full state information for health monitoring and diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PwStreamState {
    /// Stream is not connected to a PipeWire node
    Unconnected,
    /// Stream is connecting to a PipeWire node
    Connecting,
    /// Stream is connected but not actively streaming
    Paused,
    /// Stream is actively delivering frames
    Streaming,
    /// Stream encountered an error (carries the error message)
    Error(String),
}

impl From<StreamState> for PwStreamState {
    fn from(state: StreamState) -> Self {
        match state {
            StreamState::Unconnected => Self::Unconnected,
            StreamState::Connecting => Self::Connecting,
            StreamState::Paused => Self::Paused,
            StreamState::Streaming => Self::Streaming,
            StreamState::Error(msg) => Self::Error(msg),
        }
    }
}

/// A stream state change event
///
/// Emitted by [`PipeWireThreadManager`](crate::desktop::pipewire::pw_thread::PipeWireThreadManager)
/// when a stream's state changes. Consumers can drain these events to
/// update health monitoring, UI, or take corrective action.
#[derive(Debug, Clone)]
pub struct StreamStateEvent {
    /// Stream ID that changed state
    pub stream_id: u32,
    /// New state of the stream
    pub state: PwStreamState,
}

/// Format negotiation result.
#[derive(Debug, Clone)]
pub struct NegotiatedFormat {
    /// Video format
    pub format: VideoFormat,

    /// Width
    pub width: u32,

    /// Height
    pub height: u32,

    /// Row stride
    pub stride: u32,

    /// Framerate
    pub framerate: Fraction,
}

/// Minimal stream handle used by connection and coordinator modules.
/// The real PipeWire stream implementation is in `pw_thread.rs`.
pub struct PipeWireStream {
    id: u32,
    state: Mutex<PwStreamState>,
    frame_callback: Mutex<Option<crate::desktop::pipewire::frame::FrameCallback>>,
}

impl PipeWireStream {
    /// Create a new stream handle
    pub fn new(id: u32, _config: StreamConfig) -> Self {
        Self {
            id,
            state: Mutex::new(PwStreamState::Unconnected),
            frame_callback: Mutex::new(None),
        }
    }

    /// Get stream ID
    pub fn id(&self) -> u32 {
        self.id
    }

    /// Get current state
    pub fn state(&self) -> PwStreamState {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Set frame callback
    pub fn set_frame_callback(&mut self, callback: crate::desktop::pipewire::frame::FrameCallback) {
        *self
            .frame_callback
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(callback);
    }

    /// Stop the stream
    pub async fn stop(&mut self) -> crate::desktop::pipewire::error::Result<()> {
        *self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = PwStreamState::Unconnected;
        Ok(())
    }
}

/// Stream timing snapshot from PipeWire's graph cycle.
///
/// Wraps the data from `pw_stream_get_time_n()`. All timing values are
/// relative to the PipeWire graph clock and updated once per graph cycle
/// (typically every quantum, e.g. every ~21ms at 48kHz/1024).
///
/// For video capture streams, `ticks` increases monotonically and `delay`
/// reflects the total pipeline latency from the capture source through
/// any intermediate filters.
#[derive(Debug, Clone, Default)]
pub struct StreamTime {
    /// Graph clock timestamp in nanoseconds from `pw_stream_get_time_n`.
    pub now_nsec: i64,

    /// Tick rate as a fraction (numerator / denominator).
    /// For video streams this is typically 1/framerate (e.g. 1/60).
    /// For audio streams it's 1/samplerate.
    pub rate_num: u32,
    pub rate_denom: u32,

    /// Monotonically increasing stream position in ticks.
    /// Gaps in this value between consecutive reads indicate dropped frames.
    pub ticks: u64,

    /// Pipeline latency from capture source through all filters, in ticks.
    /// Multiply by rate (rate_num / rate_denom) to get seconds.
    /// Does not include queued buffer latency.
    pub delay: i64,

    /// Total bytes currently queued in the stream (sum of all queued buffer sizes).
    pub queued_bytes: u64,

    /// Extra frames buffered in the resampler (audio streams).
    pub buffered: u64,

    /// Number of buffers currently queued (handed to PipeWire, not yet returned).
    pub queued_buffers: u32,

    /// Number of buffers available for dequeue.
    /// Low values indicate the producer is keeping up; zero means starvation.
    pub avail_buffers: u32,
}

impl StreamTime {
    /// Pipeline delay in nanoseconds.
    /// Returns 0 if rate_denom is zero (uninitialized stream).
    pub fn delay_nsec(&self) -> i64 {
        if self.rate_denom == 0 {
            return 0;
        }
        // delay is in ticks; convert via rate fraction to seconds, then to ns
        self.delay * self.rate_num as i64 * 1_000_000_000 / self.rate_denom as i64
    }

    /// Buffer pressure ratio (0.0 = no pressure, 1.0 = all buffers queued, none available).
    /// Returns 0.0 if no buffers exist.
    pub fn buffer_pressure(&self) -> f32 {
        let total = self.queued_buffers + self.avail_buffers;
        if total == 0 {
            return 0.0;
        }
        self.queued_buffers as f32 / total as f32
    }
}

/// Query the current stream time via `pw_stream_get_time_n()`.
///
/// RT-safe. Can be called from the process callback without blocking.
/// Returns `None` if the call fails (stream not connected or invalid pointer).
///
/// # Safety
///
/// The raw stream pointer must be valid and the stream must not have been destroyed.
/// This is guaranteed when called from within a stream callback or while the
/// PipeWire main loop is locked.
pub(crate) unsafe fn get_stream_time(
    raw_stream: *mut pipewire::sys::pw_stream,
) -> Option<StreamTime> {
    let mut time = std::mem::MaybeUninit::<pipewire::sys::pw_time>::uninit();
    // SAFETY: raw_stream is valid (caller guarantee), pw_time is POD with no
    // invariants, and pw_stream_get_time_n is documented RT-safe.
    let ret = unsafe {
        pipewire::sys::pw_stream_get_time_n(
            raw_stream,
            time.as_mut_ptr(),
            std::mem::size_of::<pipewire::sys::pw_time>(),
        )
    };
    if ret < 0 {
        return None;
    }
    // SAFETY: pw_stream_get_time_n returned success (>= 0), so time is initialized.
    let t = unsafe { time.assume_init() };
    Some(StreamTime {
        now_nsec: t.now,
        rate_num: t.rate.num,
        rate_denom: t.rate.denom,
        ticks: t.ticks,
        delay: t.delay,
        queued_bytes: t.queued,
        buffered: t.buffered,
        queued_buffers: t.queued_buffers,
        avail_buffers: t.avail_buffers,
    })
}

/// Stream metrics.
#[derive(Debug, Clone, Default)]
pub struct StreamMetrics {
    /// Frames processed
    pub frames_processed: u64,

    /// Bytes processed
    pub bytes_processed: u64,

    /// Errors encountered
    pub error_count: u64,

    /// Buffer underruns
    pub underruns: u64,

    /// Average frame latency (milliseconds)
    pub avg_latency_ms: f64,

    /// Current FPS
    pub current_fps: f32,

    /// Most recent stream timing snapshot (None until first frame)
    pub last_stream_time: Option<StreamTime>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stream_config() {
        let config = StreamConfig::new("test-stream")
            .with_resolution(1920, 1080)
            .with_framerate(60)
            .with_dmabuf(true)
            .with_buffer_count(4);

        assert_eq!(config.name, "test-stream");
        assert_eq!(config.width, 1920);
        assert_eq!(config.height, 1080);
        assert_eq!(config.framerate, 60);
        assert!(config.use_dmabuf);
        assert_eq!(config.buffer_count, 4);
    }

    #[test]
    fn test_stream_state_from_pw() {
        assert_eq!(
            PwStreamState::from(StreamState::Streaming),
            PwStreamState::Streaming
        );
        assert_eq!(
            PwStreamState::from(StreamState::Paused),
            PwStreamState::Paused
        );
        assert_eq!(
            PwStreamState::from(StreamState::Unconnected),
            PwStreamState::Unconnected
        );
        assert_eq!(
            PwStreamState::from(StreamState::Connecting),
            PwStreamState::Connecting
        );
        assert_eq!(
            PwStreamState::from(StreamState::Error("test error".to_string())),
            PwStreamState::Error("test error".to_string())
        );
    }

    #[test]
    fn test_stream_state_event() {
        let event = StreamStateEvent {
            stream_id: 42,
            state: PwStreamState::Streaming,
        };
        assert_eq!(event.stream_id, 42);
        assert_eq!(event.state, PwStreamState::Streaming);
    }

    #[test]
    fn test_negotiated_format() {
        let format = NegotiatedFormat {
            format: VideoFormat::BGRA,
            width: 1920,
            height: 1080,
            stride: 7680,
            framerate: Fraction { num: 60, denom: 1 },
        };

        assert_eq!(format.width, 1920);
        assert_eq!(format.stride, 7680);
    }

    #[test]
    fn test_stream_time_delay_nsec() {
        let t = StreamTime {
            rate_num: 1,
            rate_denom: 60,
            delay: 120, // 120 ticks at 1/60 = 2 seconds = 2_000_000_000 ns
            ..Default::default()
        };
        assert_eq!(t.delay_nsec(), 2_000_000_000);
    }

    #[test]
    fn test_stream_time_delay_zero_denom() {
        let t = StreamTime {
            rate_num: 1,
            rate_denom: 0,
            delay: 100,
            ..Default::default()
        };
        // Uninitialized stream: should return 0 rather than divide-by-zero
        assert_eq!(t.delay_nsec(), 0);
    }

    #[test]
    fn test_stream_time_buffer_pressure() {
        // All buffers queued (maximum pressure)
        let t = StreamTime {
            queued_buffers: 4,
            avail_buffers: 0,
            ..Default::default()
        };
        assert!((t.buffer_pressure() - 1.0).abs() < f32::EPSILON);

        // No buffers queued (no pressure)
        let t2 = StreamTime {
            queued_buffers: 0,
            avail_buffers: 4,
            ..Default::default()
        };
        assert!(t2.buffer_pressure().abs() < f32::EPSILON);

        // Half and half
        let t3 = StreamTime {
            queued_buffers: 2,
            avail_buffers: 2,
            ..Default::default()
        };
        assert!((t3.buffer_pressure() - 0.5).abs() < f32::EPSILON);

        // No buffers at all
        let t4 = StreamTime::default();
        assert!(t4.buffer_pressure().abs() < f32::EPSILON);
    }
}
