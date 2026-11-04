//! Certificate subject and validity helpers.
use crate::Result;
use anyhow::ensure;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use openssl::{asn1::Asn1Time, x509::X509};
use sha2::{Digest, Sha256};

pub fn device_subject(uid: &str, device_id: &str, device_type: u32) -> String {
    let device = URL_SAFE_NO_PAD.encode(Sha256::digest(device_id.as_bytes()));
    let account = URL_SAFE_NO_PAD.encode(Sha256::digest(format!("{device}{uid}").as_bytes()));
    format!("lyra.{account}.{device}.{device_type}")
}

pub(crate) fn validate_dates(cert: &X509) -> Result<()> {
    let now = Asn1Time::days_from_now(0)?;
    ensure!(
        cert.not_before() <= now.as_ref() && cert.not_after() >= now.as_ref(),
        "账号证书已过期或尚未生效"
    );
    Ok(())
}
