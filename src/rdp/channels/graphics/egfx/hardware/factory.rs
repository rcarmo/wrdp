//! VA-API encoder construction.

#[cfg(not(feature = "vaapi"))]
use super::HardwareEncoderError;
#[cfg(feature = "vaapi")]
use super::vaapi::VaapiEncoder;
use super::{HardwareEncoder, HardwareEncoderResult, QualityPreset};
use crate::config::HardwareEncodingConfig;
use tracing::{info, warn};

pub fn create_hardware_encoder(
    config: &HardwareEncodingConfig,
    width: u32,
    height: u32,
) -> HardwareEncoderResult<Box<dyn HardwareEncoder>> {
    #[cfg(not(feature = "vaapi"))]
    {
        let _ = (config, width, height);
        return Err(HardwareEncoderError::NoBackendAvailable {
            reason: "VA-API support is not compiled".into(),
        });
    }
    #[cfg(feature = "vaapi")]
    {
        let preset = QualityPreset::parse(&config.quality_preset).unwrap_or_else(|| {
            warn!("invalid hardware quality preset; using balanced");
            QualityPreset::Balanced
        });
        info!(width,height,device=?config.vaapi_device,"initializing VA-API H.264 encoder");
        Ok(Box::new(VaapiEncoder::new(config, width, height, preset)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preset_parser_remains_the_policy_boundary() {
        assert_eq!(
            QualityPreset::parse("balanced"),
            Some(QualityPreset::Balanced)
        );
    }
}
