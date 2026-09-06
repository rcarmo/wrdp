//! Standards-backed color metadata used by H.264 encoders.

use super::convert::ColorMatrix;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ColorRange {
    #[default]
    Limited,
    Full,
}
impl ColorRange {
    pub const fn vui_flag(self) -> u8 {
        matches!(self, Self::Full) as u8
    }
    pub const fn y_range(self) -> (u8, u8) {
        if matches!(self, Self::Full) {
            (0, 255)
        } else {
            (16, 235)
        }
    }
    pub const fn uv_range(self) -> (u8, u8) {
        if matches!(self, Self::Full) {
            (0, 255)
        } else {
            (16, 240)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ColourPrimaries {
    BT709 = 1,
    Unspecified = 2,
    BT601PAL = 5,
    BT601NTSC = 6,
    BT2020 = 9,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TransferCharacteristics {
    BT709 = 1,
    Unspecified = 2,
    BT601 = 6,
    SRGB = 13,
    BT2020_10 = 14,
    BT2020_12 = 15,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MatrixCoefficients {
    BT709 = 1,
    Unspecified = 2,
    BT601 = 6,
    BT2020NCL = 9,
}

#[derive(Debug, Clone, Copy)]
pub struct ColorSpaceConfig {
    pub matrix: ColorMatrix,
    pub range: ColorRange,
    pub primaries: ColourPrimaries,
    pub transfer: TransferCharacteristics,
    pub matrix_coeff: MatrixCoefficients,
}
impl ColorSpaceConfig {
    pub const OPENH264_COMPATIBLE: Self = Self {
        matrix: ColorMatrix::OpenH264,
        range: ColorRange::Limited,
        primaries: ColourPrimaries::BT601NTSC,
        transfer: TransferCharacteristics::BT601,
        matrix_coeff: MatrixCoefficients::BT601,
    };
    pub const BT601_LIMITED: Self = Self {
        matrix: ColorMatrix::BT601,
        range: ColorRange::Limited,
        primaries: ColourPrimaries::BT601NTSC,
        transfer: TransferCharacteristics::BT601,
        matrix_coeff: MatrixCoefficients::BT601,
    };
    pub const BT709_LIMITED: Self = Self {
        matrix: ColorMatrix::BT709,
        range: ColorRange::Limited,
        primaries: ColourPrimaries::BT709,
        transfer: TransferCharacteristics::BT709,
        matrix_coeff: MatrixCoefficients::BT709,
    };
    pub const BT709_FULL: Self = Self {
        matrix: ColorMatrix::BT709,
        range: ColorRange::Full,
        primaries: ColourPrimaries::BT709,
        transfer: TransferCharacteristics::BT709,
        matrix_coeff: MatrixCoefficients::BT709,
    };
    pub const SRGB_FULL: Self = Self {
        matrix: ColorMatrix::BT709,
        range: ColorRange::Full,
        primaries: ColourPrimaries::BT709,
        transfer: TransferCharacteristics::SRGB,
        matrix_coeff: MatrixCoefficients::BT709,
    };
    pub const fn auto_select(width: u32, height: u32, openh264_compat: bool) -> Self {
        if openh264_compat {
            Self::OPENH264_COMPATIBLE
        } else if width >= 1280 && height >= 720 {
            Self::BT709_LIMITED
        } else {
            Self::BT601_LIMITED
        }
    }
    pub fn from_resolution(width: u32, height: u32) -> Self {
        Self::auto_select(width, height, false)
    }
    pub fn from_config(space: &str, range: &str, width: u32, height: u32) -> Self {
        let mut selected = match space.to_ascii_lowercase().as_str() {
            "bt709" => Self::BT709_LIMITED,
            "bt601" => Self::BT601_LIMITED,
            "srgb" => Self::SRGB_FULL,
            "openh264" => Self::OPENH264_COMPATIBLE,
            _ => Self::auto_select(width, height, true),
        };
        if range.eq_ignore_ascii_case("full") {
            selected.range = ColorRange::Full;
        } else if range.eq_ignore_ascii_case("limited") {
            selected.range = ColorRange::Limited;
        }
        selected
    }
    pub const fn vui_full_range_flag(self) -> u8 {
        self.range.vui_flag()
    }
    pub const fn vui_colour_primaries(self) -> u8 {
        self.primaries as u8
    }
    pub const fn vui_transfer_characteristics(self) -> u8 {
        self.transfer as u8
    }
    pub const fn vui_matrix_coefficients(self) -> u8 {
        self.matrix_coeff as u8
    }
    pub const fn is_limited_range(self) -> bool {
        matches!(self.range, ColorRange::Limited)
    }
    pub fn description(self) -> String {
        format!(
            "{} {}",
            match self.matrix {
                ColorMatrix::BT601 => "BT.601",
                ColorMatrix::BT709 => "BT.709",
                ColorMatrix::OpenH264 => "OpenH264",
            },
            if self.is_limited_range() {
                "limited"
            } else {
                "full"
            }
        )
    }
}
impl Default for ColorSpaceConfig {
    fn default() -> Self {
        Self::OPENH264_COMPATIBLE
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hd_uses_bt709() {
        assert_eq!(
            ColorSpaceConfig::auto_select(1920, 1080, false).matrix,
            ColorMatrix::BT709
        );
    }
    #[test]
    fn openh264_uses_limited_601() {
        let c = ColorSpaceConfig::OPENH264_COMPATIBLE;
        assert_eq!(c.range, ColorRange::Limited);
        assert_eq!(c.vui_matrix_coefficients(), 6);
    }
}
