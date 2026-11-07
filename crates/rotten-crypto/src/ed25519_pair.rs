use ed25519_dalek::SigningKey;
use rand::rngs::OsRng;

/// Ed25519 keypair used for AirPlay pair-verify.
pub struct Ed25519KeyPair {
    pub public_key: [u8; 32],
    pub private_key: [u8; 32],
}

pub fn generate_ed25519_keypair() -> Ed25519KeyPair {
    let signing_key = SigningKey::generate(&mut OsRng);
    let verifying_key = signing_key.verifying_key();
    Ed25519KeyPair {
        public_key: verifying_key.to_bytes(),
        private_key: signing_key.to_bytes(),
    }
}
