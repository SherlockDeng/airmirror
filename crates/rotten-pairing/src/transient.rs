use base64::Engine;
use rotten_core::config::DeviceCredentials;
use rotten_core::device::AirPlayDevice;
use rotten_core::error::{Result, RottenError};
use rotten_crypto::generate_ed25519_keypair;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const AIRPLAY_USER_AGENT: &str = "AirPlay/320.20";

/// Original AirPlay transient pair-setup.
///
/// The request must use RTSP framing on the AirPlay control connection. A
/// plain HTTP POST is accepted by some receivers but does not establish an
/// authorized mirroring identity.
pub async fn pair_transient(device: &AirPlayDevice) -> Result<DeviceCredentials> {
    let keypair = generate_ed25519_keypair();
    let (status, body) = raw_pair_setup_rtsp(device, &keypair.public_key).await?;

    let server_public_key = if status == 200 && body.len() == 32 {
        body
    } else if status == 200 && body.is_empty() {
        decode_advertised_public_key(device.pk.as_deref()).ok_or_else(|| {
            RottenError::Pairing(
                "receiver returned an empty pair-setup response and advertised no valid pk".into(),
            )
        })?
    } else {
        return Err(RottenError::Pairing(format!(
            "transient pair-setup RTSP {status} returned {} bytes, expected 32 or an advertised pk",
            body.len()
        )));
    };

    Ok(DeviceCredentials {
        device_id: device.device_id.clone(),
        identifier: hex::encode(keypair.public_key),
        public_key: keypair.public_key.to_vec(),
        private_key: keypair.private_key.to_vec(),
        server_public_key,
    })
}

async fn raw_pair_setup_rtsp(
    device: &AirPlayDevice,
    public_key: &[u8; 32],
) -> Result<(u16, Vec<u8>)> {
    let addr = device.address();
    let mut stream =
        tokio::time::timeout(std::time::Duration::from_secs(5), TcpStream::connect(&addr))
            .await
            .map_err(|_| RottenError::Pairing(format!("pair-setup connect timeout: {addr}")))?
            .map_err(|error| RottenError::Pairing(format!("pair-setup connect {addr}: {error}")))?;

    let headers = format!(
        "POST /pair-setup RTSP/1.0\r\n\
         CSeq: 1\r\n\
         User-Agent: {AIRPLAY_USER_AGENT}\r\n\
         Content-Type: application/octet-stream\r\n\
         Content-Length: {}\r\n\r\n",
        public_key.len()
    );
    stream
        .write_all(headers.as_bytes())
        .await
        .map_err(|error| RottenError::Pairing(format!("pair-setup write headers: {error}")))?;
    stream
        .write_all(public_key)
        .await
        .map_err(|error| RottenError::Pairing(format!("pair-setup write key: {error}")))?;

    let mut response = Vec::new();
    let mut scratch = [0u8; 4096];
    let mut header_end = None;
    let mut content_length = 0usize;
    loop {
        let read = stream
            .read(&mut scratch)
            .await
            .map_err(|error| RottenError::Pairing(format!("pair-setup read: {error}")))?;
        if read == 0 {
            break;
        }
        response.extend_from_slice(&scratch[..read]);
        if header_end.is_none()
            && let Some(position) = response.windows(4).position(|window| window == b"\r\n\r\n")
        {
            let end = position + 4;
            header_end = Some(end);
            let header = String::from_utf8_lossy(&response[..end]);
            content_length = parse_content_length(&header);
        }
        if let Some(end) = header_end
            && response.len() >= end + content_length
        {
            break;
        }
        if response.len() > 64 * 1024 {
            return Err(RottenError::Pairing(
                "pair-setup response exceeded 64 KiB".into(),
            ));
        }
    }

    let end = header_end
        .ok_or_else(|| RottenError::Pairing("pair-setup response headers missing".into()))?;
    let header = String::from_utf8_lossy(&response[..end]);
    let status = header
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse::<u16>().ok())
        .ok_or_else(|| RottenError::Pairing("invalid pair-setup status line".into()))?;
    let available = response.len().saturating_sub(end);
    let body = response[end..end + content_length.min(available)].to_vec();
    Ok((status, body))
}

fn parse_content_length(headers: &str) -> usize {
    headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())
                .flatten()
        })
        .unwrap_or(0)
}

fn decode_advertised_public_key(value: Option<&str>) -> Option<Vec<u8>> {
    let value = value?.trim();
    if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(value) {
        if bytes.len() == 32 {
            return Some(bytes);
        }
    }
    if value.len() == 64 {
        if let Ok(bytes) = hex::decode(value) {
            if bytes.len() == 32 {
                return Some(bytes);
            }
        }
    }
    None
}
