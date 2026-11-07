//! AES-128-CTR mirror video encryption (legacy / raw pair-verify path).
//!
//! Matches doubletake `mirrorCipher` and UxPlay `mirror_buffer_decrypt` block alignment.

use aes::Aes128;
use ctr::cipher::{KeyIvInit, StreamCipher as _};
use sha2::{Digest, Sha512};

type Aes128Ctr = ctr::Ctr128BE<Aes128>;

/// Derive AES-128-CTR key/IV for mirror video (SHA-512 over label + stream id + shk).
pub fn derive_mirror_video_keys(shk: &[u8; 16], stream_connection_id: i64) -> ([u8; 16], [u8; 16]) {
    let id = stream_connection_id as u64;

    let mut h = Sha512::new();
    h.update(format!("AirPlayStreamKey{id}"));
    h.update(shk);
    let key: [u8; 16] = h.finalize()[..16].try_into().expect("sha512 key");

    let mut h = Sha512::new();
    h.update(format!("AirPlayStreamIV{id}"));
    h.update(shk);
    let iv: [u8; 16] = h.finalize()[..16].try_into().expect("sha512 iv");

    (key, iv)
}

/// Stateful AES-CTR cipher matching receiver block-boundary semantics across frames.
pub struct MirrorAesCtr {
    stream: Aes128Ctr,
    block_offset: usize,
    og: [u8; 16],
    next_crypt_count: usize,
}

impl MirrorAesCtr {
    pub fn new(key: &[u8; 16], iv: &[u8; 16]) -> Self {
        Self {
            stream: Aes128Ctr::new(key.into(), iv.into()),
            block_offset: 0,
            og: [0u8; 16],
            next_crypt_count: 0,
        }
    }

    /// Encrypt one VCL payload (ciphertext length equals plaintext length).
    pub fn encrypt_frame(&mut self, payload: &[u8]) -> Vec<u8> {
        let input_len = payload.len();
        let mut out = vec![0u8; input_len];
        let mut pos = 0usize;

        if self.next_crypt_count > 0 {
            let n = self.next_crypt_count.min(input_len);
            let og_start = 16 - self.next_crypt_count;
            for i in 0..n {
                out[i] = payload[i] ^ self.og[og_start + i];
            }
            pos = n;
        }

        if self.block_offset > 0 {
            let waste_len = 16 - self.block_offset;
            let mut waste = vec![0u8; waste_len];
            self.stream.apply_keystream(&mut waste);
            self.block_offset = 0;
        }

        let remaining = input_len - pos;
        let full_blocks = (remaining / 16) * 16;
        if full_blocks > 0 {
            out[pos..pos + full_blocks].copy_from_slice(&payload[pos..pos + full_blocks]);
            self.stream
                .apply_keystream(&mut out[pos..pos + full_blocks]);
            self.block_offset = 0;
            pos += full_blocks;
        }

        let rest_len = remaining % 16;
        self.next_crypt_count = 0;
        if rest_len > 0 {
            let mut padded = [0u8; 16];
            padded[..rest_len].copy_from_slice(&payload[pos..pos + rest_len]);
            self.stream.apply_keystream(&mut padded);
            out[pos..pos + rest_len].copy_from_slice(&padded[..rest_len]);
            self.og = padded;
            self.next_crypt_count = 16 - rest_len;
            self.block_offset = 0;
        }

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_keys_deterministic() {
        let shk = [0x11u8; 16];
        let (k1, iv1) = derive_mirror_video_keys(&shk, 42);
        let (k2, iv2) = derive_mirror_video_keys(&shk, 42);
        assert_eq!(k1, k2);
        assert_eq!(iv1, iv2);
        assert_ne!(k1, iv1);
    }

    #[test]
    fn encrypt_preserves_length() {
        let key = [0u8; 16];
        let iv = [1u8; 16];
        let mut c = MirrorAesCtr::new(&key, &iv);
        for len in [1, 15, 16, 17, 32, 100, 1352] {
            let plain = vec![0xABu8; len];
            let enc = c.encrypt_frame(&plain);
            assert_eq!(enc.len(), len);
        }
    }
}
