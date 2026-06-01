use anyhow::{Context, Result};
use rcgen::{CertificateParams, DnType, KeyPair};
use sha2::{Digest, Sha256};
use std::{fs, path::PathBuf};

#[derive(Clone)]
pub struct Identity {
    #[allow(dead_code)]
    pub cert_der: Vec<u8>,
    pub key_pem: String,
    pub cert_pem: String,
    pub fingerprint: String, // hex sha256 of cert DER
}

impl Identity {
    pub fn load_or_create(data_dir: &PathBuf) -> Result<Self> {
        let cert_path = data_dir.join("identity.cert.pem");
        let key_path = data_dir.join("identity.key.pem");

        if cert_path.exists() && key_path.exists() {
            let cert_pem = fs::read_to_string(&cert_path)?;
            let key_pem = fs::read_to_string(&key_path)?;
            let cert_der = pem_to_der(&cert_pem)?;
            let fp = fingerprint_hex(&cert_der);
            return Ok(Self {
                cert_der,
                key_pem,
                cert_pem,
                fingerprint: fp,
            });
        }

        let mut params = CertificateParams::new(vec!["localbeam.local".to_string()])?;
        params
            .distinguished_name
            .push(DnType::CommonName, "LocalBeam");
        let key_pair = KeyPair::generate()?;
        let cert = params.self_signed(&key_pair)?;
        let cert_pem = cert.pem();
        let key_pem = key_pair.serialize_pem();
        let cert_der = cert.der().to_vec();
        let fingerprint = fingerprint_hex(&cert_der);

        fs::create_dir_all(data_dir).ok();
        fs::write(&cert_path, &cert_pem).context("write cert")?;
        fs::write(&key_path, &key_pem).context("write key")?;

        Ok(Self {
            cert_der,
            key_pem,
            cert_pem,
            fingerprint,
        })
    }
}

fn pem_to_der(pem: &str) -> Result<Vec<u8>> {
    let mut cursor = std::io::Cursor::new(pem.as_bytes());
    let mut iter = rustls_pemfile::certs(&mut cursor);
    if let Some(c) = iter.next() {
        let c = c?;
        return Ok(c.as_ref().to_vec());
    }
    anyhow::bail!("no cert in PEM")
}

pub fn fingerprint_hex(cert_der: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(cert_der);
    hex::encode(h.finalize())
}
