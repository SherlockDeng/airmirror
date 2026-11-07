use std::{collections::BTreeMap, net::IpAddr};

use serde::{Deserialize, Serialize};

pub const FEATURE_SCREEN_MIRRORING: u64 = 1 << 7;
pub const FEATURE_AUDIO: u64 = 1 << 9;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReceiverCapabilities {
    pub features: Option<u64>,
    pub screen_mirroring: Option<bool>,
    pub audio: Option<bool>,
}

impl ReceiverCapabilities {
    pub fn from_features(features: Option<u64>) -> Self {
        Self {
            features,
            screen_mirroring: features.map(|value| value & FEATURE_SCREEN_MIRRORING != 0),
            audio: features.map(|value| value & FEATURE_AUDIO != 0),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receiver {
    pub id: String,
    pub name: String,
    pub model: String,
    pub host: IpAddr,
    pub port: u16,
    pub addresses: Vec<IpAddr>,
    pub capabilities: ReceiverCapabilities,
    pub source_version: Option<String>,
    pub protocol_version: Option<String>,
    pub txt: BTreeMap<String, String>,
}

impl Receiver {
    pub fn supports_screen_mirroring(&self) -> bool {
        self.capabilities.screen_mirroring.unwrap_or(true)
    }

    pub fn display_address(&self) -> String {
        match self.host {
            IpAddr::V4(address) => format!("{address}:{}", self.port),
            IpAddr::V6(address) => format!("[{address}]:{}", self.port),
        }
    }
}

/// Parses the AirPlay `features` TXT value, for example `0x5A7FFFF7,0x1E`.
///
/// The first value contains bits 0-31 and the optional second value contains
/// bits 32-63. A single value is accepted because older receivers publish only
/// the low word.
pub fn parse_features(value: &str) -> Option<u64> {
    let mut parts = value.split(',');
    let low = parse_hex_word(parts.next()?)?;
    let high = match parts.next() {
        Some(value) => parse_hex_word(value)?,
        None => 0,
    };
    if parts.next().is_some() {
        return None;
    }
    Some(low | (high << 32))
}

fn parse_hex_word(value: &str) -> Option<u64> {
    let value = value.trim();
    let value = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
        .unwrap_or(value);
    u64::from_str_radix(value, 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_split_feature_words() {
        assert_eq!(parse_features("0x5A7FFFF7,0x1E"), Some(0x1E_5A7F_FFF7));
        assert_eq!(parse_features("80"), Some(0x80));
    }

    #[test]
    fn rejects_malformed_feature_masks() {
        assert_eq!(parse_features("0x10,0x20,0x30"), None);
        assert_eq!(parse_features("not-hex"), None);
    }

    #[test]
    fn detects_screen_and_audio_capabilities() {
        let capabilities = ReceiverCapabilities::from_features(Some((1 << 7) | (1 << 9)));
        assert_eq!(capabilities.screen_mirroring, Some(true));
        assert_eq!(capabilities.audio, Some(true));
    }
}
