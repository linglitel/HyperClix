//! Symmetric helpers shared by authentication and SDK channels.
use crate::Result;
use anyhow::ensure;
use openssl::symm::{Cipher, decrypt_aead, encrypt_aead};
use rand::RngCore;

fn random(n: usize) -> Vec<u8> {
    let mut bytes = vec![0; n];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes
}

pub(crate) fn aes_encrypt(key: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    let iv = random(12);
    let mut tag = [0; 16];
    let encrypted = encrypt_aead(Cipher::aes_256_gcm(), key, Some(&iv), &[], data, &mut tag)?;
    Ok([iv, encrypted, tag.to_vec()].concat())
}

pub(crate) fn aes_decrypt(key: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    ensure!(data.len() >= 28, "Truncated AES-GCM packet");
    Ok(decrypt_aead(
        Cipher::aes_256_gcm(),
        key,
        Some(&data[..12]),
        &[],
        &data[12..data.len() - 16],
        &data[data.len() - 16..],
    )?)
}
