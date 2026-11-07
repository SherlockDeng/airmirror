use std::collections::HashMap;
use std::time::Duration;

use mdns_sd::{ServiceDaemon, ServiceEvent};
use rotten_core::device::{AirPlayDevice, DeviceFeatures};
use rotten_core::error::{Result, RottenError};
use tracing::{debug, info};

const AIRPLAY_SERVICE: &str = "_airplay._tcp.local.";

/// Browse the LAN for AirPlay receivers for up to `timeout`.
pub async fn discover_for(timeout: Duration) -> Result<Vec<AirPlayDevice>> {
    let daemon =
        ServiceDaemon::new().map_err(|e| RottenError::Discovery(format!("mDNS daemon: {e}")))?;

    let receiver = daemon
        .browse(AIRPLAY_SERVICE)
        .map_err(|e| RottenError::Discovery(format!("browse: {e}")))?;

    let mut devices: HashMap<String, AirPlayDevice> = HashMap::new();
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }

        match tokio::time::timeout(remaining, receiver.recv_async()).await {
            Ok(Ok(event)) => match event {
                ServiceEvent::ServiceResolved(info) => {
                    let name = info.get_fullname().to_string();
                    let hostname = info.get_hostname().trim_end_matches('.').to_string();
                    let port = info.get_port();
                    let host = info
                        .get_addresses()
                        .iter()
                        .find(|ip| ip.is_ipv4())
                        .map(|ip| ip.to_string())
                        .unwrap_or_else(|| hostname.clone());
                    let props = info.get_properties();

                    let device_id = props
                        .get("deviceid")
                        .map(|v| v.val_str().to_string())
                        .unwrap_or_else(|| host.clone());

                    let model = props.get("model").map(|v| v.val_str().to_string());
                    let features = props
                        .get("features")
                        .map(|v| DeviceFeatures::from_hex(v.val_str()))
                        .unwrap_or_default();
                    let pi = props.get("pi").map(|v| v.val_str().to_string());
                    let pk = props.get("pk").map(|v| v.val_str().to_string());

                    let display_name = name.split('.').next().unwrap_or(&name).to_string();

                    debug!(%display_name, %host, port, %device_id, "resolved AirPlay device");

                    devices.insert(
                        device_id.clone(),
                        AirPlayDevice {
                            name: display_name,
                            host,
                            port,
                            device_id,
                            model,
                            features,
                            display_width: None,
                            display_height: None,
                            screen_audio_formats: None,
                            pi,
                            pk,
                        },
                    );
                }
                ServiceEvent::ServiceFound(_, fullname) => {
                    debug!(%fullname, "AirPlay service found, resolving...");
                }
                ServiceEvent::ServiceRemoved(_, fullname) => {
                    debug!(%fullname, "AirPlay service removed");
                }
                _ => {}
            },
            Ok(Err(e)) => {
                return Err(RottenError::Discovery(format!("recv: {e}")));
            }
            Err(_) => break,
        }
    }

    let _ = daemon.shutdown();

    let mut list: Vec<AirPlayDevice> = devices.into_values().collect();
    list.sort_by(|a, b| a.name.cmp(&b.name));
    info!(count = list.len(), "discovery complete");
    Ok(list)
}

/// Default 5-second discovery scan.
pub async fn discover_devices() -> Result<Vec<AirPlayDevice>> {
    discover_for(Duration::from_secs(5)).await
}
