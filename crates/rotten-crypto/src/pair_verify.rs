use aes::Aes128;
use ctr::Ctr128BE;
use ctr::cipher::{KeyIvInit, StreamCipher};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use rand::RngCore;
use sha2::{Digest, Sha512};
use x25519_dalek::{PublicKey, StaticSecret};

use rotten_core::error::{Result, RottenError};

type Aes128Ctr = Ctr128BE<Aes128>;

/// Build pair-verify step 1 request body and ephemeral X25519 key material.
pub fn pair_verify_step1(client_ed25519_pk: &[u8; 32]) -> ([u8; 68], StaticSecret, [u8; 32]) {
    let mut eph_sk_bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut eph_sk_bytes);
    let secret = StaticSecret::from(eph_sk_bytes);
    let eph_pk = PublicKey::from(&secret).to_bytes();

    let mut body = [0u8; 68];
    body[0] = 1;
    body[4..36].copy_from_slice(&eph_pk);
    body[36..68].copy_from_slice(client_ed25519_pk);

    (body, secret, eph_pk)
}

/// Process pair-verify step 1 response and build step 2 request body.
pub fn pair_verify_step2(
    eph_secret: &StaticSecret,
    client_eph_pk: &[u8; 32],
    client_ed25519_sk: &[u8; 32],
    step1_response: &[u8],
    server_ed25519_pk: &[u8; 32],
) -> Result<([u8; 68], [u8; 32])> {
    if step1_response.len() < 96 {
        return Err(RottenError::Crypto(format!(
            "pair-verify step1 response too short: {}",
            step1_response.len()
        )));
    }

    let server_eph_pk = array32(&step1_response[0..32])?;
    let shared = eph_secret.diffie_hellman(&PublicKey::from(server_eph_pk));
    let shared_secret = *shared.as_bytes();
    let (aes_key, aes_iv) = derive_pair_verify_aes(shared.as_bytes());

    let mut cipher = Aes128Ctr::new(&aes_key.into(), &aes_iv.into());
    let mut server_sig = step1_response[32..96].to_vec();
    cipher.apply_keystream(&mut server_sig);

    let mut signed = Vec::with_capacity(64);
    signed.extend_from_slice(&server_eph_pk);
    signed.extend_from_slice(client_eph_pk);
    verify_ed25519(server_ed25519_pk, &signed, &server_sig)?;

    let signing_key = SigningKey::from_bytes(client_ed25519_sk);
    let mut client_message = Vec::with_capacity(64);
    client_message.extend_from_slice(client_eph_pk);
    client_message.extend_from_slice(&server_eph_pk);
    let client_sig = signing_key.sign(&client_message);

    let mut encrypted_sig = client_sig.to_bytes().to_vec();
    cipher.apply_keystream(&mut encrypted_sig);

    let mut body = [0u8; 68];
    body[4..68].copy_from_slice(&encrypted_sig);
    Ok((body, shared_secret))
}

fn derive_pair_verify_aes(shared_secret: &[u8]) -> ([u8; 16], [u8; 16]) {
    let mut key_hasher = Sha512::new();
    key_hasher.update(b"Pair-Verify-AES-Key");
    key_hasher.update(shared_secret);
    let key_digest = key_hasher.finalize();

    let mut iv_hasher = Sha512::new();
    iv_hasher.update(b"Pair-Verify-AES-IV");
    iv_hasher.update(shared_secret);
    let iv_digest = iv_hasher.finalize();

    let mut aes_key = [0u8; 16];
    let mut aes_iv = [0u8; 16];
    aes_key.copy_from_slice(&key_digest[..16]);
    aes_iv.copy_from_slice(&iv_digest[..16]);
    (aes_key, aes_iv)
}

fn verify_ed25519(pk: &[u8; 32], message: &[u8], sig_bytes: &[u8]) -> Result<()> {
    let sig_array: [u8; 64] = sig_bytes
        .try_into()
        .map_err(|_| RottenError::Crypto("invalid ed25519 signature length".into()))?;
    let verifying_key =
        VerifyingKey::from_bytes(pk).map_err(|e| RottenError::Crypto(e.to_string()))?;
    let signature = Signature::from_bytes(&sig_array);
    verifying_key
        .verify_strict(message, &signature)
        .map_err(|e| RottenError::Crypto(format!("pair-verify server signature: {e}")))
}

fn array32(bytes: &[u8]) -> Result<[u8; 32]> {
    bytes
        .try_into()
        .map_err(|_| RottenError::Crypto("expected 32-byte value".into()))
}
