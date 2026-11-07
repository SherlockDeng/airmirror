//! FairPlay SAP client (fp-setup + AES key wrapping).

use crate::fpsap_helper::fpsap_hash_from_setup1;
use fpsap_rs::fpsap::decrypt_fpsap_body;
use rand::RngCore;
use rotten_core::error::{Result, RottenError};
use sha2::{Digest, Sha512};

/// Result of a completed FairPlay fp-setup handshake.
#[derive(Debug, Clone)]
pub struct FairPlaySession {
    pub key_message: [u8; 164],
    pub mode: u8,
    pub receiver_sap: [u8; 128],
}

/// FairPlay keys for RTSP mirror SETUP (ekey + hashed stream key material).
#[derive(Debug, Clone)]
pub struct MirrorFpKeys {
    pub ekey: [u8; 72],
    /// Raw key supplied in the stream descriptor.
    pub fp_key: [u8; 16],
    /// Key used by the sender for the encrypted VCL stream.
    pub stream_key: [u8; 16],
    pub fp_iv: [u8; 16],
    pub fp_aes_key: [u8; 16],
}

const FP_SETUP1: [u8; 16] = [
    0x46, 0x50, 0x4c, 0x59, 0x03, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x04, 0x02, 0x00, 0x03, 0xbb,
];

/// Precomputed FairPlay key messages (mode 0..3) from openairplay/airplay2-receiver fp_decrypt.py.
const KEY_MESSAGES: [&[u8; 164]; 4] = [
    &hex_const(
        "46504c590301030000000098008f1a9ca548fdd57560a52926ff399f2eb154d0a7a0fffc997f58e27e00499eb9f310110d019e550e328047aea54308ab71b647041406878af96e06cf74127ae35941dceb58931b5543b39903f9f76a376248ee52e3656b561e1c1a0106ec6608df0ab4f2df528e65db6d622d3892d5b49c6c025606a574f19ebea7d93500bdd69db23333f22edcb3ccf7a6acde7389f2facabfa61b0b50",
    ),
    &hex_const(
        "46504c590301030000000098018f1a9c144a77fb15383f69cf6ba6ae3504582d489a121c644dac40bfb382388d758b294841cbe51bb4feea983f9157a1fb2e57765d1bfc7262053ca6f75c90c82794a43b8d844637aa018c28619a43da6727c7faf81b911a92f317d6ac7a3e1a7b923fe693cffb37317159be8904556862d81ed4f794957dc330b7e681e5a0067f596a0f3f936dd761f5afa2d69ab77938328bb1fcd92e",
    ),
    &hex_const(
        "46504c590301030000000098028f1a9c049ae04d1691802802c75b3ced9204acbeb5482b582f4faf3c008d7dd3675a37967e3bee3079bec95b8bfeea69aaec8233c7ab3b7df283e8f9a50b8ecdcc53e3ee2e5ee1d78421378fdf8cfa1e1c04995d3c6f14b47e9487f3458cc6e4727fe1e3ad2b1db60afdb590c14da5011404ab0972c15ab14ad6a71ebd8cab10098bc1b1822b14dd3e496fe13bfcca8fd399eee52581d4",
    ),
    &hex_const(
        "46504c590301030000000098038f1a9c63b126a2325158ff9ba6e5d9996b33cf4fda80c7bb3492ee15790fa57b5cdf0129fc3d29a2f6fb66ef4494aca148ddff2b726df69de41caf7292a3a960673699430df2eb5742ab52e61d5ef576ffbfb960686c14976dce6c66b07b491b94a41b445152a4449221fc704645fe1d5af60f446c3dbf1030580eb2c0002e387053fc9783d04dfba861da3f1444664737bcc2d82f4a5c",
    ),
];

/// 144-byte fp-setup step2 header used by real Apple TV clients (differs from openairplay test vectors after byte 16).
const M3_PREFIX: [u8; 144] = hex_const_144(
    "46504c590301030000000098038f1a9c991ea22c511e45ba97f1af8dfb0f86f5\
     50c54486fe6b3ab233da431ef8e5fc1156dba321fffeabb1b392b09d227e88c7\
     12202866eb7bbf310015aa1d19a5df36d5dfd8d3ca1639b376eaece946edfe8b\
     7a66cd302d04aac3c1251714019bd5f2d49b543e11eed1646291ec8efd96b691\
     01b849fd93a02860d1a0dff5cd4414aa",
);

const fn hex_const_144(hex: &str) -> [u8; 144] {
    let bytes = hex.as_bytes();
    let mut out = [0u8; 144];
    let mut i = 0;
    let mut j = 0;
    while i < 144 {
        while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\n' || bytes[j] == b'\r') {
            j += 1;
        }
        let hi = hex_nibble(bytes[j]);
        j += 1;
        while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\n' || bytes[j] == b'\r') {
            j += 1;
        }
        let lo = hex_nibble(bytes[j]);
        j += 1;
        out[i] = (hi << 4) | lo;
        i += 1;
    }
    out
}

const fn hex_const(hex: &str) -> [u8; 164] {
    let bytes = hex.as_bytes();
    let mut out = [0u8; 164];
    let mut i = 0;
    while i < 164 {
        let hi = hex_nibble(bytes[i * 2]);
        let lo = hex_nibble(bytes[i * 2 + 1]);
        out[i] = (hi << 4) | lo;
        i += 1;
    }
    out
}

const fn hex_nibble(b: u8) -> u8 {
    match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => b - b'a' + 10,
        b'A'..=b'F' => b - b'A' + 10,
        _ => 0,
    }
}

impl FairPlaySession {
    pub fn fp_setup1_request() -> &'static [u8; 16] {
        &FP_SETUP1
    }

    /// Mode byte from the client fp-setup step1 request (byte 14), not the server reply.
    pub fn fp_setup_mode() -> u8 {
        FP_SETUP1[14].min(3)
    }

    pub fn key_message_for_mode(mode: u8) -> Result<[u8; 164]> {
        let idx = (mode as usize).min(3);
        Ok(*KEY_MESSAGES[idx])
    }

    /// Build the 164-byte fp-setup step2 message from the 142-byte step1 response.
    pub fn key_message_from_setup1(setup1: &[u8], mode: u8) -> Result<[u8; 164]> {
        let tail = fpsap_hash_from_setup1(setup1)?;
        let mut msg = [0u8; 164];
        msg[..144].copy_from_slice(&M3_PREFIX);
        msg[144..].copy_from_slice(&tail);
        let _ = mode;
        Ok(msg)
    }

    /// Recover the receiver's per-session SAP from the m2 payload. The ekey
    /// wrapper must be generated from this value, not from the fixed legacy
    /// test SAP used by some older FairPlay implementations.
    pub fn receiver_sap_from_setup1(setup1: &[u8]) -> Result<[u8; 128]> {
        if setup1.len() != 142 {
            return Err(RottenError::Protocol(format!(
                "FairPlay m2 length {}, expected 142",
                setup1.len()
            )));
        }
        let mode = setup1[13];
        let mut payload = [0u8; 128];
        payload.copy_from_slice(&setup1[14..142]);
        decrypt_fpsap_body(mode, payload)
            .map_err(|e| RottenError::Crypto(format!("FairPlay m2 SAP: {e}")))
    }

    pub fn m3_prefix() -> &'static [u8; 144] {
        &M3_PREFIX
    }

    pub fn from_mode(mode: u8) -> Result<Self> {
        Ok(Self {
            key_message: Self::key_message_for_mode(mode)?,
            mode,
            receiver_sap: [0u8; 128],
        })
    }

    pub fn from_key_message(key_message: [u8; 164], mode: u8, receiver_sap: [u8; 128]) -> Self {
        Self {
            key_message,
            mode,
            receiver_sap,
        }
    }

    /// Wrap a 16-byte AES key into the 72-byte FairPlay blob (param1 / ekey).
    pub fn encrypt_aes_key(&self, aes_key: &[u8; 16]) -> Result<[u8; 72]> {
        let mask = random_block16();
        fpsap_rs::fpsap::wrap_fair_play_key(&self.receiver_sap, &self.key_message, *aes_key, mask)
            .map_err(|error| RottenError::Crypto(format!("fairplay AES key wrap: {error}")))
    }

    pub fn random_aes_key_iv() -> ([u8; 16], [u8; 16]) {
        let mut key = [0u8; 16];
        let mut iv = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut key);
        rand::thread_rng().fill_bytes(&mut iv);
        (key, iv)
    }

    /// Derive ekey, playfair-decrypted AES key, and hashed fpKey/fpIV for mirror SETUP.
    pub fn derive_mirror_fp_keys(
        &self,
        shared_secret: &[u8; 32],
        mix_pair_verify_secret: bool,
    ) -> Result<MirrorFpKeys> {
        let (fp_aes_key, fp_iv) = Self::random_aes_key_iv();
        let ekey = self.encrypt_aes_key(&fp_aes_key)?;
        let stream_key = if mix_pair_verify_secret {
            hash_fp_key(&fp_aes_key, shared_secret)
        } else {
            fp_aes_key
        };
        Ok(MirrorFpKeys {
            ekey,
            fp_key: fp_aes_key,
            stream_key,
            fp_iv,
            fp_aes_key,
        })
    }
}

fn hash_fp_key(fp_aes_key: &[u8; 16], shared_secret: &[u8; 32]) -> [u8; 16] {
    let mut h = Sha512::new();
    h.update(fp_aes_key);
    h.update(shared_secret);
    let digest = h.finalize();
    let mut out = [0u8; 16];
    out.copy_from_slice(&digest[..16]);
    out
}

fn random_block16() -> [u8; 16] {
    let mut value = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut value);
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn m3_prefix_matches_apple_tv_header() {
        assert_eq!(M3_PREFIX.len(), 144);
        assert_eq!(&M3_PREFIX[0..4], b"FPLY");
        assert_eq!(M3_PREFIX[12], 0x03);
        assert_eq!(M3_PREFIX[16], 0x99);
    }
}
