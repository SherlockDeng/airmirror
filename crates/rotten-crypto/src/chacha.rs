use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use rotten_core::error::{Result, RottenError};

/// ChaCha20-Poly1305 stream cipher for AirPlay 2 encrypted transport.
pub struct StreamCipher {
    cipher: ChaCha20Poly1305,
    counter: u64,
}

impl StreamCipher {
    pub fn new(key: &[u8; 32]) -> Self {
        Self {
            cipher: ChaCha20Poly1305::new(key.into()),
            counter: 0,
        }
    }

    pub fn encrypt(&mut self, plaintext: &[u8], aad: &[u8]) -> Result<Vec<u8>> {
        let nonce = self.next_nonce();
        self.encrypt_with_nonce(plaintext, aad, &nonce)
    }

    /// Encrypt mirroring VCL with the 128-byte packet header as AEAD associated data.
    pub fn encrypt_mirror_vcl(&mut self, plaintext: &[u8], header: &[u8; 128]) -> Result<Vec<u8>> {
        let nonce = self.next_nonce();
        self.encrypt_with_nonce(plaintext, header, &nonce)
    }

    fn encrypt_with_nonce(&self, plaintext: &[u8], aad: &[u8], nonce: &Nonce) -> Result<Vec<u8>> {
        let ciphertext = self
            .cipher
            .encrypt(
                nonce,
                chacha20poly1305::aead::Payload {
                    msg: plaintext,
                    aad,
                },
            )
            .map_err(|e| RottenError::Crypto(format!("encrypt: {e}")))?;
        Ok(ciphertext)
    }

    pub fn decrypt(&mut self, ciphertext: &[u8], aad: &[u8]) -> Result<Vec<u8>> {
        let nonce = self.next_nonce();
        self.cipher
            .decrypt(
                &nonce,
                chacha20poly1305::aead::Payload {
                    msg: ciphertext,
                    aad,
                },
            )
            .map_err(|e| RottenError::Crypto(format!("decrypt: {e}")))
    }

    fn next_nonce(&mut self) -> Nonce {
        let mut nonce_bytes = [0u8; 12];
        nonce_bytes[4..12].copy_from_slice(&self.counter.to_le_bytes());
        self.counter += 1;
        *Nonce::from_slice(&nonce_bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let key = [7u8; 32];
        let mut enc = StreamCipher::new(&key);
        let mut dec = StreamCipher::new(&key);
        let pt = b"hello airplay";
        let ct = enc.encrypt(pt, b"aad").unwrap();
        let out = dec.decrypt(&ct, b"aad").unwrap();
        assert_eq!(out, pt);
    }
}
