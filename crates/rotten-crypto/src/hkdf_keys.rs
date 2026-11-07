use hkdf::Hkdf;
use sha2::Sha512;

/// Derive AirPlay session keys from shared secret material.
pub fn derive_session_keys(shared_secret: &[u8], salt: &[u8], info: &[u8]) -> ([u8; 32], [u8; 32]) {
    let hk = Hkdf::<Sha512>::new(Some(salt), shared_secret);
    let mut control_key = [0u8; 32];
    let mut stream_key = [0u8; 32];
    hk.expand(info, &mut control_key).expect("hkdf control");
    hk.expand(b"AirPlay-Stream-Key", &mut stream_key)
        .expect("hkdf stream");
    (control_key, stream_key)
}

/// HKDF-SHA512 key for encrypted mirroring data channel (ChaCha20-Poly1305).
pub fn derive_data_stream_chacha_key(shared_secret: &[u8], stream_connection_id: i64) -> [u8; 32] {
    let salt = format!("DataStream-Salt{}", stream_connection_id as u64);
    let hk = Hkdf::<Sha512>::new(Some(salt.as_bytes()), shared_secret);
    let mut key = [0u8; 32];
    hk.expand(b"DataStream-Output-Encryption-Key", &mut key)
        .expect("hkdf data stream key");
    key
}
