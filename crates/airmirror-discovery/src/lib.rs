use std::{
    collections::{BTreeMap, HashMap},
    net::{IpAddr, SocketAddr},
    time::{Duration, Instant},
};

use airmirror_core::{Receiver, ReceiverCapabilities, parse_features};
use mdns_sd::{ResolvedService, ServiceDaemon, ServiceEvent};
use thiserror::Error;

pub const AIRPLAY_SERVICE: &str = "_airplay._tcp.local.";

#[derive(Debug, Error)]
pub enum DiscoveryError {
    #[error("failed to start mDNS service: {0}")]
    Start(#[from] mdns_sd::Error),

    #[error("received an invalid AirPlay service record: {0}")]
    InvalidRecord(String),
}

/// Discovers AirPlay receivers on all active LAN interfaces.
///
/// The browser remains active for the full timeout. This allows receivers that
/// answer late to be included and avoids relying on a single multicast packet.
pub fn discover(timeout: Duration) -> Result<Vec<Receiver>, DiscoveryError> {
    let daemon = ServiceDaemon::new()?;
    let events = daemon.browse(AIRPLAY_SERVICE)?;
    let deadline = Instant::now() + timeout;
    let mut receivers = HashMap::<String, Receiver>::new();

    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        let remaining = deadline.saturating_duration_since(now);
        match events.recv_timeout(remaining) {
            Ok(ServiceEvent::ServiceResolved(info)) => {
                if let Ok(receiver) = receiver_from_service_info(&info) {
                    receivers.insert(receiver.id.clone(), receiver);
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }

    let _ = daemon.stop_browse(AIRPLAY_SERVICE);
    let _ = daemon.shutdown();

    let mut receivers: Vec<_> = receivers.into_values().collect();
    receivers.sort_by(|left, right| {
        left.name
            .to_ascii_lowercase()
            .cmp(&right.name.to_ascii_lowercase())
            .then_with(|| left.host.cmp(&right.host))
    });
    Ok(receivers)
}

fn receiver_from_service_info(info: &ResolvedService) -> Result<Receiver, DiscoveryError> {
    let txt = txt_properties(info);
    let features = txt
        .get("features")
        .or_else(|| txt.get("ft"))
        .and_then(|value| parse_features(value));
    let host = preferred_host(info)
        .ok_or_else(|| DiscoveryError::InvalidRecord("record has no IPv4 address".into()))?;
    let name = instance_name(info.get_fullname());
    let device_id = txt
        .get("deviceid")
        .filter(|value| !value.is_empty())
        .cloned()
        .unwrap_or_else(|| info.get_fullname().to_owned());

    Ok(Receiver {
        id: device_id.to_ascii_uppercase(),
        name,
        model: txt.get("model").cloned().unwrap_or_default(),
        host,
        port: info.get_port(),
        addresses: sorted_addresses(info),
        capabilities: ReceiverCapabilities::from_features(features),
        source_version: txt
            .get("srcvers")
            .or_else(|| txt.get("sourceversion"))
            .cloned(),
        protocol_version: txt
            .get("protovers")
            .or_else(|| txt.get("protocolversion"))
            .cloned(),
        txt,
    })
}

fn txt_properties(info: &ResolvedService) -> BTreeMap<String, String> {
    info.get_properties()
        .iter()
        .map(|property| {
            (
                property.key().to_ascii_lowercase(),
                property.val_str().to_owned(),
            )
        })
        .collect()
}

fn preferred_host(info: &ResolvedService) -> Option<IpAddr> {
    let addresses: Vec<_> = info
        .get_addresses()
        .iter()
        .map(|address| address.to_ip_addr())
        .collect();
    addresses
        .iter()
        .copied()
        .find(|address| matches!(address, IpAddr::V4(address) if !address.is_loopback()))
        .or_else(|| {
            addresses
                .iter()
                .copied()
                .find(|address| matches!(address, IpAddr::V4(_)))
        })
}

fn sorted_addresses(info: &ResolvedService) -> Vec<IpAddr> {
    let mut addresses: Vec<_> = info
        .get_addresses()
        .iter()
        .map(|address| address.to_ip_addr())
        .collect();
    addresses.sort_by_key(|address| match address {
        IpAddr::V4(address) => (0, u128::from(u32::from(*address))),
        IpAddr::V6(address) => (1, u128::from(*address)),
    });
    addresses
}

fn instance_name(fullname: &str) -> String {
    fullname
        .strip_suffix(AIRPLAY_SERVICE)
        .and_then(|name| name.strip_suffix('.'))
        .unwrap_or(fullname)
        .replace("\\032", " ")
}

pub fn socket_address(receiver: &Receiver) -> SocketAddr {
    SocketAddr::new(receiver.host, receiver.port)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn extracts_instance_name() {
        assert_eq!(
            instance_name("Living Room._airplay._tcp.local."),
            "Living Room"
        );
    }

    #[test]
    fn keeps_unknown_name() {
        assert_eq!(instance_name("unexpected"), "unexpected");
    }

    #[test]
    fn socket_address_uses_receiver_port() {
        let receiver = Receiver {
            id: "id".into(),
            name: "TV".into(),
            model: String::new(),
            host: IpAddr::V4(Ipv4Addr::new(192, 168, 1, 20)),
            port: 7000,
            addresses: Vec::new(),
            capabilities: ReceiverCapabilities::default(),
            source_version: None,
            protocol_version: None,
            txt: BTreeMap::new(),
        };
        assert_eq!(
            socket_address(&receiver),
            "192.168.1.20:7000".parse().unwrap()
        );
    }
}
