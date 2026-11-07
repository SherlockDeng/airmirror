use rotten_core::config::DeviceCredentials;
use rotten_core::debug_log::agent_log;
use rotten_core::device::AirPlayDevice;
use rotten_core::error::{Result, RottenError};
use rotten_crypto::{pair_verify_step1, pair_verify_step2};
use tracing::debug;

use crate::airplay_conn::AirPlayRtspConn;

/// X25519 ECDH shared secret from pair-verify (needed for FairPlay + data-stream keys).
pub struct PairVerifyOutcome {
    pub shared_secret: [u8; 32],
}

/// Legacy raw pair-verify on a persistent RTSP connection (plaintext, required before fp-setup).
pub async fn pair_verify_conn(
    conn: &mut AirPlayRtspConn,
    device: &AirPlayDevice,
    creds: &DeviceCredentials,
) -> Result<PairVerifyOutcome> {
    let client_pk = key32(&creds.public_key, "client public key")?;
    let client_sk = key32(&creds.private_key, "client private key")?;
    let server_pk = resolve_server_public_key(creds, device)?;

    let (body1, eph_secret, eph_pk) = pair_verify_step1(&client_pk);

    debug!(host = %device.host, "pair-verify step 1");

    let (status1, bytes1) = conn.post_pair_verify("/pair-verify", &body1).await?;

    // #region agent log
    agent_log(
        "pair_verify.rs:pair_verify_conn",
        "pair-verify step1 response",
        "L",
        serde_json::json!({
            "httpStatus": status1,
            "bodyLen": bytes1.len(),
            "hasStoredServerPk": creds.server_public_key.len() == 32,
            "hasInfoPk": device.pk.is_some(),
            "sameSocket": true,
        }),
    );
    // #endregion

    if status1 != 200 {
        return Err(RottenError::Protocol(format!(
            "pair-verify step1 HTTP {status1}"
        )));
    }

    let (body2, shared_secret) =
        pair_verify_step2(&eph_secret, &eph_pk, &client_sk, &bytes1, &server_pk)
            .map_err(|e| RottenError::Protocol(e.to_string()))?;

    debug!(host = %device.host, "pair-verify step 2");

    let (status2, _) = conn.post_pair_verify("/pair-verify", &body2).await?;

    // #region agent log
    agent_log(
        "pair_verify.rs:pair_verify_conn",
        "pair-verify step2 response",
        "L",
        serde_json::json!({
            "httpStatus": status2,
            "sameSocket": true,
        }),
    );
    // #endregion

    if status2 != 200 {
        return Err(RottenError::Protocol(format!(
            "pair-verify step2 HTTP {status2}"
        )));
    }

    Ok(PairVerifyOutcome { shared_secret })
}

fn key32(bytes: &[u8], label: &str) -> Result<[u8; 32]> {
    bytes
        .try_into()
        .map_err(|_| RottenError::Protocol(format!("invalid {label} length")))
}

fn resolve_server_public_key(
    creds: &DeviceCredentials,
    device: &AirPlayDevice,
) -> Result<[u8; 32]> {
    if creds.server_public_key.len() == 32 {
        return creds
            .server_public_key
            .as_slice()
            .try_into()
            .map_err(|_| RottenError::Protocol("invalid stored server public key".into()));
    }
    device
        .pk
        .as_deref()
        .and_then(decode_info_pk)
        .ok_or_else(|| {
            RottenError::Protocol(
                "missing Apple TV public key for pair-verify — re-pair with `rottingapple pair --force`"
                    .into(),
            )
        })
}

fn decode_info_pk(pk: &str) -> Option<[u8; 32]> {
    use base64::Engine;
    if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(pk) {
        if bytes.len() == 32 {
            return bytes.try_into().ok();
        }
    }
    if pk.len() == 64 {
        if let Ok(bytes) = hex::decode(pk) {
            if bytes.len() == 32 {
                return bytes.try_into().ok();
            }
        }
    }
    None
}
