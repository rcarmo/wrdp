//! Optional VA-API H.264 encoder boundary.

mod error;
mod factory;
mod stats;
#[cfg(feature = "vaapi")]
mod worker;
#[cfg(feature = "vaapi")]
pub use worker::HardwareEncoderWorker;
#[cfg(feature = "vaapi")]
pub mod vaapi;
#[cfg(feature = "vaapi")]
pub use error::VaapiError;
pub use error::{HardwareEncoderError, HardwareEncoderResult};
pub use factory::create_hardware_encoder;
pub use stats::{EncodeTimer, HardwareEncoderStats};

#[derive(Debug, Clone)]
pub struct H264Frame {
    pub data: Vec<u8>,
    pub is_keyframe: bool,
    pub timestamp_ms: u64,
    pub size: usize,
}
impl H264Frame {
    pub fn new(data: Vec<u8>, is_keyframe: bool, timestamp_ms: u64) -> Self {
        let size = data.len();
        Self {
            data,
            is_keyframe,
            timestamp_ms,
            size,
        }
    }
}

pub trait HardwareEncoder {
    fn encode_bgra(
        &mut self,
        data: &[u8],
        width: u32,
        height: u32,
        timestamp_ms: u64,
    ) -> HardwareEncoderResult<Option<H264Frame>>;
    fn force_keyframe(&mut self);
    fn stats(&self) -> HardwareEncoderStats;
    fn backend_name(&self) -> &'static str;
    fn supports_dynamic_resolution(&self) -> bool {
        false
    }
    fn reconfigure(&mut self, _width: u32, _height: u32) -> HardwareEncoderResult<()> {
        Err(HardwareEncoderError::ReconfigureNotSupported {
            backend: self.backend_name(),
        })
    }
    fn driver_name(&self) -> Option<&str> {
        None
    }
    fn flush(&mut self) -> HardwareEncoderResult<()> {
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QualityPreset {
    Speed,
    #[default]
    Balanced,
    Quality,
}
impl QualityPreset {
    pub fn parse(v: &str) -> Option<Self> {
        match v.trim().to_ascii_lowercase().as_str() {
            "speed" | "fast" => Some(Self::Speed),
            "balanced" | "default" | "medium" => Some(Self::Balanced),
            "quality" | "slow" | "high" => Some(Self::Quality),
            _ => None,
        }
    }
    pub const fn bitrate_kbps(self) -> u32 {
        match self {
            Self::Speed => 3000,
            Self::Balanced => 5000,
            Self::Quality => 10000,
        }
    }
    pub const fn gop_size(self) -> u32 {
        match self {
            Self::Speed => 60,
            Self::Balanced => 30,
            Self::Quality => 15,
        }
    }
}
impl std::fmt::Display for QualityPreset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Self::Speed => "speed",
                Self::Balanced => "balanced",
                Self::Quality => "quality",
            }
        )
    }
}
pub const fn is_hardware_encoding_available() -> bool {
    cfg!(feature = "vaapi")
}
pub fn available_backends() -> Vec<&'static str> {
    if cfg!(feature = "vaapi") {
        vec!["vaapi"]
    } else {
        vec![]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn presets_are_stable() {
        assert_eq!(QualityPreset::parse("fast"), Some(QualityPreset::Speed));
        assert_eq!(QualityPreset::Quality.gop_size(), 15);
    }
    #[test]
    fn frame_records_size() {
        assert_eq!(H264Frame::new(vec![1, 2], true, 0).size, 2);
    }
}
