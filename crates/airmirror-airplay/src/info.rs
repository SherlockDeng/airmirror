use std::{
    collections::BTreeMap,
    io::{Cursor, Read, Write},
    net::{IpAddr, SocketAddr, TcpStream},
    time::Duration,
};

use airmirror_core::{Receiver, ReceiverCapabilities};
use plist::Value;
use serde::{Deserialize, Serialize};
use thiserror::Error;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum InfoError {
    #[error("network error while contacting {address}: {source}")]
    Network {
        address: SocketAddr,
        #[source]
        source: std::io::Error,
    },

    #[error("invalid AirPlay response: {0}")]
    InvalidResponse(String),

    #[error("AirPlay returned status {status}: {reason}")]
    HttpStatus { status: u16, reason: String },

    #[error("invalid binary plist: {0}")]
    Plist(#[from] plist::Error),

    #[error("the /info response did not contain a plist dictionary")]
    MissingDictionary,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AirPlayInfo {
    pub name: String,
    pub model: String,
    pub device_id: String,
    pub pi: Option<String>,
    pub pk: Option<String>,
    pub display_width: Option<u32>,
    pub display_height: Option<u32>,
    pub features: Option<u64>,
    pub status_flags: Option<u64>,
    pub screen_audio_formats: Option<u64>,
    pub protocol_version: Option<String>,
    pub source_version: Option<String>,
    pub os_build_version: Option<String>,
    pub raw_keys: Vec<String>,
}

impl AirPlayInfo {
    pub fn supports_screen_mirroring(&self) -> Option<bool> {
        self.features
            .map(|features| features & airmirror_core::FEATURE_SCREEN_MIRRORING != 0)
    }

    pub fn supports_audio(&self) -> Option<bool> {
        self.features
            .map(|features| features & airmirror_core::FEATURE_AUDIO != 0)
    }

    pub fn to_receiver(&self, host: IpAddr, port: u16) -> Receiver {
        let id = if self.device_id.is_empty() {
            format!("{host}:{port}")
        } else {
            self.device_id.to_ascii_uppercase()
        };
        Receiver {
            id,
            name: if self.name.is_empty() {
                format!("AirPlay {host}")
            } else {
                self.name.clone()
            },
            model: self.model.clone(),
            host,
            port,
            addresses: vec![host],
            capabilities: ReceiverCapabilities::from_features(self.features),
            source_version: self.source_version.clone(),
            protocol_version: self.protocol_version.clone(),
            txt: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct AirPlayInfoClient {
    pub timeout: Duration,
}

impl Default for AirPlayInfoClient {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_TIMEOUT,
        }
    }
}

impl AirPlayInfoClient {
    pub fn with_timeout(timeout: Duration) -> Self {
        Self { timeout }
    }

    pub fn probe(&self, host: IpAddr, port: u16) -> Result<AirPlayInfo, InfoError> {
        let address = SocketAddr::new(host, port);
        tracing::debug!(%address, "requesting AirPlay /info");
        let mut stream = TcpStream::connect_timeout(&address, self.timeout)
            .map_err(|source| InfoError::Network { address, source })?;
        stream
            .set_read_timeout(Some(self.timeout))
            .and_then(|_| stream.set_write_timeout(Some(self.timeout)))
            .map_err(|source| InfoError::Network { address, source })?;

        let request = concat!(
            "GET /info RTSP/1.0\r\n",
            "CSeq: 1\r\n",
            "User-Agent: AirMirror/0.1\r\n",
            "Content-Length: 0\r\n",
            "\r\n",
        );
        stream
            .write_all(request.as_bytes())
            .map_err(|source| InfoError::Network { address, source })?;

        let response =
            read_response(&mut stream).map_err(|source| InfoError::Network { address, source })?;
        let parsed = parse_response(&response)?;
        parse_info(&parsed.body)
    }
}

pub fn probe(host: IpAddr, port: u16) -> Result<AirPlayInfo, InfoError> {
    AirPlayInfoClient::default().probe(host, port)
}

#[derive(Debug, PartialEq, Eq)]
struct ParsedResponse {
    status: u16,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

fn read_response(stream: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    let mut response = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                response.extend_from_slice(&buffer[..read]);
                if response.len() > MAX_RESPONSE_BYTES {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "AirPlay response exceeds 4 MiB",
                    ));
                }
                if response_is_complete(&response) {
                    break;
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                if response_is_complete(&response) || !response.is_empty() {
                    break;
                }
                return Err(error);
            }
            Err(error) => return Err(error),
        }
    }
    Ok(response)
}

fn response_is_complete(response: &[u8]) -> bool {
    let Some(header_end) = find_header_end(response) else {
        return false;
    };
    let Ok(headers) = std::str::from_utf8(&response[..header_end]) else {
        return false;
    };
    let content_length = headers.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("content-length")
            .then(|| value.trim().parse::<usize>().ok())
            .flatten()
    });
    match content_length {
        Some(length) => response.len() >= header_end + 4 + length,
        None => false,
    }
}

fn parse_response(response: &[u8]) -> Result<ParsedResponse, InfoError> {
    let header_end = find_header_end(response)
        .ok_or_else(|| InfoError::InvalidResponse("missing header terminator".into()))?;
    let header_text = std::str::from_utf8(&response[..header_end])
        .map_err(|_| InfoError::InvalidResponse("headers are not UTF-8".into()))?;
    let mut lines = header_text.split("\r\n");
    let status_line = lines
        .next()
        .ok_or_else(|| InfoError::InvalidResponse("missing status line".into()))?;
    let mut status_parts = status_line.splitn(3, ' ');
    let protocol = status_parts.next().unwrap_or_default();
    if protocol != "RTSP/1.0" && protocol != "HTTP/1.1" && protocol != "HTTP/1.0" {
        return Err(InfoError::InvalidResponse(format!(
            "unexpected response protocol {protocol:?}"
        )));
    }
    let status = status_parts
        .next()
        .and_then(|value| value.parse::<u16>().ok())
        .ok_or_else(|| InfoError::InvalidResponse("missing status code".into()))?;
    let reason = status_parts.next().unwrap_or_default().to_owned();
    let headers: BTreeMap<_, _> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect();

    if status != 200 {
        return Err(InfoError::HttpStatus { status, reason });
    }

    let body_start = header_end + 4;
    let declared_length = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok());
    let body = match declared_length {
        Some(length) if response.len() >= body_start + length => {
            response[body_start..body_start + length].to_vec()
        }
        Some(length) => {
            return Err(InfoError::InvalidResponse(format!(
                "body is truncated: expected {length} bytes, got {}",
                response.len().saturating_sub(body_start)
            )));
        }
        None => response[body_start..].to_vec(),
    };

    Ok(ParsedResponse {
        status,
        headers,
        body,
    })
}

fn parse_info(body: &[u8]) -> Result<AirPlayInfo, InfoError> {
    let value = Value::from_reader(Cursor::new(body))?;
    let dictionary = value.as_dictionary().ok_or(InfoError::MissingDictionary)?;
    let mut raw_keys: Vec<_> = dictionary.keys().cloned().collect();
    raw_keys.sort();
    let features = integer_value(dictionary, "features");
    let screen_audio_formats = supported_formats_screen_stream(dictionary).or_else(|| {
        features
            .map(feature_screen_audio_formats)
            .filter(|formats| *formats != 0)
    });

    Ok(AirPlayInfo {
        name: string_value(dictionary, "name"),
        model: string_value(dictionary, "model"),
        device_id: string_value(dictionary, "deviceID"),
        pi: optional_string_value(dictionary, "pi"),
        pk: public_key_value(dictionary),
        display_width: display_dimension(dictionary, "widthPixels", "width"),
        display_height: display_dimension(dictionary, "heightPixels", "height"),
        features,
        status_flags: integer_value(dictionary, "statusFlags"),
        screen_audio_formats,
        protocol_version: optional_string_value(dictionary, "protocolVersion"),
        source_version: optional_string_value(dictionary, "sourceVersion"),
        os_build_version: optional_string_value(dictionary, "osBuildVersion"),
        raw_keys,
    })
}

fn feature_screen_audio_formats(features: u64) -> u64 {
    let mut formats = 0;
    if features & (1 << 18) != 0 {
        formats |= 0x0000_0800;
    }
    if features & (1 << 19) != 0 {
        formats |= 0x0004_0000;
    }
    if features & (1 << 20) != 0 {
        formats |= 0x0040_0000;
    }
    if features & (1 << 21) != 0 {
        formats |= 0x0100_0000;
    }
    formats
}

fn string_value(dictionary: &plist::Dictionary, key: &str) -> String {
    dictionary
        .get(key)
        .and_then(Value::as_string)
        .unwrap_or_default()
        .to_owned()
}

fn optional_string_value(dictionary: &plist::Dictionary, key: &str) -> Option<String> {
    dictionary
        .get(key)
        .and_then(Value::as_string)
        .map(str::to_owned)
}

fn integer_value(dictionary: &plist::Dictionary, key: &str) -> Option<u64> {
    dictionary.get(key).and_then(|value| {
        value.as_unsigned_integer().or_else(|| {
            value
                .as_signed_integer()
                .and_then(|value| u64::try_from(value).ok())
        })
    })
}

fn supported_formats_screen_stream(dictionary: &plist::Dictionary) -> Option<u64> {
    dictionary
        .get("supportedFormats")?
        .as_dictionary()?
        .get("screenStream")
        .and_then(|value| {
            value.as_unsigned_integer().or_else(|| {
                value
                    .as_signed_integer()
                    .and_then(|value| u64::try_from(value).ok())
            })
        })
}

fn public_key_value(dictionary: &plist::Dictionary) -> Option<String> {
    let value = dictionary.get("pk")?;
    if let Some(value) = value.as_string() {
        return Some(value.to_owned());
    }
    if let Some(value) = value.as_data() {
        return Some(hex::encode(value));
    }
    None
}

fn display_dimension(
    dictionary: &plist::Dictionary,
    primary_key: &str,
    fallback_key: &str,
) -> Option<u32> {
    let display = dictionary
        .get("displays")?
        .as_array()?
        .first()?
        .as_dictionary()?;
    display
        .get(primary_key)
        .or_else(|| display.get(fallback_key))
        .and_then(|value| {
            value.as_unsigned_integer().or_else(|| {
                value
                    .as_signed_integer()
                    .and_then(|v| u64::try_from(v).ok())
            })
        })
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| *value > 0)
}

fn find_header_end(response: &[u8]) -> Option<usize> {
    response.windows(4).position(|window| window == b"\r\n\r\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rtsp_response() {
        let response = b"RTSP/1.0 200 OK\r\nContent-Length: 4\r\n\r\ntest";
        let parsed = parse_response(response).unwrap();
        assert_eq!(parsed.status, 200);
        assert_eq!(parsed.headers.get("content-length"), Some(&"4".to_owned()));
        assert_eq!(parsed.body, b"test");
    }

    #[test]
    fn rejects_non_success_status() {
        let response = b"RTSP/1.0 403 Forbidden\r\nContent-Length: 0\r\n\r\n";
        assert!(matches!(
            parse_response(response),
            Err(InfoError::HttpStatus { status: 403, .. })
        ));
    }

    #[test]
    fn detects_complete_response() {
        let response = b"RTSP/1.0 200 OK\r\nContent-Length: 4\r\n\r\ntest";
        assert!(response_is_complete(response));
        assert!(!response_is_complete(&response[..response.len() - 1]));
    }

    #[test]
    fn parses_binary_plist_info() {
        let mut dictionary = plist::Dictionary::new();
        dictionary.insert("name".into(), Value::String("Test TV".into()));
        dictionary.insert("model".into(), Value::String("AppleTV14,1".into()));
        dictionary.insert("deviceID".into(), Value::String("AA:BB:CC".into()));
        dictionary.insert("features".into(), Value::from((1_u64 << 7) | (1_u64 << 9)));
        let mut formats = plist::Dictionary::new();
        formats.insert("screenStream".into(), Value::from(0x0004_0000_u64));
        dictionary.insert(
            "supportedFormats".into(),
            Value::Dictionary(formats),
        );
        let mut bytes = Vec::new();
        Value::Dictionary(dictionary)
            .to_writer_binary(&mut bytes)
            .unwrap();

        let info = parse_info(&bytes).unwrap();
        assert_eq!(info.name, "Test TV");
        assert_eq!(info.supports_screen_mirroring(), Some(true));
        assert_eq!(info.supports_audio(), Some(true));
        assert_eq!(info.screen_audio_formats, Some(0x0004_0000));
    }

    #[test]
    fn derives_screen_audio_formats_when_dictionary_is_absent() {
        let mut dictionary = plist::Dictionary::new();
        dictionary.insert("features".into(), Value::from((1_u64 << 19) | (1_u64 << 21)));
        let mut bytes = Vec::new();
        Value::Dictionary(dictionary)
            .to_writer_binary(&mut bytes)
            .unwrap();

        let info = parse_info(&bytes).unwrap();
        assert_eq!(info.screen_audio_formats, Some(0x0104_0000));
    }
}
