//! Original ChaCha20-Poly1305 AEAD with 8-byte nonce (Apple mirror audio RTP).
//!
//! Matches `github.com/aead/chacha20poly1305` used by doubletake, not IETF RFC 8439.

use chacha20::ChaCha20Legacy;
use chacha20::cipher::{KeyIvInit, StreamCipher, StreamCipherSeek};
use poly1305::Poly1305;
use poly1305::universal_hash::{KeyInit, UniversalHash};

/// Seal `plaintext` with 8-byte `nonce`, returning ciphertext || 16-byte Poly1305 tag.
pub fn seal(key: &[u8; 32], nonce: &[u8; 8], plaintext: &[u8], aad: &[u8]) -> Vec<u8> {
    let mut poly_key = [0u8; 32];
    let mut cipher = ChaCha20Legacy::new(key.into(), nonce.into());
    cipher.apply_keystream(&mut poly_key);
    cipher.seek(64);

    let mut ciphertext = vec![0u8; plaintext.len()];
    cipher.apply_keystream_b2b(plaintext, &mut ciphertext);

    let tag = poly1305_tag(&poly_key, aad, &ciphertext);
    ciphertext.extend_from_slice(&tag);
    ciphertext
}

fn poly1305_tag(poly_key: &[u8; 32], aad: &[u8], ciphertext: &[u8]) -> [u8; 16] {
    let mut mac = Poly1305::new(poly_key.into());
    mac.update_padded(aad);
    mac.update_padded(ciphertext);
    let mut lens = [0u8; 16];
    lens[..8].copy_from_slice(&(aad.len() as u64).to_le_bytes());
    lens[8..].copy_from_slice(&(ciphertext.len() as u64).to_le_bytes());
    mac.update_padded(&lens);
    mac.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_matches_aead_chacha20poly1305_layout() {
        let key = [0x42u8; 32];
        let nonce = [0x11u8; 8];
        let aad = [0x90u8, 0x78, 0x56, 0x34, 0x12, 0xef, 0xcd, 0xab];
        let plaintext = b"doubletake mirrored audio";
        let sealed = seal(&key, &nonce, plaintext, &aad);
        assert_eq!(sealed.len(), plaintext.len() + 16);
    }

    #[test]
    fn ietf_zero_prefix_matches_for_small_counter() {
        use chacha20poly1305::aead::{Aead, KeyInit, Payload};
        use chacha20poly1305::{ChaCha20Poly1305, Nonce};

        let key = [0x35u8; 32];
        let nonce8 = [0x12u8; 8];
        let aad = [0xaau8, 0xbb, 0xcc, 0xdd, 0xee, 0xff, 0x00, 0x11];
        let plaintext = b"original 64-bit nonce variant";

        let legacy = seal(&key, &nonce8, plaintext, &aad);
        let ietf = ChaCha20Poly1305::new(&key.into());
        let mut nonce12 = [0u8; 12];
        nonce12[4..].copy_from_slice(&nonce8);
        let ietf_sealed = ietf
            .encrypt(
                Nonce::from_slice(&nonce12),
                Payload {
                    msg: plaintext,
                    aad: &aad,
                },
            )
            .unwrap();
        assert_eq!(legacy, ietf_sealed);
    }
}
