//! Thin OpenH264 AVC420 adapter for IronRDP EGFX delivery.

use crate::rdp::channels::graphics::egfx::color::space::ColorSpaceConfig;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum EncoderError {
    #[error("encoder initialization: {0}")]
    InitFailed(String),
    #[error("encode: {0}")]
    EncodeFailed(String),
    #[error("invalid frame dimensions: {width}x{height}")]
    InvalidDimensions { width: u32, height: u32 },
    #[error("H.264 feature not enabled")]
    FeatureDisabled,
}
pub type EncoderResult<T> = Result<T, EncoderError>;
#[derive(Debug, Clone)]
pub struct EncoderConfig {
    pub bitrate_kbps: u32,
    pub max_fps: f32,
    pub enable_skip_frame: bool,
    pub width: Option<u16>,
    pub height: Option<u16>,
    pub color_space: Option<ColorSpaceConfig>,
    pub qp_min: u8,
    pub qp_max: u8,
    pub encoder_threads: u16,
}
impl Default for EncoderConfig {
    fn default() -> Self {
        Self {
            bitrate_kbps: 5000,
            max_fps: 30.0,
            enable_skip_frame: true,
            width: None,
            height: None,
            color_space: None,
            qp_min: 0,
            qp_max: 51,
            encoder_threads: 0,
        }
    }
}
impl EncoderConfig {
    pub fn for_resolution(width: u16, height: u16) -> Self {
        Self {
            width: Some(width),
            height: Some(height),
            ..Self::default()
        }
    }
    pub fn high_quality() -> Self {
        Self {
            bitrate_kbps: 10000,
            enable_skip_frame: false,
            qp_min: 10,
            qp_max: 25,
            ..Self::default()
        }
    }
    pub fn high_performance() -> Self {
        Self {
            bitrate_kbps: 8000,
            max_fps: 60.0,
            ..Self::default()
        }
    }
    pub fn low_bandwidth() -> Self {
        Self {
            bitrate_kbps: 1000,
            max_fps: 15.0,
            qp_min: 20,
            qp_max: 45,
            ..Self::default()
        }
    }
    pub fn with_color_space(mut self, value: ColorSpaceConfig) -> Self {
        self.color_space = Some(value);
        self
    }
}
#[derive(Debug)]
pub struct H264Frame {
    pub data: Vec<u8>,
    pub is_keyframe: bool,
    pub timestamp_ms: u64,
    pub size: usize,
}
#[inline]
pub const fn align_to_16(value: u32) -> u32 {
    value.div_ceil(16) * 16
}
#[derive(Debug, Clone)]
pub struct EncoderStats {
    pub frames_encoded: u64,
    pub bitrate_kbps: u32,
}

#[cfg(feature = "h264")]
pub(crate) fn load_openh264_api() -> EncoderResult<::openh264::OpenH264API> {
    #[cfg(feature = "h264-source")]
    {
        Ok(::openh264::OpenH264API::from_source())
    }
    #[cfg(not(feature = "h264-source"))]
    {
        let path = std::env::var_os("OPENH264_LIBRARY_PATH").ok_or_else(|| {
            EncoderError::InitFailed(
                "OPENH264_LIBRARY_PATH must name a hash-verified Cisco module".into(),
            )
        })?;
        ::openh264::OpenH264API::from_blob_path(path)
            .map_err(|e| EncoderError::InitFailed(e.to_string()))
    }
}

#[cfg(feature = "h264")]
pub struct Avc420Encoder {
    encoder: ::openh264::encoder::Encoder,
    config: EncoderConfig,
    frames: u64,
    parameter_sets: Vec<u8>,
}
#[cfg(feature = "h264")]
impl Avc420Encoder {
    pub fn new(config: EncoderConfig) -> EncoderResult<Self> {
        use ::openh264::encoder::{
            BitRate, EncoderConfig as Native, FrameRate, QpRange, UsageType,
        };
        let mut native = Native::new()
            .bitrate(BitRate::from_bps(config.bitrate_kbps * 1000))
            .max_frame_rate(FrameRate::from_hz(config.max_fps))
            .usage_type(UsageType::ScreenContentRealTime)
            .num_threads(config.encoder_threads)
            .skip_frames(config.enable_skip_frame)
            .qp(QpRange::new(config.qp_min, config.qp_max));
        if let Some(level) = config
            .width
            .zip(config.height)
            .map(|(w, h)| super::level::H264Level::for_config(w, h, config.max_fps))
        {
            native = native.level(level.to_upstream());
        }
        let encoder = ::openh264::encoder::Encoder::with_api_config(load_openh264_api()?, native)
            .map_err(|e| EncoderError::InitFailed(e.to_string()))?;
        Ok(Self {
            encoder,
            config,
            frames: 0,
            parameter_sets: vec![],
        })
    }
    pub fn encode_bgra(
        &mut self,
        bgra: &[u8],
        width: u32,
        height: u32,
        timestamp_ms: u64,
    ) -> EncoderResult<Option<H264Frame>> {
        use ::openh264::formats::{BgraSliceU8, YUVBuffer};
        if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            return Err(EncoderError::InvalidDimensions { width, height });
        }
        let required = width as usize * height as usize * 4;
        if bgra.len() < required {
            return Err(EncoderError::EncodeFailed(
                "BGRA input is shorter than its dimensions".into(),
            ));
        }
        let yuv = YUVBuffer::from_rgb_source(BgraSliceU8::new(
            &bgra[..required],
            (width as usize, height as usize),
        ));
        let encoded = self
            .encoder
            .encode_at(&yuv, ::openh264::Timestamp::from_millis(timestamp_ms))
            .map_err(|e| EncoderError::EncodeFailed(e.to_string()))?;
        let keyframe = matches!(
            encoded.frame_type(),
            ::openh264::encoder::FrameType::IDR | ::openh264::encoder::FrameType::I
        );
        let mut data = encoded.to_vec();
        if data.is_empty() {
            return Ok(None);
        }
        if keyframe {
            self.parameter_sets = parameter_sets(&data);
        } else if !self.parameter_sets.is_empty() {
            let mut framed = self.parameter_sets.clone();
            framed.extend_from_slice(&data);
            data = framed;
        }
        self.frames += 1;
        let size = data.len();
        Ok(Some(H264Frame {
            data,
            is_keyframe: keyframe,
            timestamp_ms,
            size,
        }))
    }
    pub fn force_keyframe(&mut self) {
        self.encoder.force_intra_frame();
    }
    pub fn stats(&self) -> EncoderStats {
        EncoderStats {
            frames_encoded: self.frames,
            bitrate_kbps: self.config.bitrate_kbps,
        }
    }
}

#[cfg_attr(
    not(feature = "h264"),
    expect(
        dead_code,
        reason = "retained shared API when the runtime backend feature is disabled"
    )
)]
fn parameter_sets(data: &[u8]) -> Vec<u8> {
    annex_b(data)
        .filter(|(_, kind)| matches!(kind, 7 | 8))
        .flat_map(|(bytes, _)| bytes.iter().copied())
        .collect()
}
fn annex_b(data: &[u8]) -> impl Iterator<Item = (&[u8], u8)> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        let n = if data[i..].starts_with(&[0, 0, 0, 1]) {
            4
        } else if data[i..].starts_with(&[0, 0, 1]) {
            3
        } else {
            i += 1;
            continue;
        };
        starts.push((i, n));
        i += n;
    }
    starts
        .into_iter()
        .enumerate()
        .filter_map(move |(index, (start, prefix))| {
            let end = starts_get(data, index + 1).unwrap_or(data.len());
            let payload = start + prefix;
            (payload < end).then(|| (&data[start..end], data[payload] & 0x1f))
        })
}
fn starts_get(data: &[u8], wanted: usize) -> Option<usize> {
    let mut count = 0;
    let mut i = 0;
    while i + 3 <= data.len() {
        let found = data[i..].starts_with(&[0, 0, 1]) || data[i..].starts_with(&[0, 0, 0, 1]);
        if found {
            if count == wanted {
                return Some(i);
            }
            count += 1;
            i += 3;
        } else {
            i += 1;
        }
    }
    None
}

#[cfg(not(feature = "h264"))]
pub struct Avc420Encoder;
#[cfg(not(feature = "h264"))]
impl Avc420Encoder {
    pub fn new(_: EncoderConfig) -> EncoderResult<Self> {
        Err(EncoderError::FeatureDisabled)
    }
    pub fn encode_bgra(
        &mut self,
        _: &[u8],
        width: u32,
        height: u32,
        _: u64,
    ) -> EncoderResult<Option<H264Frame>> {
        if width == 0 || height == 0 {
            Err(EncoderError::InvalidDimensions { width, height })
        } else {
            Err(EncoderError::FeatureDisabled)
        }
    }
    pub fn force_keyframe(&mut self) {}
    pub fn stats(&self) -> EncoderStats {
        EncoderStats {
            frames_encoded: 0,
            bitrate_kbps: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn alignment_rounds_up() {
        assert_eq!(align_to_16(17), 32);
        assert_eq!(align_to_16(32), 32);
    }
    #[cfg(feature = "h264")]
    #[test]
    fn extracts_parameter_sets() {
        let bytes = [0, 0, 0, 1, 0x67, 1, 0, 0, 1, 0x68, 2, 0, 0, 1, 0x65, 3];
        assert_eq!(
            parameter_sets(&bytes),
            vec![0, 0, 0, 1, 0x67, 1, 0, 0, 1, 0x68, 2]
        );
    }
    #[test]
    fn presets_are_bounded() {
        let q = EncoderConfig::high_quality();
        assert!(q.qp_min <= q.qp_max);
    }
}
