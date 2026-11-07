use rotten_core::config::DeviceCredentials;
use rotten_core::debug_log::agent_log;
use rotten_core::error::{Result, RottenError};
use rotten_crypto::FairPlaySession;
use tracing::debug;

use crate::airplay_conn::AirPlayRtspConn;

/// FairPlay fp-setup on the same RTSP connection as pair-verify (steps 1→2, CSeq continues).
pub async fn run_fp_setup_conn(
    conn: &mut AirPlayRtspConn,
    creds: &DeviceCredentials,
) -> Result<FairPlaySession> {
    let request_mode = FairPlaySession::fp_setup_mode();

    // #region agent log
    agent_log(
        "fp_setup.rs:run_fp_setup_conn",
        "fp-setup starting",
        "O",
        serde_json::json!({
            "flowOrder": "same-rtsp-socket-after-pair-verify",
            "requestMode": request_mode,
            "dacpIdPrefix": &airplay_remote_ids(creds).0[..8],
        }),
    );
    // #endregion

    debug!(request_mode, "fp-setup step 1");

    let step1 = FairPlaySession::fp_setup1_request();
    let (status1, body1) = conn.post_fp_setup(step1).await?;

    let response_hex_prefix: String = body1.iter().take(32).map(|b| format!("{b:02x}")).collect();
    // #region agent log
    agent_log(
        "fp_setup.rs:run_fp_setup_conn",
        "fp-setup step1 response",
        "N",
        serde_json::json!({
            "httpStatus": status1,
            "bodyLen": body1.len(),
            "requestMode": request_mode,
            "responseByte12": body1.get(12).copied().unwrap_or(0),
            "responseByte13": body1.get(13).copied().unwrap_or(0),
            "responseByte14": body1.get(14).copied().unwrap_or(0),
            "responseHexPrefix": response_hex_prefix,
            "fply": body1.len() >= 4 && &body1[0..4] == b"FPLY",
        }),
    );
    // #endregion

    if status1 != 200 || body1.len() != 142 {
        return Err(RottenError::Protocol(format!(
            "fp-setup step1 failed: HTTP {} len {}",
            status1,
            body1.len()
        )));
    }

    let receiver_sap = FairPlaySession::receiver_sap_from_setup1(&body1)?;
    let key_message = FairPlaySession::key_message_from_setup1(&body1, request_mode)?;
    let mut local_sap = [0u8; 128];
    fpsap_rs::fairplay_message::decrypt_fairplay_message(&key_message, &mut local_sap);
    let expected_tail = fpsap_rs::fpsap::fpsap_exchange_for_sap(&local_sap, &receiver_sap);
    let tail_matches = expected_tail == key_message[144..];
    // #region agent log
    agent_log(
        "fp_setup.rs:run_fp_setup_conn",
        "FairPlay SAP consistency",
        "H128",
        serde_json::json!({
            "tailMatches": tail_matches,
            "receiverSapPrefix": hex::encode(&receiver_sap[..8]),
            "localSapPrefix": hex::encode(&local_sap[..8]),
        }),
    );
    // #endregion
    if !tail_matches {
        return Err(RottenError::Crypto(
            "FairPlay m2/m3 SAP exchange is inconsistent".into(),
        ));
    }
    let key_msg_prefix: String = key_message
        .iter()
        .take(16)
        .map(|b| format!("{b:02x}"))
        .collect();
    let key_msg_mid: String = key_message
        .iter()
        .skip(16)
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect();
    let key_msg_tail: String = key_message
        .iter()
        .skip(144)
        .map(|b| format!("{b:02x}"))
        .collect();

    debug!(request_mode, "fp-setup step 2");

    // #region agent log
    agent_log(
        "fp_setup.rs:run_fp_setup_conn",
        "fp-setup step2 request",
        "P",
        serde_json::json!({
            "keyMsgMode": request_mode,
            "keyMsgSource": "m3Prefix+fpsap-hash",
            "keyMsgPrefix": key_msg_prefix,
            "keyMsgBytes16to23": key_msg_mid,
            "keyMsgTail": key_msg_tail,
        }),
    );
    // #endregion

    let (status2, body2) = conn.post_fp_setup(&key_message).await?;

    // #region agent log
    agent_log(
        "fp_setup.rs:run_fp_setup_conn",
        "fp-setup step2 response",
        "N",
        serde_json::json!({
            "httpStatus": status2,
            "bodyLen": body2.len(),
            "keyMsgMode": request_mode,
        }),
    );
    // #endregion

    if status2 != 200 || body2.len() != 32 {
        return Err(RottenError::Protocol(format!(
            "fp-setup step2 failed: HTTP {} len {}",
            status2,
            body2.len()
        )));
    }

    Ok(FairPlaySession::from_key_message(
        key_message,
        request_mode,
        receiver_sap,
    ))
}

pub(crate) fn airplay_remote_ids(creds: &DeviceCredentials) -> (String, u32) {
    let hex: String = creds
        .identifier
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .collect();
    let dacp = if hex.len() >= 16 {
        hex[..16].to_string()
    } else {
        format!("{:0>16}", hex)
    };
    let active_remote =
        u32::from_str_radix(&hex.chars().take(8).collect::<String>(), 16).unwrap_or(0x4a7f_9c12);
    (dacp, active_remote)
}
