//! Mirror video stream encryption mode (AES-CTR vs ChaCha20-Poly1305).

use crate::hkdf_keys::derive_data_stream_chacha_key;
use crate::mirror_aes::derive_mirror_video_keys;
use rotten_core::config::MirrorCipherMode;

/// How VCL frames are encrypted on the mirror data TCP socket.
#[derive(Debug, Clone)]
pub enum MirrorVideoCrypto {
    /// No encryption (debug / receivers that accept plaintext VCL).
    None,
    /// Legacy raw pair-verify: AES-128-CTR, payload size = ciphertext size (no Poly1305 tag).
    AesCtr { key: [u8; 16], iv: [u8; 16] },
    /// HAP encrypted pair-verify: ChaCha20-Poly1305 with 128-byte header as AAD.
    ChaCha { key: [u8; 32] },
}

impl MirrorVideoCrypto {
    pub fn from_setup(
        mode: MirrorCipherMode,
        no_encrypt: bool,
        shk: &[u8; 16],
        shared_secret: &[u8; 32],
        stream_connection_id: i64,
    ) -> Self {
        if no_encrypt {
            return Self::None;
        }
        match mode {
            MirrorCipherMode::AesCtr => Self::legacy_aes_from_shk(shk, stream_connection_id),
            MirrorCipherMode::ChaCha => Self::hap_chacha(shared_secret, stream_connection_id),
        }
    }

    pub fn legacy_aes_from_shk(shk: &[u8; 16], stream_connection_id: i64) -> Self {
        let (key, iv) = derive_mirror_video_keys(shk, stream_connection_id);
        Self::AesCtr { key, iv }
    }

    pub fn hap_chacha(shared_secret: &[u8], stream_connection_id: i64) -> Self {
        Self::ChaCha {
            key: derive_data_stream_chacha_key(shared_secret, stream_connection_id),
        }
    }

    pub fn mode_name(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::AesCtr { .. } => "aes-ctr",
            Self::ChaCha { .. } => "chacha",
        }
    }
}
