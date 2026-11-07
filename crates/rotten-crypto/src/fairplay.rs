use async_trait::async_trait;
use rotten_core::error::{Result, RottenError};

/// FairPlay SAP authentication provider.
#[async_trait]
pub trait SapProvider: Send + Sync {
    async fn generate_sap_setup(&self, data: &[u8]) -> Result<Vec<u8>>;
    async fn decrypt_key(&self, encrypted: &[u8]) -> Result<Vec<u8>>;
}

/// Stub SAP provider for development / protocol testing without full FairPlay.
pub struct StubSapProvider;

#[async_trait]
impl SapProvider for StubSapProvider {
    async fn generate_sap_setup(&self, data: &[u8]) -> Result<Vec<u8>> {
        Ok(data.to_vec())
    }

    async fn decrypt_key(&self, encrypted: &[u8]) -> Result<Vec<u8>> {
        if encrypted.is_empty() {
            return Err(RottenError::Crypto("empty SAP key".into()));
        }
        Ok(encrypted.to_vec())
    }
}

/// FairPlay SAP wrapper delegating to a provider implementation.
pub struct FairPlaySap<P: SapProvider> {
    provider: P,
}

impl<P: SapProvider> FairPlaySap<P> {
    pub fn new(provider: P) -> Self {
        Self { provider }
    }

    pub async fn setup(&self, payload: &[u8]) -> Result<Vec<u8>> {
        self.provider.generate_sap_setup(payload).await
    }

    pub async fn decrypt_aes_key(&self, encrypted: &[u8]) -> Result<Vec<u8>> {
        self.provider.decrypt_key(encrypted).await
    }
}
