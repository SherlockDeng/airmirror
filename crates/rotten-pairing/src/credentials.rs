use rotten_core::config::{CredentialsStore, DeviceCredentials};
use rotten_core::device::AirPlayDevice;
use rotten_core::error::Result;

use crate::transient::pair_transient;

/// Manages pairing state and credential persistence.
pub struct PairingManager {
    store: CredentialsStore,
    path: std::path::PathBuf,
}

impl PairingManager {
    pub fn load(path: std::path::PathBuf) -> Result<Self> {
        let store = CredentialsStore::load(&path)?;
        Ok(Self { store, path })
    }

    pub fn has_credentials(&self, device_id: &str) -> bool {
        self.store.get(device_id).is_some()
    }

    pub fn get_credentials(&self, device_id: &str) -> Option<&DeviceCredentials> {
        self.store.get(device_id)
    }

    /// Return stored credentials or establish a fresh transient pairing.
    ///
    /// AirMirror intentionally does not support PIN-based pairing.
    pub async fn pair(&mut self, device: &AirPlayDevice, force: bool) -> Result<DeviceCredentials> {
        if !force {
            if let Some(creds) = self.store.get(&device.device_id) {
                return Ok(creds.clone());
            }
        }

        let creds = pair_transient(device).await?;
        // Some receivers commit a new transient identity asynchronously and
        // reset the first media session if pair-verify starts immediately.
        tokio::time::sleep(std::time::Duration::from_millis(750)).await;

        self.store.upsert(creds.clone());
        self.store.save(&self.path)?;
        Ok(creds)
    }

    pub fn remove(&mut self, device_id: &str) -> Result<()> {
        self.store.remove(device_id);
        self.store.save(&self.path)
    }
}
