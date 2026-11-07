use serde::{Deserialize, Serialize};

pub const SCREEN_AUDIO_ALAC_44100_STEREO: u64 = 0x0004_0000;
pub const SCREEN_AUDIO_AAC_ELD_44100_STEREO: u64 = 0x0100_0000;

/// Parsed feature flags from AirPlay TXT record `features` field.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeviceFeatures {
    pub raw: u64,
}

impl DeviceFeatures {
    /// Parse AirPlay `features` TXT (`0xLOW,0xHIGH` or single hex value).
    pub fn from_hex(hex: &str) -> Self {
        let s = hex.trim();
        if let Some((lo, hi)) = s.split_once(',') {
            let lo = parse_hex_u64(lo);
            let hi = parse_hex_u64(hi);
            return Self {
                raw: (hi << 32) | lo,
            };
        }
        Self {
            raw: parse_hex_u64(s),
        }
    }

    pub fn supports_screen_mirroring(&self) -> bool {
        const FEATURE_SCREEN: u64 = 1 << 8;
        self.raw & FEATURE_SCREEN != 0 || self.raw & 0x80 != 0
    }

    /// FairPlay SAP 2.5 — modern Apple receivers buffer at very low latency.
    pub fn supports_fairplay_sap(&self) -> bool {
        const FEATURE_FP_SAP_25: u64 = 1 << 14;
        self.raw & FEATURE_FP_SAP_25 != 0
    }

    /// Minimum playout lead for receivers without robust jitter buffers.
    pub fn playout_latency_floor_ms(&self) -> u64 {
        if self.supports_fairplay_sap() { 0 } else { 500 }
    }

    /// Legacy `/info` receivers omit `supportedFormats`; feature bits 18-21
    /// describe the same 44.1 kHz stereo screen formats.
    pub fn fallback_screen_audio_formats(&self) -> u64 {
        let mut formats = 0;
        if self.raw & (1 << 18) != 0 {
            formats |= 0x0000_0800; // PCM
        }
        if self.raw & (1 << 19) != 0 {
            formats |= SCREEN_AUDIO_ALAC_44100_STEREO;
        }
        if self.raw & (1 << 20) != 0 {
            formats |= 0x0040_0000; // AAC-LC
        }
        if self.raw & (1 << 21) != 0 {
            formats |= SCREEN_AUDIO_AAC_ELD_44100_STEREO;
        }
        formats
    }
}

fn parse_hex_u64(s: &str) -> u64 {
    let s = s.trim().trim_start_matches("0x").trim_start_matches("0X");
    u64::from_str_radix(s, 16).unwrap_or(0)
}

/// A discovered or manually specified AirPlay receiver (Apple TV).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AirPlayDevice {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub device_id: String,
    pub model: Option<String>,
    pub features: DeviceFeatures,
    /// Primary display width from `/info` `displays[0]` (presentation size in codec header).
    pub display_width: Option<u32>,
    /// Primary display height from `/info` `displays[0]`.
    pub display_height: Option<u32>,
    /// `supportedFormats.screenStream` from `/info`, when advertised.
    #[serde(default)]
    pub screen_audio_formats: Option<u64>,
    pub pi: Option<String>,
    pub pk: Option<String>,
}

impl AirPlayDevice {
    pub fn address(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    pub fn base_url(&self) -> String {
        format!(
            "http://{}:{}",
            crate::debug_log::format_host_for_url(&self.host),
            self.port
        )
    }

    pub fn supports_screen_alac(&self) -> Option<bool> {
        self.screen_audio_formats
            .map(|formats| formats & SCREEN_AUDIO_ALAC_44100_STEREO != 0)
    }

    pub fn supports_screen_aac_eld(&self) -> Option<bool> {
        self.screen_audio_formats
            .map(|formats| formats & SCREEN_AUDIO_AAC_ELD_44100_STEREO != 0)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DeviceFeatures, SCREEN_AUDIO_AAC_ELD_44100_STEREO, SCREEN_AUDIO_ALAC_44100_STEREO,
    };

    #[test]
    fn parses_comma_separated_features() {
        let f = DeviceFeatures::from_hex("0x527feec,0x0");
        assert_ne!(f.raw, 0);
    }

    #[test]
    fn derives_screen_audio_formats_from_legacy_feature_bits() {
        let features = DeviceFeatures {
            raw: (1 << 18) | (1 << 19) | (1 << 20) | (1 << 21),
        };
        let formats = features.fallback_screen_audio_formats();

        assert_ne!(formats & SCREEN_AUDIO_ALAC_44100_STEREO, 0);
        assert_ne!(formats & SCREEN_AUDIO_AAC_ELD_44100_STEREO, 0);
        assert_eq!(formats, 0x0144_0800);
    }
}
