//! Configuration type definitions shared across loader, validator, and runtime.
//!
//! These structs are the contract between operator-facing configuration
//! (INI/env/CLI), validation in `config::mod`, and runtime consumers in
//! server/session/channel modules. Keep them descriptive but conservative:
//! add new fields only for concrete runtime behavior, and document defaults in
//! terms of protocol/deployment constraints rather than convenience.

use std::{collections::BTreeMap, path::PathBuf};

use serde::{Deserialize, Serialize};

/// Server configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    /// Address to listen on (e.g., "0.0.0.0:3389")
    pub listen_addr: String,

    /// Maximum number of concurrent connections
    pub max_connections: usize,

    /// Session timeout in seconds (0 = no timeout)
    pub session_timeout: u64,

    /// Use XDG Desktop Portals for screen capture
    pub use_portals: bool,

    /// View-only mode: video streaming without input injection or clipboard.
    /// When true, forces ScreenCast-only strategy regardless of available capabilities.
    /// RDP clients can see the desktop but cannot control it.
    #[serde(default)]
    pub view_only: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen_addr: "0.0.0.0:3389".to_string(),
            max_connections: 1,
            session_timeout: 0,
            use_portals: true,
            view_only: false,
        }
    }
}

/// Security and authentication configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecurityConfig {
    /// Path to TLS certificate file
    pub cert_path: PathBuf,

    /// Path to TLS private key file
    pub key_path: PathBuf,

    /// Authentication method ("pam" or "password")
    pub auth_method: String,

    /// Optional username that this server instance will accept.
    /// Applied after credential validation and before session binding.
    #[serde(default)]
    pub allowed_username: Option<String>,

    /// Username -> Argon2id PHC password hash map for auth_method="password".
    ///
    /// Each key is an RDP-only username (not a system account), each value is
    /// an Argon2id PHC string generated from that user's password.
    #[serde(default)]
    pub password_credentials: BTreeMap<String, String>,

    /// Require TLS 1.3 or higher
    pub require_tls_13: bool,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            cert_path: super::default_cert_path(),
            key_path: super::default_key_path(),
            auth_method: "pam".to_string(),
            allowed_username: None,
            password_credentials: BTreeMap::new(),
            require_tls_13: false,
        }
    }
}

/// Video encoding configuration
///
/// Note: Encoder selection and bitrate are configured in their respective sections:
/// - Hardware encoding: `hardware_encoding.*`
/// - Bitrate: `egfx.h264_bitrate`
/// - Damage tracking: `damage_tracking.*`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoConfig {
    /// Target frames per second
    pub target_fps: u32,

    /// Cursor rendering mode ("embedded", "metadata", "hidden")
    pub cursor_mode: String,
}

impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            target_fps: 30,
            cursor_mode: "metadata".to_string(),
        }
    }
}

/// Capture protocol configuration
///
/// Controls which Wayland capture protocol is used by the portal-generic backend.
/// These settings are only relevant when the portal-generic backend is active
/// (i.e., direct Wayland compositor access, not Portal-based capture).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureProtocolConfig {
    /// Capture protocol preference: "auto", "ext", "wlr"
    /// - "auto": Auto-detect best available (ext preferred, wlr fallback)
    /// - "ext": Prefer ext-image-copy-capture-v1
    /// - "wlr": Prefer wlr-screencopy-unstable-v1 (wlroots)
    #[serde(default = "default_protocol_auto")]
    pub protocol: String,

    /// Allow fallback to alternative capture protocol if preferred is unavailable
    #[serde(default = "default_true")]
    pub allow_fallback: bool,

    /// Ext-capture handshake timeout in milliseconds
    /// How long to wait for the compositor to deliver constraint events after
    /// requesting an ext-image-copy-capture session. If the compositor advertises
    /// the protocol but doesn't respond, the capture will time out.
    /// Set to 0 to disable the timeout.
    #[serde(default = "default_handshake_timeout")]
    pub handshake_timeout_ms: u64,
}

fn default_protocol_auto() -> String {
    "auto".to_string()
}

fn default_handshake_timeout() -> u64 {
    5000
}

impl Default for CaptureProtocolConfig {
    fn default() -> Self {
        Self {
            protocol: default_protocol_auto(),
            allow_fallback: default_true(),
            handshake_timeout_ms: default_handshake_timeout(),
        }
    }
}

/// Input handling configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputConfig {
    /// Keyboard layout ("auto" or XKB layout name)
    pub keyboard_layout: String,

    /// Enable touch input support
    pub enable_touch: bool,

    /// Fall back to clipboard-paste (Ctrl+V) for CJK/non-ASCII Unicode characters
    /// that cannot be injected via evdev keycodes or XKB keysyms.
    ///
    /// When true (default), consecutive non-ASCII Unicode input units are buffered
    /// and flushed to the system clipboard, then a synthetic Ctrl+V is injected.
    /// This handles CJK text from mobile IMEs on KDE where keysym injection fails.
    #[serde(default = "default_true")]
    pub cjk_paste_fallback: bool,
}

impl Default for InputConfig {
    fn default() -> Self {
        Self {
            keyboard_layout: "auto".to_string(),
            enable_touch: false,
            cjk_paste_fallback: true,
        }
    }
}

/// Clipboard configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipboardConfig {
    /// Enable clipboard synchronization
    pub enabled: bool,

    /// Maximum clipboard data size in bytes
    pub max_size: usize,

    /// Minimum milliseconds between clipboard events (rate limiting)
    /// Default: 200 (max 5 events/second). Set to 0 to disable.
    #[serde(default = "default_rate_limit_ms")]
    pub rate_limit_ms: u64,

    /// Allowed MIME types (empty = all types allowed)
    pub allowed_types: Vec<String>,

    /// Clipboard protocol preference: "auto", "ext", "wlr"
    /// - "auto": Auto-detect best available (ext preferred, wlr fallback)
    /// - "ext": Prefer ext-data-control-v1
    /// - "wlr": Prefer wlr-data-control-v1 (wlroots)
    ///
    /// Only relevant for portal-generic backend (direct Wayland access).
    #[serde(default = "default_protocol_auto")]
    pub protocol: String,

    /// Allow fallback to alternative clipboard protocol if preferred is unavailable
    #[serde(default = "default_true")]
    pub allow_fallback: bool,
}

impl Default for ClipboardConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_size: 10_485_760, // 10 MB
            rate_limit_ms: default_rate_limit_ms(),
            allowed_types: vec![],
            protocol: default_protocol_auto(),
            allow_fallback: default_true(),
        }
    }
}

fn default_rate_limit_ms() -> u64 {
    200
}

/// Performance tuning configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerformanceConfig {
    /// Number of encoder threads (0 = auto)
    pub encoder_threads: usize,

    /// Number of network threads (0 = auto)
    pub network_threads: usize,

    /// Size of the frame buffer pool
    pub buffer_pool_size: usize,

    /// Enable zero-copy operations where possible
    pub zero_copy: bool,

    /// Adaptive FPS configuration (Premium feature)
    #[serde(default)]
    pub adaptive_fps: AdaptiveFpsConfig,

    /// Latency governor configuration (Premium feature)
    #[serde(default)]
    pub latency: LatencyConfig,
}

impl Default for PerformanceConfig {
    fn default() -> Self {
        Self {
            encoder_threads: 0,
            network_threads: 0,
            buffer_pool_size: 16,
            zero_copy: true,
            adaptive_fps: AdaptiveFpsConfig::default(),
            latency: LatencyConfig::default(),
        }
    }
}

/// Adaptive FPS configuration
///
/// Dynamically adjusts frame rate based on screen activity:
/// - Static screen: 5 FPS (saves CPU/bandwidth)
/// - Low activity: 15 FPS (typing, cursor)
/// - Medium activity: 20 FPS (scrolling)
/// - High activity: 30-60 FPS (video, dragging)
///
/// # High Performance Mode (60 FPS)
///
/// For systems with powerful GPUs and fast networks, enable 60fps in wrdp.ini:
///
/// ```toml
/// [performance.adaptive_fps]
/// enabled = true
/// max_fps = 60
/// ```
///
/// **Requirements for 60fps:**
/// - Hardware encoder (VA-API) strongly recommended
/// - Fast network connection (>10Mbps recommended)
/// - Modern client supporting H.264 High Profile
/// - Sufficient GPU headroom for encoding
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdaptiveFpsConfig {
    /// Enable adaptive FPS (false = fixed FPS)
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Minimum FPS even for static content
    #[serde(default = "default_min_fps")]
    pub min_fps: u32,

    /// Maximum FPS for high activity (default: 30, set to 60 for high-performance mode)
    #[serde(default = "default_max_fps")]
    pub max_fps: u32,

    /// Damage ratio threshold for high activity (0.0-1.0)
    #[serde(default = "default_high_activity")]
    pub high_activity_threshold: f32,

    /// Damage ratio threshold for medium activity (0.0-1.0)
    #[serde(default = "default_medium_activity")]
    pub medium_activity_threshold: f32,

    /// Damage ratio threshold for low activity (0.0-1.0)
    #[serde(default = "default_low_activity")]
    pub low_activity_threshold: f32,
}

fn default_min_fps() -> u32 {
    5
}
fn default_max_fps() -> u32 {
    30
}
fn default_high_activity() -> f32 {
    0.30
}
fn default_medium_activity() -> f32 {
    0.10
}
fn default_low_activity() -> f32 {
    0.01
}

impl Default for AdaptiveFpsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_fps: 5,
            max_fps: 30,
            high_activity_threshold: 0.30,
            medium_activity_threshold: 0.10,
            low_activity_threshold: 0.01,
        }
    }
}

/// Latency governor configuration
///
/// Professional latency vs quality tradeoffs:
/// - Interactive: <50ms (gaming, CAD)
/// - Balanced: <100ms (general desktop)
/// - Quality: <300ms (photo/video editing)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LatencyConfig {
    /// Latency mode: "interactive", "balanced", "quality"
    #[serde(default = "default_latency_mode")]
    pub mode: String,

    /// Interactive mode max frame delay (ms)
    #[serde(default = "default_interactive_delay")]
    pub interactive_max_delay_ms: u32,

    /// Balanced mode max frame delay (ms)
    #[serde(default = "default_balanced_delay")]
    pub balanced_max_delay_ms: u32,

    /// Quality mode max frame delay (ms)
    #[serde(default = "default_quality_delay")]
    pub quality_max_delay_ms: u32,

    /// Balanced mode damage threshold
    #[serde(default = "default_balanced_threshold")]
    pub balanced_damage_threshold: f32,

    /// Quality mode damage threshold
    #[serde(default = "default_quality_threshold")]
    pub quality_damage_threshold: f32,
}

fn default_latency_mode() -> String {
    "balanced".to_string()
}
fn default_interactive_delay() -> u32 {
    16
}
fn default_balanced_delay() -> u32 {
    33
}
fn default_quality_delay() -> u32 {
    100
}
fn default_balanced_threshold() -> f32 {
    0.02
}
fn default_quality_threshold() -> f32 {
    0.05
}

impl Default for LatencyConfig {
    fn default() -> Self {
        Self {
            mode: "balanced".to_string(),
            interactive_max_delay_ms: 16,
            balanced_max_delay_ms: 33,
            quality_max_delay_ms: 100,
            balanced_damage_threshold: 0.02,
            quality_damage_threshold: 0.05,
        }
    }
}

/// Cursor handling configuration (Premium)
///
/// Controls how cursors are rendered and managed:
/// - Metadata: Client-side rendering (lowest latency)
/// - Painted: Composited into video frames (maximum compatibility)
/// - Hidden: No cursor (touch/pen)
///
/// Predictive cursor configuration
///
/// Physics-based cursor prediction to compensate for network latency.
/// Uses velocity and acceleration tracking to predict where the cursor
/// Logging configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    /// Log level ("trace", "debug", "info", "warn", "error")
    pub level: String,

    /// Directory for log files (None = console only)
    pub log_dir: Option<PathBuf>,

    /// Enable metrics collection
    pub metrics: bool,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
            log_dir: None,
            metrics: true,
        }
    }
}

/// Frame processor configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessorConfig {
    /// Target frame rate (FPS)
    pub target_fps: u32,

    /// Maximum frame queue depth
    pub max_queue_depth: usize,

    /// Enable adaptive quality
    pub adaptive_quality: bool,

    /// Damage tracking threshold (0.0-1.0)
    pub damage_threshold: f32,

    /// Drop frames when queue is full
    pub drop_on_full_queue: bool,

    /// Enable performance metrics
    pub enable_metrics: bool,
}

/// Frame dispatcher configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DispatcherConfig {
    /// Channel buffer size per stream
    pub channel_size: usize,

    /// Enable priority-based dispatch
    pub priority_dispatch: bool,

    /// Maximum frame age before drop (ms)
    pub max_frame_age_ms: u64,

    /// Enable backpressure handling
    pub enable_backpressure: bool,

    /// High water mark (0.0-1.0)
    pub high_water_mark: f32,

    /// Low water mark (0.0-1.0)
    pub low_water_mark: f32,

    /// Enable load balancing
    pub load_balancing: bool,
}

/// Bitmap converter configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConverterConfig {
    /// Buffer pool size
    pub buffer_pool_size: usize,

    /// Enable SIMD optimizations
    pub enable_simd: bool,

    /// Damage threshold for full update (0.0-1.0)
    pub damage_threshold: f32,

    /// Enable statistics collection
    pub enable_statistics: bool,
}

impl Default for ProcessorConfig {
    fn default() -> Self {
        Self {
            target_fps: 30,
            max_queue_depth: 30,
            adaptive_quality: true,
            damage_threshold: 0.05,
            drop_on_full_queue: true,
            enable_metrics: true,
        }
    }
}

impl Default for DispatcherConfig {
    fn default() -> Self {
        Self {
            channel_size: 30,
            priority_dispatch: true,
            max_frame_age_ms: 150,
            enable_backpressure: true,
            high_water_mark: 0.8,
            low_water_mark: 0.5,
            load_balancing: true,
        }
    }
}

impl Default for ConverterConfig {
    fn default() -> Self {
        Self {
            buffer_pool_size: 8,
            enable_simd: true,
            damage_threshold: 0.75,
            enable_statistics: true,
        }
    }
}

/// EGFX (Graphics Pipeline Extension) configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EgfxConfig {
    /// Enable EGFX graphics pipeline
    pub enabled: bool,

    /// H.264 level: "auto" or explicit "3.0", "3.1", "4.0", "4.1", "5.0", "5.1", "5.2"
    pub h264_level: String,

    /// H.264 bitrate in kbps (main stream for AVC444)
    pub h264_bitrate: u32,

    /// ZGFX compression mode: "never", "auto", "always"
    pub zgfx_compression: String,

    /// Maximum frames in flight before backpressure
    pub max_frames_in_flight: u32,

    /// Periodic IDR keyframe interval in seconds (0 = disabled)
    /// Forces a full IDR keyframe at regular intervals to clear accumulated artifacts.
    /// Recommended: 5-10 seconds for VDI, 2-3 for unreliable networks.
    /// Default: 5 seconds
    #[serde(default = "default_periodic_idr_interval")]
    pub periodic_idr_interval: u32,

    /// Video codec preference: "auto", "avc420", "avc444", "bitmap".
    /// Preferences are intersected with the client's wire capabilities; an
    /// unavailable requested AVC mode safely downgrades to core bitmap updates.
    /// - "auto": Prefer AVC444, then AVC420, then bitmap
    /// - "avc420": Prefer AVC420; do not substitute AVC444
    /// - "avc444": Prefer AVC444; do not substitute AVC420
    /// - "bitmap": Disable RDPGFX and use core bitmap updates immediately
    pub codec: String,

    /// Quality parameter range
    pub qp_min: u8,
    pub qp_max: u8,
    pub qp_default: u8,

    // === AVC444-specific configuration ===
    /// AVC444 auxiliary stream bitrate ratio (0.3-1.0)
    /// Ratio of auxiliary stream bitrate relative to main stream.
    /// - 0.5 = aux gets 50% of main's bitrate (good for typical content)
    /// - 1.0 = aux gets same bitrate as main (best quality for text-heavy)
    /// - 0.3 = aux gets 30% of main's bitrate (saves bandwidth)
    #[serde(default = "default_avc444_aux_ratio")]
    pub avc444_aux_bitrate_ratio: f32,

    /// Color matrix for YUV conversion: "auto", "openh264", "bt709", "bt601", "srgb"
    /// - "auto": Use OpenH264-compatible for AVC444 consistency
    /// - "openh264": Match OpenH264's internal conversion (BT.601 limited)
    /// - "bt709": BT.709 for HD content
    /// - "bt601": BT.601 for SD content
    /// - "srgb": sRGB for computer graphics
    ///
    /// Default: "auto" (OpenH264-compatible for AVC420/AVC444 consistency)
    #[serde(default = "default_color_matrix")]
    pub color_matrix: String,

    /// Color range for YUV encoding: "auto", "limited", "full"
    /// - "auto": Use matrix default (limited for broadcast compatibility)
    /// - "limited": TV range (Y: 16-235, UV: 16-240) - recommended
    /// - "full": PC range (Y: 0-255, UV: 0-255) - maximum dynamic range
    ///
    /// Default: "auto" (limited range for compatibility)
    #[serde(default = "default_color_range")]
    pub color_range: String,

    /// Enable AVC444 when client supports it
    /// Set to false to disable AVC444 globally regardless of codec preference
    #[serde(default = "default_true")]
    pub avc444_enabled: bool,

    // === PHASE 1: AUX OMISSION (BANDWIDTH OPTIMIZATION) ===
    /// Enable auxiliary stream omission for bandwidth optimization
    /// When true: Implements FreeRDP-style aux omission (LC field)
    /// When false: Always sends both streams (backward compatible)
    /// Default: true (production proven at 0.81 MB/s)
    #[serde(default = "default_true")]
    pub avc444_enable_aux_omission: bool,

    /// Maximum frames between auxiliary updates (1-120)
    /// Forces aux refresh even if unchanged for quality assurance
    /// - 10-20: Responsive to color changes, higher bandwidth
    /// - 30-40: Balanced (recommended)
    /// - 60-120: Aggressive omission, static content optimized
    ///
    /// Default: 30 frames (1 second @ 30fps)
    #[serde(default = "default_aux_interval")]
    pub avc444_max_aux_interval: u32,

    /// Force auxiliary IDR when reintroducing after omission
    /// true: Safe mode, but with single encoder forces Main to IDR too!
    /// false: Required for single encoder to allow Main P-frames (PRODUCTION)
    /// Default: false
    #[serde(default = "default_false")]
    pub avc444_force_aux_idr_on_return: bool,
}

fn default_avc444_aux_ratio() -> f32 {
    0.5
}

fn default_aux_interval() -> u32 {
    30 // 1 second @ 30fps
}

fn default_false() -> bool {
    false
}

fn default_periodic_idr_interval() -> u32 {
    5 // 5 seconds - clears artifacts regularly without excessive bandwidth
}

fn default_color_matrix() -> String {
    "auto".to_string()
}

fn default_color_range() -> String {
    "auto".to_string()
}

fn default_true() -> bool {
    true
}

impl Default for EgfxConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            h264_level: "auto".to_string(),
            h264_bitrate: 5000,
            zgfx_compression: "never".to_string(),
            max_frames_in_flight: 3,
            periodic_idr_interval: 5, // Force IDR every 5 seconds to clear artifacts
            codec: "auto".to_string(), // Use best available (AVC444 if supported, else AVC420)
            qp_min: 10,
            qp_max: 40,
            qp_default: 23,
            // AVC444-specific defaults
            avc444_aux_bitrate_ratio: 0.5, // Aux gets 50% of main's bitrate
            color_matrix: "auto".to_string(), // Auto-detect based on resolution
            color_range: "auto".to_string(), // Use matrix default (limited for compatibility)
            avc444_enabled: true,          // Enable AVC444 when client supports it
            // Phase 1: Aux omission defaults (NOW PRODUCTION DEFAULTS)
            avc444_enable_aux_omission: true, // Enabled by default (production proven)
            avc444_max_aux_interval: 30,      // 1 second @ 30fps
            avc444_force_aux_idr_on_return: false, // Must be false for single encoder
        }
    }
}

/// Damage tracking configuration
///
/// Controls how frame changes are detected to optimize bandwidth.
/// Smaller tiles and lower thresholds = more sensitive (detects small changes like typing)
/// Larger tiles and higher thresholds = less sensitive (better for video/animations)
///
/// ## Sensitivity Tuning
///
/// For **text/office work** (typing must be detected) - NEW DEFAULTS:
/// - `tile_size: 16` - Matches FreeRDP's 16x16 tiles for maximum sensitivity
/// - `diff_threshold: 0.01` - 1% threshold catches single characters
/// - `pixel_threshold: 1` - Maximum sensitivity pixel comparison
///
/// For **video/streaming** (prioritize bandwidth):
/// - `tile_size: 128` - Larger tiles reduce overhead
/// - `diff_threshold: 0.10` - Higher threshold (10% required)
/// - `pixel_threshold: 8` - Less sensitive to subtle changes
///
/// ## How Detection Works
///
/// 1. Frame is divided into tiles of `tile_size` x `tile_size` pixels
/// 2. Each tile is compared to the preceding frame
/// 3. Pixels differing by more than `pixel_threshold` (in any RGB channel) are counted
/// 4. If changed pixels exceed `diff_threshold` fraction of tile, tile is marked dirty
/// 5. Dirty tiles are merged if within `merge_distance` pixels
/// 6. Regions smaller than `min_region_area` are discarded
///
/// ## Example: New Defaults Ensure Typing Is Detected
///
/// With new defaults (tile_size=16, diff_threshold=0.01):
/// - Tile area = 16×16 = 256 pixels
/// - Threshold = 256 × 0.01 = 2.56 → 3 pixels must change
/// - A typed character ≈ 10×14 = 140 pixels
/// - 140 > 3 → tile marked dirty ✓
///
/// This matches FreeRDP's approach (16x16 tiles) for reliable detection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DamageTrackingConfig {
    /// Enable damage region detection
    ///
    /// When enabled, only changed regions are encoded and sent,
    /// significantly reducing bandwidth for static content.
    pub enabled: bool,

    /// Detection method: "pipewire", "diff", "hybrid"
    ///
    /// - "diff": CPU-based pixel differencing (most compatible)
    /// - "pipewire": Use PipeWire damage hints if available
    /// - "hybrid": Combine both methods
    pub method: String,

    /// Tile size for differencing (pixels)
    ///
    /// Smaller values = more sensitive, more CPU overhead
    /// Larger values = less sensitive, less CPU overhead
    ///
    /// Recommended: 32 for text work, 64 for general use, 128 for video
    #[serde(default = "default_tile_size")]
    pub tile_size: usize,

    /// Fraction of tile pixels that must differ to mark tile as dirty (0.0-1.0)
    ///
    /// Lower values = more sensitive (detects smaller changes)
    /// Higher values = less sensitive (ignores minor changes)
    ///
    /// Recommended: 0.02 for text work, 0.05 for general use, 0.10 for video
    #[serde(default = "default_diff_threshold")]
    pub diff_threshold: f32,

    /// Pixel difference threshold for RGB comparison
    ///
    /// Pixels differing by less than this value (in any RGB channel) are
    /// considered identical. Higher values ignore subtle color variations.
    ///
    /// Recommended: 2 for text work, 4 for general use, 8 for video
    #[serde(default = "default_pixel_threshold")]
    pub pixel_threshold: u8,

    /// Merge distance for adjacent dirty tiles (pixels)
    ///
    /// Dirty tiles within this distance are merged into larger regions
    /// to reduce encoding overhead.
    #[serde(default = "default_merge_distance")]
    pub merge_distance: u32,

    /// Minimum region area to encode (pixels²)
    ///
    /// Regions smaller than this are discarded as noise.
    /// Set to 1 to encode all detected changes.
    #[serde(default = "default_min_region_area")]
    pub min_region_area: u64,
}

fn default_tile_size() -> usize {
    16 // 16x16 tiles for maximum sensitivity (FreeRDP uses 16x16)
}

fn default_diff_threshold() -> f32 {
    0.01 // 1% threshold - very sensitive, catches single-character changes
}

fn default_pixel_threshold() -> u8 {
    1 // Single pixel difference threshold for maximum sensitivity
}

fn default_merge_distance() -> u32 {
    16
}

fn default_min_region_area() -> u64 {
    64 // 8x8 pixel minimum
}

impl Default for DamageTrackingConfig {
    fn default() -> Self {
        Self {
            enabled: true, // Enable by default for bandwidth savings
            method: "diff".to_string(),
            tile_size: default_tile_size(),
            diff_threshold: default_diff_threshold(),
            pixel_threshold: default_pixel_threshold(),
            merge_distance: default_merge_distance(),
            min_region_area: default_min_region_area(),
        }
    }
}

/// Hardware encoding configuration
///
/// Supports multiple GPU backends:
/// - VA-API: Intel (iHD/i965) and AMD (radeonsi) GPUs
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HardwareEncodingConfig {
    /// Enable hardware-accelerated encoding
    pub enabled: bool,

    /// VA-API device path (for Intel/AMD GPUs)
    pub vaapi_device: PathBuf,

    /// Enable zero-copy DMA-BUF path (VA-API only)
    pub enable_dmabuf_zerocopy: bool,

    /// Fallback to software encoding if hardware fails
    pub fallback_to_software: bool,

    /// Encoder quality preset: "speed", "balanced", "quality"
    /// - speed: Low latency, lower quality (3 Mbps)
    /// - balanced: Good balance of quality and latency (5 Mbps)
    /// - quality: Best quality, higher latency (10 Mbps)
    pub quality_preset: String,
}

impl Default for HardwareEncodingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            vaapi_device: PathBuf::from("/dev/dri/renderD128"),
            enable_dmabuf_zerocopy: true,
            fallback_to_software: true,
            quality_preset: "balanced".to_string(),
        }
    }
}

/// Display control configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisplayConfig {
    /// Allow dynamic resolution changes
    pub allow_resize: bool,

    /// Allowed resolutions (empty = all allowed)
    pub allowed_resolutions: Vec<String>,

    /// DPI scaling support
    pub dpi_aware: bool,

    /// Allow orientation changes
    pub allow_rotation: bool,
}

impl Default for DisplayConfig {
    fn default() -> Self {
        Self {
            allow_resize: true,
            allowed_resolutions: vec![],
            dpi_aware: false,
            allow_rotation: false,
        }
    }
}
