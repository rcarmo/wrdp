//! PipeWire FFI bindings and low-level helpers.
//!
//! Provides low-level types and conversion utilities used by runtime buffer and
//! format negotiation paths.

// Re-export ref types from pipewire crate (not owned Box/Rc types)
pub use pipewire::{
    context::Context,
    core::Core,
    loop_::Loop,
    main_loop::MainLoop,
    spa::{self, pod::Pod},
    stream::{Stream, StreamState},
};
// Owned types (use these when constructing PipeWire objects)
pub use pipewire::{
    context::ContextBox, main_loop::MainLoopBox, properties::PropertiesBox, stream::StreamBox,
};

pub use libspa::param::video::{VideoFormat, VideoInfoRaw};
pub use libspa::param::{ParamType, format::MediaSubtype, format::MediaType};
pub use libspa::pod::{self as spa_pod, Pod as SpaPod};
pub use libspa::utils::{Choice, ChoiceFlags, Direction, Fraction, Id, Rectangle};
pub use libspa_sys as spa_sys;

/// DRM format modifiers for DMA-BUF
pub mod drm_fourcc {
    pub const DRM_FORMAT_INVALID: u32 = 0;
    pub const DRM_FORMAT_MOD_INVALID: u64 = 0x00ffffffffffffff;
    pub const DRM_FORMAT_MOD_LINEAR: u64 = 0;

    // Common DRM formats
    pub const DRM_FORMAT_XRGB8888: u32 = 0x34325258; // XR24
    pub const DRM_FORMAT_ARGB8888: u32 = 0x34325241; // AR24
    pub const DRM_FORMAT_XBGR8888: u32 = 0x34324258; // XB24
    pub const DRM_FORMAT_ABGR8888: u32 = 0x34324241; // AB24
}

/// SPA video format to DRM fourcc conversion
pub fn spa_video_format_to_drm_fourcc(format: VideoFormat) -> u32 {
    match format {
        VideoFormat::BGRx => drm_fourcc::DRM_FORMAT_XRGB8888,
        VideoFormat::BGRA => drm_fourcc::DRM_FORMAT_ARGB8888,
        VideoFormat::RGBx => drm_fourcc::DRM_FORMAT_XBGR8888,
        VideoFormat::RGBA => drm_fourcc::DRM_FORMAT_ABGR8888,
        _ => drm_fourcc::DRM_FORMAT_INVALID,
    }
}

/// DRM fourcc to SPA video format conversion
pub fn drm_fourcc_to_spa_video_format(fourcc: u32) -> Option<VideoFormat> {
    match fourcc {
        drm_fourcc::DRM_FORMAT_XRGB8888 => Some(VideoFormat::BGRx),
        drm_fourcc::DRM_FORMAT_ARGB8888 => Some(VideoFormat::BGRA),
        drm_fourcc::DRM_FORMAT_XBGR8888 => Some(VideoFormat::RGBx),
        drm_fourcc::DRM_FORMAT_ABGR8888 => Some(VideoFormat::RGBA),
        _ => None,
    }
}

/// Get bytes per pixel for a video format
pub fn get_bytes_per_pixel(format: VideoFormat) -> usize {
    match format {
        VideoFormat::BGRx | VideoFormat::BGRA | VideoFormat::RGBx | VideoFormat::RGBA => 4,
        VideoFormat::RGB | VideoFormat::BGR => 3,
        VideoFormat::GRAY8 => 1,
        // YUV formats - return for Y plane
        VideoFormat::NV12 | VideoFormat::I420 => 1,
        VideoFormat::YUY2 => 2,
        _ => 4, // Default to 4
    }
}

/// Calculate stride for a given width and format
pub fn calculate_stride(width: u32, format: VideoFormat) -> u32 {
    let bpp = get_bytes_per_pixel(format) as u32;
    // Align to 16 bytes for performance
    (width * bpp).div_ceil(16) * 16
}

/// Calculate buffer size for a given format
pub fn calculate_buffer_size(width: u32, height: u32, format: VideoFormat) -> usize {
    let stride = calculate_stride(width, format) as usize;
    match format {
        // RGB formats
        VideoFormat::BGRx
        | VideoFormat::BGRA
        | VideoFormat::RGBx
        | VideoFormat::RGBA
        | VideoFormat::RGB
        | VideoFormat::BGR
        | VideoFormat::GRAY8 => stride * height as usize,

        // YUV420 formats (1.5 bytes per pixel)
        VideoFormat::NV12 | VideoFormat::I420 => (stride * height as usize * 3) / 2,

        // YUV422 formats (2 bytes per pixel)
        VideoFormat::YUY2 => stride * height as usize,

        _ => stride * height as usize,
    }
}

/// SPA Data type for buffer negotiation
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpaDataType {
    Invalid = 0,
    MemPtr = 1,
    MemFd = 2,
    DmaBuf = 3,
}

impl SpaDataType {
    pub fn from_u32(value: u32) -> Option<Self> {
        match value {
            0 => Some(Self::Invalid),
            1 => Some(Self::MemPtr),
            2 => Some(Self::MemFd),
            3 => Some(Self::DmaBuf),
            _ => None,
        }
    }
}

/// Buffer metadata structure
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct BufferMetadata {
    pub pts: u64,
    pub dts_offset: i64,
    pub seq: u32,
    pub flags: u32,
}

/// Damage region
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct DamageRegion {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl DamageRegion {
    pub fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn is_valid(&self) -> bool {
        self.width > 0 && self.height > 0
    }
}

/// Stream events listener trait
pub trait StreamEventsListener: Send + Sync {
    /// Called when stream state changes
    fn on_state_changed(&mut self, old: StreamState, new: StreamState, error: Option<&str>);

    /// Called when stream parameters change
    fn on_param_changed(&mut self, param_type: u32, param: &Pod);

    /// Called when a new buffer is added
    fn on_add_buffer(&mut self, buffer_id: u32);

    /// Called when a buffer is removed
    fn on_remove_buffer(&mut self, buffer_id: u32);

    /// Called when there's a frame to process
    fn on_process(&mut self);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_conversions() {
        assert_eq!(
            spa_video_format_to_drm_fourcc(VideoFormat::BGRx),
            drm_fourcc::DRM_FORMAT_XRGB8888
        );

        assert_eq!(
            drm_fourcc_to_spa_video_format(drm_fourcc::DRM_FORMAT_XRGB8888),
            Some(VideoFormat::BGRx)
        );
    }

    #[test]
    fn test_bytes_per_pixel() {
        assert_eq!(get_bytes_per_pixel(VideoFormat::BGRA), 4);
        assert_eq!(get_bytes_per_pixel(VideoFormat::RGB), 3);
        assert_eq!(get_bytes_per_pixel(VideoFormat::GRAY8), 1);
    }

    #[test]
    fn test_stride_calculation() {
        // 1920 * 4 = 7680, which is already aligned to 16
        assert_eq!(calculate_stride(1920, VideoFormat::BGRA), 7680);

        // 1921 * 4 = 7684, should align to 7696
        assert_eq!(calculate_stride(1921, VideoFormat::BGRA), 7696);
    }

    #[test]
    fn test_buffer_size_calculation() {
        // 1920x1080 BGRA: 1920 * 4 * 1080 = 8294400
        assert_eq!(
            calculate_buffer_size(1920, 1080, VideoFormat::BGRA),
            7680 * 1080
        );
    }

    #[test]
    fn test_damage_region() {
        let region = DamageRegion::new(10, 20, 100, 200);
        assert!(region.is_valid());

        let invalid = DamageRegion::new(0, 0, 0, 0);
        assert!(!invalid.is_valid());
    }
}
