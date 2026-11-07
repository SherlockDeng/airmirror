//! RTSP mirror setup on port 7000 (doubletake-style), after pair-verify + fp-setup.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use plist::Value;
use rand::RngCore;
use rotten_core::config::{DeviceCredentials, MirrorCipherMode};
use rotten_core::debug_log::agent_log;
use rotten_core::device::AirPlayDevice;
use rotten_core::error::{Result, RottenError};
use rotten_core::media::MediaSyncConfig;
use rotten_crypto::{FairPlaySession, MirrorFpKeys, MirrorVideoCrypto};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::task::JoinHandle;
use tracing::{debug, info};

use crate::airplay_conn::AirPlayRtspConn;
use crate::audio_rtp::{MirrorAudioCipher, MirrorAudioSetup, plist_audio_ports};
use crate::fp_setup::airplay_remote_ids;
use crate::ntp::ntp_boot_with_epoch;
use crate::pair_verify::PairVerifyOutcome;

/// Resources that must stay alive for the mirror session (timing UDP, event TCP listener).
pub struct MirrorRtspResources {
    _timing_task: JoinHandle<()>,
    _event_task: JoinHandle<()>,
    _udp_sockets: Vec<UdpSocket>,
    _event_listener: Arc<TcpListener>,
    _receiver_event_task: Option<JoinHandle<()>>,
}

/// Result of RTSP mirror negotiation.
pub struct MirrorRtspSetup {
    pub video_crypto: MirrorVideoCrypto,
    pub data_port: u16,
    pub event_port: Option<u16>,
    pub timing_port: u16,
    pub session_uuid: String,
    pub control_uri: String,
    pub data_stream: TcpStream,
    pub audio: Option<MirrorAudioSetup>,
    pub resources: MirrorRtspResources,
}

pub async fn setup_mirror_rtsp(
    conn: &mut AirPlayRtspConn,
    device: &AirPlayDevice,
    creds: &DeviceCredentials,
    fp: &FairPlaySession,
    pv: &PairVerifyOutcome,
    no_encrypt: bool,
    cipher_mode: MirrorCipherMode,
    sync: MediaSyncConfig,
) -> Result<MirrorRtspSetup> {
    rotten_core::ntp::init_session_clock();

    // Receivers without feature bit 27 mix the FairPlay key with the raw
    // pair-verify shared secret before deriving both audio and video keys.
    let mix_pair_verify_key =
        uses_legacy_fairplay_audio(device) && device.features.raw & (1 << 27) == 0;
    let fp_keys = fp
        .derive_mirror_fp_keys(&pv.shared_secret, mix_pair_verify_key)
        .map_err(|e| {
            // #region agent log
            agent_log(
                "mirror_rtsp.rs:setup_mirror_rtsp",
                "FairPlay mirror key derivation failed",
                "Z",
                serde_json::json!({ "error": e.to_string() }),
            );
            // #endregion
            e
        })?;

    // #region agent log
    agent_log(
        "mirror_rtsp.rs:setup_mirror_rtsp",
        "FairPlay mirror keys derived",
        "Z",
        serde_json::json!({
            "ekeyLen": fp_keys.ekey.len(),
            "fpKeyPrefix": hex::encode(&fp_keys.fp_key[..4]),
        }),
    );
    // #endregion

    let (dacp_id, active_remote) = airplay_remote_ids(creds);
    let session_uuid = uuid::Uuid::new_v4().to_string();
    let device_id = mac_to_device_id_int(&creds.identifier);
    let audio_stream_id = stream_connection_id();
    let video_stream_id = stream_connection_id();

    let ntp_probed = Arc::new(AtomicBool::new(false));
    let event_connected = Arc::new(AtomicBool::new(false));

    let mut udp_sockets = bind_consecutive_udp(3).await?;
    let timing_port = udp_sockets[0]
        .local_addr()
        .map_err(|e| RottenError::Protocol(e.to_string()))?
        .port();
    let audio_control_port = udp_sockets[1]
        .local_addr()
        .map_err(|e| RottenError::Protocol(e.to_string()))?
        .port();
    let audio_data_port = udp_sockets[2]
        .local_addr()
        .map_err(|e| RottenError::Protocol(e.to_string()))?
        .port();

    // #region agent log
    agent_log(
        "mirror_rtsp.rs:setup_mirror_rtsp",
        "local UDP port triple",
        "H78",
        serde_json::json!({
            "timingPort": timing_port,
            "audioControlPort": audio_control_port,
            "audioDataPort": audio_data_port,
        }),
    );
    // #endregion

    let session_latency_samples = if uses_legacy_fairplay_audio(device) {
        0
    } else {
        sync.audio_latency_samples()
    };
    // #region agent log
    agent_log(
        "mirror_rtsp.rs:setup_mirror_rtsp",
        "session playout latency",
        "H85",
        serde_json::json!({
            "latencySamples": session_latency_samples,
            "audioOffsetMs": sync.audio_offset_ms,
            "videoLeadMs": sync.video_lead().as_millis(),
            "deviceFeatures": format!("0x{:x}", device.features.raw),
            "fairplaySap": device.features.supports_fairplay_sap(),
            "legacyPairingFeature": device.features.raw & (1 << 27) != 0,
        }),
    );
    // #endregion

    let timing_sock = udp_sockets.remove(0);
    let ntp_flag = ntp_probed.clone();
    let timing_task = tokio::spawn(ntp_timing_responder(timing_sock, ntp_flag));

    let (event_listener, local_event_port, event_bind_strategy) =
        bind_event_listener(timing_port).await?;
    let event_listener = Arc::new(event_listener);
    debug!(
        local_event_port,
        timing_port, audio_control_port, "mirror RTSP local ports ready"
    );

    // #region agent log
    agent_log(
        "mirror_rtsp.rs:setup_mirror_rtsp",
        "event TCP listener bound",
        "H96",
        serde_json::json!({
            "localEventPort": local_event_port,
            "timingPort": timing_port,
            "bindStrategy": event_bind_strategy,
        }),
    );
    // #endregion

    let event_listener_accept = event_listener.clone();
    let event_flag = event_connected.clone();
    let event_task = tokio::spawn(async move {
        loop {
            match event_listener_accept.accept().await {
                Ok((conn, peer)) => {
                    event_flag.store(true, Ordering::Relaxed);
                    // #region agent log
                    agent_log(
                        "mirror_rtsp.rs:event",
                        "Apple TV event channel connected",
                        "AB",
                        serde_json::json!({ "peer": peer.to_string() }),
                    );
                    // #endregion
                    tokio::spawn(async move {
                        let mut conn = conn;
                        let mut buf = [0u8; 4096];
                        loop {
                            match conn.read(&mut buf).await {
                                Ok(0) | Err(_) => break,
                                Ok(n) => {
                                    // #region agent log
                                    agent_log(
                                        "mirror_rtsp.rs:event",
                                        "inbound event channel bytes",
                                        "H96",
                                        serde_json::json!({
                                            "bytes": n,
                                            "prefix": hex::encode(&buf[..n.min(32)]),
                                        }),
                                    );
                                    // #endregion
                                }
                            }
                        }
                    });
                }
                Err(_) => break,
            }
        }
    });

    let host = &device.host;
    let rtsp_port = device.port;
    let audio_uri = format!("rtsp://{host}:{rtsp_port}/{audio_stream_id}");

    if device.supports_screen_alac() == Some(false) {
        let aac_eld = device.supports_screen_aac_eld() == Some(true);
        return Err(RottenError::Protocol(if aac_eld {
            "receiver advertises AAC-ELD but not ALAC; AAC-ELD encoding is not implemented yet"
                .into()
        } else {
            format!(
                "receiver screenStream formats 0x{:x} do not include ALAC 0x40000",
                device.screen_audio_formats.unwrap_or_default()
            )
        }));
    }

    let legacy_fairplay_audio = uses_legacy_fairplay_audio(device);
    let env_flag = |name: &str| {
        std::env::var(name)
            .map(|value| matches!(value.as_str(), "1" | "true" | "yes"))
            .unwrap_or(false)
    };
    let legacy_pcm_audio = legacy_fairplay_audio && env_flag("AIRMIRROR_AUDIO_PCM");
    let legacy_plaintext_audio =
        legacy_fairplay_audio && (env_flag("AIRMIRROR_AUDIO_PLAINTEXT") || legacy_pcm_audio);
    let legacy_audio_format = std::env::var("AIRMIRROR_AUDIO_FORMAT")
        .ok()
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(if legacy_pcm_audio { 0x800 } else { 0x40000 });
    let legacy_audio_ct = std::env::var("AIRMIRROR_AUDIO_CT")
        .ok()
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(if legacy_pcm_audio { 1 } else { 2 });
    let legacy_audio_spf = std::env::var("AIRMIRROR_AUDIO_SPF")
        .ok()
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(352);
    let audio_chacha_key = if legacy_fairplay_audio {
        None
    } else {
        Some(random_chacha_key())
    };
    let audio_cipher = if legacy_plaintext_audio {
        MirrorAudioCipher::Plaintext
    } else if legacy_fairplay_audio {
        MirrorAudioCipher::FairPlayAesCbc {
            key: fp_keys.stream_key,
            iv: fp_keys.fp_iv,
        }
    } else {
        MirrorAudioCipher::ChaCha20Poly1305(
            audio_chacha_key.expect("modern audio key must be generated"),
        )
    };
    let audio_body = if legacy_fairplay_audio {
        encode_audio_setup_plist_legacy_aes(
            device_id,
            &session_uuid,
            timing_port,
            audio_stream_id,
            audio_control_port,
            &fp_keys.ekey,
            &fp_keys.fp_iv,
            session_latency_samples,
            !legacy_plaintext_audio,
            legacy_audio_format,
            legacy_audio_ct,
            legacy_audio_spf,
        )?
    } else {
        encode_audio_setup_plist_chacha(
            device_id,
            &session_uuid,
            timing_port,
            audio_stream_id,
            audio_control_port,
            &audio_chacha_key.expect("modern audio key must be generated"),
            session_latency_samples,
        )?
    };

    let video_uri = format!("rtsp://{host}:{rtsp_port}/{video_stream_id}");
    let video_encrypt_keys = !no_encrypt;
    let include_fairplay_roots = video_encrypt_keys && !env_flag("AIRMIRROR_NO_ROOT_FAIRPLAY");
    let video_body = encode_video_setup_plist(
        device_id,
        &session_uuid,
        timing_port,
        video_stream_id,
        &fp_keys,
        video_encrypt_keys,
        include_fairplay_roots,
        sync.video_lead().as_millis() as i64,
    )?;

    let force_media_first = std::env::var("AIRMIRROR_SETUP_ORDER")
        .is_ok_and(|value| matches!(value.as_str(), "media-first" | "legacy"));
    let try_control_first = legacy_fairplay_audio && !force_media_first;
    let control_body = try_control_first
        .then(|| {
            encode_control_setup_plist(
                device_id,
                &session_uuid,
                timing_port,
                &fp_keys,
                !legacy_plaintext_audio,
            )
        })
        .transpose()?;
    let mut session_first = false;
    let mut control_resp = Vec::new();

    if let Some(control_body) = control_body.as_deref() {
        // #region agent log
        agent_log(
            "mirror_rtsp.rs:setup_mirror_rtsp",
            "control SETUP request",
            "AC",
            serde_json::json!({
                "uri": audio_uri,
                "bodyLen": control_body.len(),
                "timingPort": timing_port,
                "hasEkey": !legacy_plaintext_audio,
                "flow": "control-first",
            }),
        );
        // #endregion

        let (control_status, response) = conn
            .rtsp_setup(&audio_uri, control_body, &dacp_id, active_remote)
            .await?;

        // #region agent log
        agent_log(
            "mirror_rtsp.rs:setup_mirror_rtsp",
            "control SETUP response",
            "AC",
            serde_json::json!({
                "httpStatus": control_status,
                "bodyLen": response.len(),
                "ntpProbed": ntp_probed.load(Ordering::Relaxed),
                "eventConnected": event_connected.load(Ordering::Relaxed),
            }),
        );
        // #endregion

        if control_status == 200 {
            session_first = true;
            control_resp = response;
        } else if setup_order_rejected(control_status) {
            debug!(
                control_status,
                "receiver rejected control-first SETUP; using media-first compatibility flow"
            );
            // #region agent log
            agent_log(
                "mirror_rtsp.rs:setup_mirror_rtsp",
                "control-first rejected; falling back to media-first",
                "AC",
                serde_json::json!({ "httpStatus": control_status }),
            );
            // #endregion
        } else {
            return Err(setup_failed(
                control_status,
                "control",
                timing_port,
                local_event_port,
                ntp_probed.load(Ordering::Relaxed),
                event_connected.load(Ordering::Relaxed),
            ));
        }
    }

    let setup_flow = if session_first {
        "control-first"
    } else if try_control_first {
        "media-first-fallback"
    } else {
        "media-first"
    };
    let audio_request_body = if session_first {
        plist_stream_only(&audio_body)?
    } else {
        audio_body
    };

    // #region agent log
    agent_log(
        "mirror_rtsp.rs:setup_mirror_rtsp",
        "audio SETUP request",
        "AC",
        serde_json::json!({
            "uri": audio_uri,
            "bodyLen": audio_request_body.len(),
            "timingPort": timing_port,
            "localEventPort": local_event_port,
            "audioControlPort": audio_control_port,
            "hasShk": !legacy_fairplay_audio,
            "hasEkey": !session_first && legacy_fairplay_audio && !legacy_plaintext_audio,
            "audioStyle": if legacy_plaintext_audio {
                "plaintext"
            } else if legacy_fairplay_audio {
                "fairplay-aes-cbc"
            } else {
                "chacha-hap"
            },
            "mixPairVerifyKey": mix_pair_verify_key,
            "flow": setup_flow,
        }),
    );
    // #endregion

    let (audio_status, audio_resp) = conn
        .rtsp_setup(&audio_uri, &audio_request_body, &dacp_id, active_remote)
        .await?;

    // #region agent log
    agent_log(
        "mirror_rtsp.rs:setup_mirror_rtsp",
        "audio SETUP response",
        "AC",
        serde_json::json!({
            "httpStatus": audio_status,
            "bodyLen": audio_resp.len(),
            "ntpProbed": ntp_probed.load(Ordering::Relaxed),
            "eventConnected": event_connected.load(Ordering::Relaxed),
        }),
    );
    // #endregion

    if audio_status != 200 {
        let ntp = ntp_probed.load(Ordering::Relaxed);
        let event = event_connected.load(Ordering::Relaxed);
        // #region agent log
        agent_log(
            "mirror_rtsp.rs:setup_mirror_rtsp",
            "audio SETUP failed",
            "H123",
            serde_json::json!({
                "httpStatus": audio_status,
                "ntpProbed": ntp,
                "eventConnected": event,
                "timingPort": timing_port,
                "isWsl": rotten_core::running_in_wsl(),
            }),
        );
        // #endregion
        return Err(setup_failed(
            audio_status,
            "audio",
            timing_port,
            local_event_port,
            ntp,
            event,
        ));
    }

    let audio_ports = plist_audio_ports(&audio_resp);
    if let Ok(value) = plist::from_bytes::<Value>(&audio_resp) {
        debug!(response = ?value, "audio SETUP response plist");
    }
    // #region agent log
    agent_log(
        "mirror_rtsp.rs:setup_mirror_rtsp",
        "audio ports from SETUP response",
        "H61",
        serde_json::json!({
            "audioDataPort": audio_ports.map(|(d, _)| d),
            "audioControlPort": audio_ports.map(|(_, c)| c),
            "audioRespLen": audio_resp.len(),
        }),
    );
    // #endregion

    // #region agent log
    agent_log(
        "mirror_rtsp.rs:setup_mirror_rtsp",
        "video SETUP plist built",
        "H28",
        serde_json::json!({
            "videoEncryptKeys": video_encrypt_keys,
            "noEncrypt": no_encrypt,
            "bodyLen": video_body.len(),
        }),
    );
    // #endregion

    let video_request_body = if session_first {
        plist_stream_only(&video_body)?
    } else {
        video_body
    };
    let (video_status, video_resp) = conn
        .rtsp_setup(&video_uri, &video_request_body, &dacp_id, active_remote)
        .await?;

    // #region agent log
    agent_log(
        "mirror_rtsp.rs:setup_mirror_rtsp",
        "video SETUP response",
        "Y",
        serde_json::json!({
            "httpStatus": video_status,
            "bodyLen": video_resp.len(),
            "videoStreamId": video_stream_id,
        }),
    );
    // #endregion

    if video_status != 200 {
        return Err(setup_failed(
            video_status,
            "video",
            timing_port,
            local_event_port,
            ntp_probed.load(Ordering::Relaxed),
            event_connected.load(Ordering::Relaxed),
        ));
    }

    let video_resp_keys = plist_dict_keys(&video_resp);
    let (data_port, resp_stream_id) = plist_video_stream_info(&video_resp, 110)
        .ok_or_else(|| RottenError::Protocol("no video dataPort in SETUP response".into()))?;
    let hkdf_stream_id = resp_stream_id.unwrap_or(video_stream_id);

    // #region agent log
    agent_log(
        "mirror_rtsp.rs:setup_mirror_rtsp",
        "video SETUP response parsed",
        "H102",
        serde_json::json!({
            "dataPort": data_port,
            "requestStreamId": video_stream_id,
            "responseStreamId": resp_stream_id,
            "hkdfStreamId": hkdf_stream_id,
            "streamIdMismatch": resp_stream_id.is_some_and(|id| id != video_stream_id),
            "rootKeys": video_resp_keys,
        }),
    );
    // #endregion

    // #region agent log
    agent_log(
        "mirror_rtsp.rs:setup_mirror_rtsp",
        "video data port extracted",
        "Y",
        serde_json::json!({ "dataPort": data_port }),
    );
    // #endregion

    let receiver_event_port = plist_event_port(&control_resp)
        .or_else(|| plist_event_port(&audio_resp))
        .or_else(|| plist_event_port(&video_resp));
    let receiver_event_task = connect_receiver_event(host, receiver_event_port).await;

    let data_stream = connect_data_port(host, data_port).await?;

    let skip_record = plist_bool(&control_resp, "skipRecord")
        || plist_bool(&audio_resp, "skipRecord")
        || plist_bool(&video_resp, "skipRecord");
    let (record_status, record_audio_latency) = if skip_record {
        (200, None)
    } else {
        let (status, _, latency) = conn
            .rtsp_record(&audio_uri, &session_uuid, &dacp_id, active_remote)
            .await?;
        (status, latency)
    };

    let audio_latency_samples = record_audio_latency.unwrap_or(session_latency_samples);
    // #region agent log
    agent_log(
        "mirror_rtsp.rs:setup_mirror_rtsp",
        "RECORD audio latency",
        "H74",
        serde_json::json!({
            "audioLatencySamples": audio_latency_samples,
            "fromRecordHeader": record_audio_latency.is_some(),
            "skipped": skip_record,
        }),
    );
    // #endregion

    // #region agent log
    agent_log(
        "mirror_rtsp.rs:setup_mirror_rtsp",
        "RECORD response",
        "Y",
        serde_json::json!({ "httpStatus": record_status, "skipped": skip_record }),
    );
    // #endregion

    if record_status != 200 {
        return Err(RottenError::Protocol(format!(
            "mirror RECORD HTTP {record_status}"
        )));
    }

    let volume_body = b"volume: 0.000000\r\n";
    for i in 0..2 {
        let (vol_status, _) = conn
            .rtsp_set_parameter(&audio_uri, &session_uuid, volume_body)
            .await?;
        // #region agent log
        agent_log(
            "mirror_rtsp.rs:setup_mirror_rtsp",
            "SET_PARAMETER volume",
            "H5",
            serde_json::json!({ "httpStatus": vol_status, "attempt": i + 1 }),
        );
        // #endregion
    }

    let video_crypto = MirrorVideoCrypto::from_setup(
        cipher_mode,
        no_encrypt,
        &fp_keys.stream_key,
        &pv.shared_secret,
        hkdf_stream_id,
    );

    // #region agent log
    let effective_mode = if no_encrypt {
        "none"
    } else {
        video_crypto.mode_name()
    };
    let (derived_key_prefix, derived_iv_prefix) = match &video_crypto {
        MirrorVideoCrypto::AesCtr { key, iv } => (hex::encode(&key[..4]), hex::encode(&iv[..4])),
        MirrorVideoCrypto::ChaCha { key } => (hex::encode(&key[..4]), String::new()),
        MirrorVideoCrypto::None => (String::new(), String::new()),
    };
    let chacha_key_fp_aes = if !no_encrypt && matches!(cipher_mode, MirrorCipherMode::ChaCha) {
        let k = rotten_crypto::derive_data_stream_chacha_key(&fp_keys.fp_aes_key, hkdf_stream_id);
        hex::encode(&k[..4])
    } else {
        String::new()
    };
    agent_log(
        "mirror_rtsp.rs:setup_mirror_rtsp",
        "mirror video cipher selected",
        "H25",
        serde_json::json!({
            "cipher": effective_mode,
            "requestedCipher": match cipher_mode {
                MirrorCipherMode::AesCtr => "aes",
                MirrorCipherMode::ChaCha => "chacha",
            },
            "videoStreamId": video_stream_id,
            "hkdfStreamId": hkdf_stream_id,
            "legacyPairVerify": true,
            "noEncrypt": no_encrypt,
            "cipherMode": match cipher_mode {
                MirrorCipherMode::AesCtr => "aes",
                MirrorCipherMode::ChaCha => "chacha",
            },
            "fpKeyPrefix": hex::encode(&fp_keys.fp_key[..4]),
            "fpAesKeyPrefix": hex::encode(&fp_keys.fp_aes_key[..4]),
            "derivedKeyPrefix": derived_key_prefix,
            "chachaKeyFpAesPrefix": chacha_key_fp_aes,
            "derivedIvPrefix": derived_iv_prefix,
            "deviceFeatures": format!("0x{:x}", device.features.raw),
        }),
    );
    // #endregion

    let audio_setup = if let Some((remote_data_port, remote_control_port)) = audio_ports {
        if udp_sockets.len() >= 2 {
            let ctrl_socket = udp_sockets.remove(0);
            let data_socket = udp_sockets.remove(0);
            info!(
                host,
                remote_data_port,
                remote_control_port,
                local_data_port = data_socket.local_addr().ok().map(|addr| addr.port()),
                local_control_port = ctrl_socket.local_addr().ok().map(|addr| addr.port()),
                "AirPlay mirror audio UDP ports"
            );
            Some(MirrorAudioSetup {
                host: host.to_string(),
                cipher: audio_cipher,
                remote_data_port,
                remote_control_port,
                ctrl_socket,
                data_socket,
                latency_samples: audio_latency_samples,
                legacy_ntp_layout: legacy_fairplay_audio,
                legacy_redundancy: legacy_fairplay_audio,
                pcm_audio: legacy_pcm_audio,
            })
        } else {
            None
        }
    } else {
        None
    };

    Ok(MirrorRtspSetup {
        video_crypto,
        data_port,
        event_port: receiver_event_port,
        timing_port,
        session_uuid,
        control_uri: audio_uri,
        data_stream,
        audio: audio_setup,
        resources: MirrorRtspResources {
            _timing_task: timing_task,
            _event_task: event_task,
            _udp_sockets: udp_sockets,
            _event_listener: event_listener,
            _receiver_event_task: receiver_event_task,
        },
    })
}

async fn connect_receiver_event(host: &str, port: Option<u16>) -> Option<JoinHandle<()>> {
    let port = port?;
    let addr = format!("{host}:{port}");
    match tokio::time::timeout(Duration::from_secs(3), TcpStream::connect(&addr)).await {
        Ok(Ok(stream)) => {
            // #region agent log
            agent_log(
                "mirror_rtsp.rs:connect_receiver_event",
                "connected to receiver event port",
                "AD",
                serde_json::json!({ "addr": addr }),
            );
            // #endregion
            let _ = stream.set_nodelay(true);
            Some(tokio::spawn(async move {
                let stream = match stream.into_std() {
                    Ok(std_stream) => TcpStream::from_std(std_stream).ok(),
                    Err(_) => None,
                };
                let Some(stream) = stream else {
                    return;
                };
                let (reader, writer) = stream.into_split();
                receiver_event_loop(reader, writer).await;
            }))
        }
        Ok(Err(e)) => {
            // #region agent log
            agent_log(
                "mirror_rtsp.rs:connect_receiver_event",
                "receiver event connect failed",
                "AD",
                serde_json::json!({ "addr": addr, "error": e.to_string() }),
            );
            // #endregion
            None
        }
        Err(_) => {
            // #region agent log
            agent_log(
                "mirror_rtsp.rs:connect_receiver_event",
                "receiver event connect timeout",
                "AD",
                serde_json::json!({ "addr": addr }),
            );
            // #endregion
            None
        }
    }
}

/// Read RTSP requests from the Apple TV event port and reply 200 OK.
/// The TV sends `POST /command` with plist payloads; silence causes a reset.
async fn receiver_event_loop(mut reader: OwnedReadHalf, mut writer: OwnedWriteHalf) {
    let mut buf = Vec::new();
    let mut scratch = [0u8; 4096];
    loop {
        match reader.read(&mut scratch).await {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&scratch[..n]),
            Err(_) => break,
        }
        while let Some((header_end, content_len)) = parse_rtsp_request_headers(&buf) {
            let total = header_end + content_len;
            if buf.len() < total {
                break;
            }
            let request = &buf[..total];
            if let Some((method, path, cseq)) = rtsp_request_meta(request) {
                // #region agent log
                agent_log(
                    "mirror_rtsp.rs:receiver_event_loop",
                    "receiver event RTSP request",
                    "H95",
                    serde_json::json!({
                        "method": method,
                        "path": path,
                        "cseq": cseq,
                        "contentLen": content_len,
                        "bodyPrefix": hex::encode(&request[header_end..total.min(header_end + 32)]),
                    }),
                );
                // #endregion
                let (response_header, response_body): (String, Vec<u8>) = if path == "/command" {
                    let mut body = Vec::new();
                    let _ = plist::to_writer_binary(
                        &mut body,
                        &Value::Dictionary(plist::Dictionary::new()),
                    );
                    let header = format!(
                        "RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nContent-Type: application/x-apple-binary-plist\r\nContent-Length: {}\r\nServer: AirTunes/366.0\r\n\r\n",
                        body.len()
                    );
                    (header, body)
                } else {
                    (
                        format!(
                            "RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nServer: AirTunes/366.0\r\n\r\n"
                        ),
                        Vec::new(),
                    )
                };
                if writer.write_all(response_header.as_bytes()).await.is_err()
                    || (!response_body.is_empty()
                        && writer.write_all(&response_body).await.is_err())
                {
                    // #region agent log
                    agent_log(
                        "mirror_rtsp.rs:receiver_event_loop",
                        "receiver event response write failed",
                        "H95",
                        serde_json::json!({ "cseq": cseq, "path": path }),
                    );
                    // #endregion
                    return;
                }
                // #region agent log
                agent_log(
                    "mirror_rtsp.rs:receiver_event_loop",
                    "receiver event RTSP 200 sent",
                    "H95",
                    serde_json::json!({ "cseq": cseq, "path": path }),
                );
                // #endregion
            }
            buf.drain(..total);
        }
        if buf.len() > 64 * 1024 {
            buf.clear();
        }
    }
}

fn parse_rtsp_request_headers(buf: &[u8]) -> Option<(usize, usize)> {
    let header_end = buf.windows(4).position(|w| w == b"\r\n\r\n")? + 4;
    let headers = std::str::from_utf8(&buf[..header_end]).ok()?;
    let mut content_len = 0usize;
    for line in headers.lines() {
        if let Some(v) = line
            .strip_prefix("Content-Length:")
            .or_else(|| line.strip_prefix("content-length:"))
        {
            content_len = v.trim().parse().unwrap_or(0);
        }
    }
    Some((header_end, content_len))
}

fn rtsp_request_meta(request: &[u8]) -> Option<(&str, &str, u32)> {
    let headers = std::str::from_utf8(request).ok()?;
    let request_line = headers.lines().next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?;
    let path = parts.next()?;
    let mut cseq = 0u32;
    for line in headers.lines().skip(1) {
        if let Some(v) = line
            .strip_prefix("CSeq:")
            .or_else(|| line.strip_prefix("cseq:"))
        {
            cseq = v.trim().parse().unwrap_or(0);
            break;
        }
    }
    Some((method, path, cseq))
}

async fn connect_data_port(host: &str, data_port: u16) -> Result<TcpStream> {
    let addr = format!("{host}:{data_port}");
    let mut stream = tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(&addr))
        .await
        .map_err(|_| RottenError::Protocol(format!("timeout connecting to data port {addr}")))?
        .map_err(|e| RottenError::Protocol(format!("connect data port {addr}: {e}")))?;
    stream
        .set_nodelay(true)
        .map_err(|e| RottenError::Protocol(format!("set_nodelay: {e}")))?;
    // #region agent log
    agent_log(
        "mirror_rtsp.rs:connect_data_port",
        "video data channel connected",
        "AD",
        serde_json::json!({ "addr": addr }),
    );
    // #endregion
    match stream.into_std() {
        Ok(std_stream) => {
            if let Ok(reader_std) = std_stream.try_clone() {
                tokio::spawn(async move {
                    let mut reader = match TcpStream::from_std(reader_std) {
                        Ok(s) => s,
                        Err(_) => return,
                    };
                    let mut buf = [0u8; 4096];
                    loop {
                        match reader.readable().await {
                            Ok(()) => match reader.try_read(&mut buf) {
                                Ok(0) => break,
                                Ok(n) => {
                                    // #region agent log
                                    agent_log(
                                        "mirror_rtsp.rs:data_read",
                                        "data channel inbound bytes",
                                        "H79",
                                        serde_json::json!({
                                            "bytes": n,
                                            "prefix": hex::encode(&buf[..n.min(32)]),
                                        }),
                                    );
                                    // #endregion
                                }
                                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                                Err(_) => break,
                            },
                            Err(_) => break,
                        }
                    }
                });
            }
            TcpStream::from_std(std_stream)
                .map_err(|e| RottenError::Protocol(format!("restore data stream: {e}")))
        }
        Err(e) => Err(RottenError::Protocol(format!("data stream into_std: {e}"))),
    }
}

fn setup_failed(
    status: u16,
    phase: &str,
    timing_port: u16,
    event_port: u16,
    ntp_probed: bool,
    event_connected: bool,
) -> RottenError {
    let hint = if !ntp_probed && rotten_core::running_in_wsl() {
        format!(
            " — mirroring from WSL cannot work: Apple TV must reach your machine on UDP timing port {timing_port} and TCP event port {event_port}, but WSL2 NAT blocks inbound LAN traffic. Run `rottingapple.exe mirror` from Windows (copy ~/.config/rottingapple/credentials.json to %USERPROFILE%\\.config\\rottingapple\\), or enable WSL mirrored networking (.wslconfig: networkingMode=mirrored)."
        )
    } else if !ntp_probed && !event_connected {
        format!(
            " — Apple TV could not reach timing UDP {timing_port} or event TCP {event_port} (ntpProbed=false). Allow inbound UDP+TCP from your LAN in the host firewall."
        )
    } else if status == 466 {
        format!(" (ntpProbed={ntp_probed}, eventConnected={event_connected})")
    } else if !ntp_probed {
        format!(" (ntpProbed=false, eventConnected={event_connected})")
    } else {
        String::new()
    };
    RottenError::Protocol(format!("mirror {phase} SETUP HTTP {status}{hint}"))
}

fn setup_order_rejected(status: u16) -> bool {
    matches!(status, 400 | 405 | 406 | 415 | 455 | 501)
}

/// Control-only SETUP creates the receiver session before either media stream
/// is attached. Legacy receivers use this request to install the FairPlay
/// root material once, then accept stream-only audio and video descriptors.
fn encode_control_setup_plist(
    device_id: i64,
    session_uuid: &str,
    timing_port: u16,
    fp_keys: &MirrorFpKeys,
    include_fairplay_security: bool,
) -> Result<Vec<u8>> {
    let mut dict = plist::Dictionary::new();
    dict.insert("deviceID".into(), Value::Integer(device_id.into()));
    dict.insert("macAddress".into(), Value::Integer(device_id.into()));
    dict.insert("sessionUUID".into(), Value::String(session_uuid.into()));
    dict.insert("sourceVersion".into(), Value::String("280.33".into()));
    dict.insert("isScreenMirroringSession".into(), Value::Boolean(true));
    dict.insert("timingProtocol".into(), Value::String("NTP".into()));
    dict.insert("timingPort".into(), Value::Integer(timing_port.into()));
    dict.insert("osBuildVersion".into(), Value::String("13F69".into()));
    dict.insert("model".into(), Value::String("Linux".into()));
    dict.insert("name".into(), Value::String("Linux".into()));
    dict.insert("updateSessionRequest".into(), Value::Boolean(false));
    dict.insert(
        "combinedGetInfoWithControlSetup".into(),
        Value::Boolean(true),
    );
    if include_fairplay_security {
        dict.insert("ekey".into(), Value::Data(fp_keys.ekey.to_vec()));
        dict.insert("eiv".into(), Value::Data(fp_keys.fp_iv.to_vec()));
        dict.insert("et".into(), Value::Integer(32.into()));
    }
    plist_encode(dict)
}

/// Audio SETUP for Apple TV 2/3 compatible firmware using FairPlay AES-CBC.
fn encode_audio_setup_plist_legacy_aes(
    device_id: i64,
    session_uuid: &str,
    timing_port: u16,
    stream_connection_id: i64,
    control_port: u16,
    ekey: &[u8; 72],
    eiv: &[u8; 16],
    latency_samples: u32,
    include_fairplay_security: bool,
    audio_format: i64,
    compression_type: i64,
    samples_per_frame: i64,
) -> Result<Vec<u8>> {
    let mut audio_stream = plist::Dictionary::new();
    audio_stream.insert("type".into(), Value::Integer(96.into()));
    audio_stream.insert(
        "streamConnectionID".into(),
        Value::Integer(stream_connection_id.into()),
    );
    audio_stream.insert("ct".into(), Value::Integer(compression_type.into()));
    audio_stream.insert("spf".into(), Value::Integer(samples_per_frame.into()));
    audio_stream.insert("sr".into(), Value::Integer(44100.into()));
    audio_stream.insert("audioFormat".into(), Value::Integer(audio_format.into()));
    audio_stream.insert("audioFormatIndex".into(), Value::Integer(19.into()));
    audio_stream.insert("controlPort".into(), Value::Integer(control_port.into()));
    audio_stream.insert("audioMode".into(), Value::String("default".into()));
    audio_stream.insert("usingScreen".into(), Value::Boolean(true));
    audio_stream.insert("latencyMin".into(), Value::Integer(0.into()));
    audio_stream.insert(
        "latencyMax".into(),
        Value::Integer(i64::from(latency_samples).into()),
    );
    audio_stream.insert("redundantAudio".into(), Value::Integer(2.into()));

    let mut dict = plist::Dictionary::new();
    dict.insert("deviceID".into(), Value::Integer(device_id.into()));
    dict.insert("macAddress".into(), Value::Integer(device_id.into()));
    dict.insert("sessionUUID".into(), Value::String(session_uuid.into()));
    dict.insert("sourceVersion".into(), Value::String("280.33".into()));
    dict.insert("timingProtocol".into(), Value::String("NTP".into()));
    dict.insert("timingPort".into(), Value::Integer(timing_port.into()));
    dict.insert("osBuildVersion".into(), Value::String("13F69".into()));
    dict.insert("model".into(), Value::String("Linux".into()));
    dict.insert("name".into(), Value::String("Linux".into()));
    if include_fairplay_security {
        dict.insert("ekey".into(), Value::Data(ekey.to_vec()));
        dict.insert("eiv".into(), Value::Data(eiv.to_vec()));
        dict.insert("et".into(), Value::Integer(32.into()));
    }
    dict.insert(
        "streams".into(),
        Value::Array(vec![Value::Dictionary(audio_stream)]),
    );
    plist_encode(dict)
}

/// Audio SETUP for HAP/modern receivers: ChaCha shk on stream (no root ekey/eiv).
fn encode_audio_setup_plist_chacha(
    device_id: i64,
    session_uuid: &str,
    timing_port: u16,
    stream_connection_id: i64,
    control_port: u16,
    audio_chacha_key: &[u8; 32],
    latency_samples: u32,
) -> Result<Vec<u8>> {
    let mut rtp_conn = plist::Dictionary::new();
    rtp_conn.insert(
        "streamConnectionKeyUseStreamEncryptionKey".into(),
        Value::Boolean(true),
    );
    let mut rtcp_conn = plist::Dictionary::new();
    rtcp_conn.insert(
        "streamConnectionKeyPort".into(),
        Value::Integer(control_port.into()),
    );
    let mut stream_connections = plist::Dictionary::new();
    stream_connections.insert(
        "streamConnectionTypeRTP".into(),
        Value::Dictionary(rtp_conn),
    );
    stream_connections.insert(
        "streamConnectionTypeRTCP".into(),
        Value::Dictionary(rtcp_conn),
    );

    let mut audio_stream = plist::Dictionary::new();
    audio_stream.insert("type".into(), Value::Integer(96.into()));
    audio_stream.insert(
        "streamConnectionID".into(),
        Value::Integer(stream_connection_id.into()),
    );
    audio_stream.insert("ct".into(), Value::Integer(2.into()));
    audio_stream.insert("spf".into(), Value::Integer(352.into()));
    audio_stream.insert("sr".into(), Value::Integer(44100.into()));
    audio_stream.insert("audioFormat".into(), Value::Integer(0x40000.into()));
    audio_stream.insert("audioFormatIndex".into(), Value::Integer(19.into()));
    audio_stream.insert("controlPort".into(), Value::Integer(control_port.into()));
    audio_stream.insert("audioMode".into(), Value::String("default".into()));
    audio_stream.insert("usingScreen".into(), Value::Boolean(true));
    audio_stream.insert("latencyMin".into(), Value::Integer(0.into()));
    audio_stream.insert(
        "latencyMax".into(),
        Value::Integer(i64::from(latency_samples).into()),
    );
    // ChaCha HAP receivers: no FEC / redundant audio (doubletake `useAudioFEC(false)`).
    audio_stream.insert("redundantAudio".into(), Value::Integer(0.into()));
    audio_stream.insert("disableRetransmits".into(), Value::Boolean(true));
    audio_stream.insert("shk".into(), Value::Data(audio_chacha_key.to_vec()));
    // Screen audio is routed separately from the receiver's primary media
    // stream. Marking it as isMedia can leave TV firmware waiting indefinitely.
    audio_stream.insert("isMedia".into(), Value::Boolean(false));
    audio_stream.insert("supportsDynamicStreamID".into(), Value::Boolean(true));
    audio_stream.insert(
        "streamConnections".into(),
        Value::Dictionary(stream_connections),
    );

    let mut dict = plist::Dictionary::new();
    dict.insert("deviceID".into(), Value::Integer(device_id.into()));
    dict.insert("macAddress".into(), Value::Integer(device_id.into()));
    dict.insert("sessionUUID".into(), Value::String(session_uuid.into()));
    dict.insert("sourceVersion".into(), Value::String("280.33".into()));
    dict.insert("timingProtocol".into(), Value::String("NTP".into()));
    dict.insert("timingPort".into(), Value::Integer(timing_port.into()));
    dict.insert("osBuildVersion".into(), Value::String("13F69".into()));
    dict.insert("model".into(), Value::String("Linux".into()));
    dict.insert("name".into(), Value::String("Linux".into()));
    dict.insert(
        "streams".into(),
        Value::Array(vec![Value::Dictionary(audio_stream)]),
    );
    plist_encode(dict)
}

fn encode_video_setup_plist(
    device_id: i64,
    session_uuid: &str,
    timing_port: u16,
    stream_connection_id: i64,
    fp_keys: &MirrorFpKeys,
    include_encryption_keys: bool,
    include_fairplay_roots: bool,
    video_latency_ms: i64,
) -> Result<Vec<u8>> {
    let timestamp_info = Value::Array(
        ["SubSu", "BePxT", "AfPxT", "BefEn", "EmEnc"]
            .iter()
            .map(|name| {
                let mut d = plist::Dictionary::new();
                d.insert("name".into(), Value::String((*name).into()));
                Value::Dictionary(d)
            })
            .collect(),
    );

    let mut video_stream = plist::Dictionary::new();
    video_stream.insert("type".into(), Value::Integer(110.into()));
    video_stream.insert(
        "streamConnectionID".into(),
        Value::Integer(stream_connection_id.into()),
    );
    video_stream.insert("timestampInfo".into(), timestamp_info);
    video_stream.insert("latencyMs".into(), Value::Integer(video_latency_ms.into()));
    if include_encryption_keys {
        video_stream.insert("shk".into(), Value::Data(fp_keys.stream_key.to_vec()));
        video_stream.insert("shiv".into(), Value::Data(fp_keys.fp_iv.to_vec()));
    }

    let mut dict = plist::Dictionary::new();
    dict.insert("deviceID".into(), Value::Integer(device_id.into()));
    dict.insert("macAddress".into(), Value::Integer(device_id.into()));
    dict.insert("sessionUUID".into(), Value::String(session_uuid.into()));
    dict.insert("sourceVersion".into(), Value::String("280.33".into()));
    dict.insert("isScreenMirroringSession".into(), Value::Boolean(true));
    dict.insert("timingProtocol".into(), Value::String("NTP".into()));
    dict.insert("timingPort".into(), Value::Integer(timing_port.into()));
    dict.insert("osBuildVersion".into(), Value::String("13F69".into()));
    dict.insert("model".into(), Value::String("Linux".into()));
    dict.insert("name".into(), Value::String("Linux".into()));
    dict.insert(
        "streams".into(),
        Value::Array(vec![Value::Dictionary(video_stream)]),
    );
    if include_fairplay_roots {
        dict.insert("ekey".into(), Value::Data(fp_keys.ekey.to_vec()));
        dict.insert("eiv".into(), Value::Data(fp_keys.fp_iv.to_vec()));
        dict.insert("et".into(), Value::Integer(32.into()));
    }
    plist_encode(dict)
}

fn plist_encode(dict: plist::Dictionary) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    plist::to_writer_binary(&mut buf, &Value::Dictionary(dict))
        .map_err(|e| RottenError::Protocol(format!("mirror plist: {e}")))?;
    Ok(buf)
}

fn plist_stream_only(body: &[u8]) -> Result<Vec<u8>> {
    let value: Value = plist::from_bytes(body)
        .map_err(|e| RottenError::Protocol(format!("mirror stream plist decode: {e}")))?;
    let streams = value
        .as_dictionary()
        .and_then(|dict| dict.get("streams"))
        .cloned()
        .ok_or_else(|| RottenError::Protocol("mirror stream plist has no streams".into()))?;
    let mut dict = plist::Dictionary::new();
    dict.insert("streams".into(), streams);
    plist_encode(dict)
}

fn plist_dict_keys(body: &[u8]) -> Vec<String> {
    let Ok(value) = plist::from_bytes::<Value>(body) else {
        return Vec::new();
    };
    let Some(dict) = value.as_dictionary() else {
        return Vec::new();
    };
    dict.keys().map(|k| k.to_string()).collect()
}

fn plist_video_stream_info(body: &[u8], stream_type: i64) -> Option<(u16, Option<i64>)> {
    let value: Value = plist::from_bytes(body).ok()?;
    let dict = value.as_dictionary()?;
    let streams = dict.get("streams")?.as_array()?;
    for stream in streams {
        let sd = stream.as_dictionary()?;
        if plist_int(sd.get("type")?) != stream_type {
            continue;
        }
        let data_port = plist_int(sd.get("dataPort")?).try_into().ok()?;
        let stream_id = sd.get("streamConnectionID").map(plist_int);
        return Some((data_port, stream_id));
    }
    None
}

fn plist_stream_data_port(body: &[u8], stream_type: i64) -> Option<u16> {
    plist_video_stream_info(body, stream_type).map(|(port, _)| port)
}

fn plist_event_port(body: &[u8]) -> Option<u16> {
    if body.is_empty() {
        return None;
    }
    let value: Value = plist::from_bytes(body).ok()?;
    let dict = value.as_dictionary()?;
    plist_int(dict.get("eventPort")?).try_into().ok()
}

fn plist_bool(body: &[u8], key: &str) -> bool {
    let Ok(value) = plist::from_bytes::<Value>(body) else {
        return false;
    };
    value
        .as_dictionary()
        .and_then(|dict| dict.get(key))
        .is_some_and(|value| value == &Value::Boolean(true))
}

fn plist_int(value: &Value) -> i64 {
    match value {
        Value::Integer(i) => i.as_signed().unwrap_or(0),
        Value::Real(f) => *f as i64,
        _ => 0,
    }
}

fn mac_to_device_id_int(device_id: &str) -> i64 {
    let hex: String = device_id
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .collect();
    if hex.is_empty() {
        return 0;
    }
    let trimmed = if hex.len() > 12 { &hex[..12] } else { &hex };
    u64::from_str_radix(trimmed, 16).unwrap_or(0) as i64
}

fn stream_connection_id() -> i64 {
    static LAST_ID: AtomicI64 = AtomicI64::new(0);

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as i64
        & 0x7fff_ffff_ffff_ffff;
    let mut previous = LAST_ID.load(Ordering::Relaxed);
    loop {
        let candidate = timestamp.max(previous.saturating_add(1)).max(1) & 0x7fff_ffff_ffff_ffff;
        match LAST_ID.compare_exchange_weak(
            previous,
            candidate,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return candidate,
            Err(observed) => previous = observed,
        }
    }
}

fn uses_legacy_fairplay_audio(device: &AirPlayDevice) -> bool {
    device
        .model
        .as_deref()
        .is_some_and(|model| model.starts_with("AppleTV2,1") || model.starts_with("AppleTV3,2"))
}

fn random_chacha_key() -> [u8; 32] {
    let mut key = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut key);
    key
}

async fn bind_consecutive_udp(count: usize) -> Result<Vec<UdpSocket>> {
    for base in (49152u16..65000).step_by(count) {
        let mut sockets = Vec::with_capacity(count);
        let mut failed = false;
        for i in 0..count {
            match UdpSocket::bind(("0.0.0.0", base + i as u16)).await {
                Ok(sock) => sockets.push(sock),
                Err(_) => {
                    failed = true;
                    break;
                }
            }
        }
        if !failed && sockets.len() == count {
            return Ok(sockets);
        }
    }
    Err(RottenError::Protocol(
        "failed to bind consecutive UDP ports for mirror timing".into(),
    ))
}

async fn bind_event_listener(timing_port: u16) -> Result<(TcpListener, u16, &'static str)> {
    if let Ok(listener) = TcpListener::bind(("0.0.0.0", timing_port)).await {
        return Ok((listener, timing_port, "timingPort"));
    }
    let fallback = timing_port.saturating_add(3);
    if let Ok(listener) = TcpListener::bind(("0.0.0.0", fallback)).await {
        return Ok((listener, fallback, "timingPortPlus3"));
    }
    let listener = TcpListener::bind("0.0.0.0:0")
        .await
        .map_err(|e| RottenError::Protocol(format!("event listener: {e}")))?;
    let port = listener
        .local_addr()
        .map_err(|e| RottenError::Protocol(e.to_string()))?
        .port();
    Ok((listener, port, "ephemeral"))
}

async fn ntp_timing_responder(sock: UdpSocket, probed: Arc<AtomicBool>) {
    let mut buf = [0u8; 128];
    loop {
        match tokio::time::timeout(Duration::from_secs(3), sock.recv_from(&mut buf)).await {
            Ok(Ok((n, peer))) if n >= 32 => {
                probed.store(true, Ordering::Relaxed);
                // #region agent log
                agent_log(
                    "mirror_rtsp.rs:ntp",
                    "NTP timing probe received",
                    "AB",
                    serde_json::json!({ "peer": peer.to_string(), "bytes": n }),
                );
                // #endregion
                let mut reply = [0u8; 32];
                reply[..32].copy_from_slice(&buf[..32]);
                reply[0] = 0x80;
                reply[1] = 0xd3;
                let now = ntp_boot_with_epoch();
                reply[8..16].copy_from_slice(&buf[24..32]);
                reply[16..24].copy_from_slice(&now.to_be_bytes());
                reply[24..32].copy_from_slice(&now.to_be_bytes());
                // #region agent log
                agent_log(
                    "mirror_rtsp.rs:ntp",
                    "NTP timing reply sent",
                    "H83",
                    serde_json::json!({
                        "ntpReplySec": now >> 32,
                        "ntpReplyFrac": now & 0xFFFF_FFFF,
                    }),
                );
                // #endregion
                let _ = sock.send_to(&reply, peer).await;
            }
            Ok(Ok(_)) => {}
            Ok(Err(_)) => break,
            Err(_) => continue,
        }
    }
}

#[cfg(test)]
mod tests {
    use plist::Value;

    use super::{
        encode_audio_setup_plist_legacy_aes, encode_control_setup_plist, plist_bool,
        plist_stream_only, setup_order_rejected, stream_connection_id,
    };
    use rotten_crypto::MirrorFpKeys;

    fn test_fp_keys() -> MirrorFpKeys {
        MirrorFpKeys {
            ekey: [0x11; 72],
            fp_key: [0x22; 16],
            stream_key: [0x33; 16],
            fp_iv: [0x44; 16],
            fp_aes_key: [0x55; 16],
        }
    }

    #[test]
    fn control_setup_creates_session_without_streams() {
        let body =
            encode_control_setup_plist(0x0011_2233_4455, "session", 6000, &test_fp_keys(), true)
                .unwrap();
        let value: Value = plist::from_bytes(&body).unwrap();
        let dict = value.as_dictionary().unwrap();

        assert!(!dict.contains_key("streams"));
        assert_eq!(
            dict.get("updateSessionRequest"),
            Some(&Value::Boolean(false))
        );
        assert_eq!(
            dict.get("combinedGetInfoWithControlSetup"),
            Some(&Value::Boolean(true))
        );
        assert_eq!(
            dict.get("ekey")
                .and_then(Value::as_data)
                .map(|data| data.len()),
            Some(72)
        );
    }

    #[test]
    fn stream_only_setup_removes_session_and_fairplay_root_fields() {
        let full = encode_audio_setup_plist_legacy_aes(
            1,
            "session",
            6000,
            42,
            6001,
            &[0x11; 72],
            &[0x22; 16],
            0,
            true,
            0x40000,
            2,
            352,
        )
        .unwrap();
        let stream_only = plist_stream_only(&full).unwrap();
        let value: Value = plist::from_bytes(&stream_only).unwrap();
        let dict = value.as_dictionary().unwrap();

        assert_eq!(dict.len(), 1);
        assert!(!dict.contains_key("sessionUUID"));
        assert!(!dict.contains_key("ekey"));
        let stream = dict
            .get("streams")
            .and_then(Value::as_array)
            .and_then(|streams| streams.first())
            .and_then(Value::as_dictionary)
            .unwrap();
        assert_eq!(
            stream.get("type").and_then(Value::as_signed_integer),
            Some(96)
        );
        assert_eq!(
            stream
                .get("streamConnectionID")
                .and_then(Value::as_signed_integer),
            Some(42)
        );
    }

    #[test]
    fn only_protocol_rejections_trigger_setup_order_fallback() {
        for status in [400, 405, 406, 415, 455, 501] {
            assert!(setup_order_rejected(status), "{status}");
        }
        for status in [200, 401, 403, 404, 500, 502] {
            assert!(!setup_order_rejected(status), "{status}");
        }
    }

    #[test]
    fn receiver_skip_record_flag_is_honored() {
        let mut dict = plist::Dictionary::new();
        dict.insert("skipRecord".into(), Value::Boolean(true));
        let mut body = Vec::new();
        plist::to_writer_binary(&mut body, &Value::Dictionary(dict)).unwrap();

        assert!(plist_bool(&body, "skipRecord"));
        assert!(!plist_bool(&body, "missing"));
    }

    #[test]
    fn consecutive_stream_ids_are_unique_on_coarse_windows_clock() {
        let first = stream_connection_id();
        let second = stream_connection_id();
        assert!(first > 0);
        assert!(second > first);
    }
}
