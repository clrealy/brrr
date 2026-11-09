//! Encrypted IPP. Printers use self-signed certs, so a CA check is useless.
//! Instead we remember each printer's cert the first time and refuse if it changes.
use crate::ipp::Uri;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, ClientConnection, DigitallySignedStruct, SignatureScheme, StreamOwned};
use std::collections::HashMap;
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

static FILE_LOCK: Mutex<()> = Mutex::new(());

fn pins_path() -> Option<PathBuf> {
    Some(crate::config_dir()?.join("known_printers"))
}

fn load() -> HashMap<String, String> {
    let Some(p) = pins_path() else { return HashMap::new() };
    std::fs::read_to_string(p).unwrap_or_default().lines()
        .filter_map(|l| l.rsplit_once(' ').map(|(a, b)| (a.to_string(), b.to_string()))).collect()
}

fn save(all: &HashMap<String, String>) {
    let Some(p) = pins_path() else { return };
    if let Some(d) = p.parent() { let _ = std::fs::create_dir_all(d); }
    let mut lines: Vec<String> = all.iter().map(|(k, v)| format!("{k} {v}")).collect();
    lines.sort();
    let _ = std::fs::write(p, lines.join("\n") + "\n");
}

pub fn pinned(uri: &Uri) -> bool {
    let _g = FILE_LOCK.lock();
    load().contains_key(&uri.id)
}

pub fn forget(id: &str) -> bool {
    let _g = FILE_LOCK.lock();
    let mut all = load();
    let had = all.remove(id).is_some();
    if had { save(&all); }
    had
}

fn fingerprint(cert: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, cert).as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Debug)]
struct Pin {
    id: String,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for Pin {
    fn verify_server_cert(&self, cert: &CertificateDer<'_>, _: &[CertificateDer<'_>], _: &ServerName<'_>, _: &[u8], _: UnixTime) -> Result<ServerCertVerified, rustls::Error> {
        let fp = fingerprint(cert.as_ref());
        let _g = FILE_LOCK.lock();
        let mut all = load();
        match all.get(&self.id) {
            Some(known) if *known == fp => {}
            Some(_) => return Err(rustls::Error::General("PIN_MISMATCH".into())),
            None => { all.insert(self.id.clone(), fp); save(&all); }
        }
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(m, c, d, &self.provider.signature_verification_algorithms)
    }
    fn verify_tls13_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(m, c, d, &self.provider.signature_verification_algorithms)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

pub fn wrap(uri: &Uri, tcp: TcpStream) -> Result<StreamOwned<ClientConnection, TcpStream>, String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cfg = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions().map_err(|e| e.to_string())?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Pin { id: uri.id.clone(), provider }))
        .with_no_client_auth();
    let name = ServerName::try_from(uri.host.clone()).map_err(|e| e.to_string())?;
    let conn = ClientConnection::new(Arc::new(cfg), name).map_err(|e| e.to_string())?;
    let mut s = StreamOwned::new(conn, tcp);
    while s.conn.is_handshaking() {
        s.conn.complete_io(&mut s.sock).map_err(|e| {
            if e.to_string().contains("PIN_MISMATCH") {
                format!("{}'s security certificate changed. if you reset or replaced the printer, run `brrr forget` on it. if not, something may be impersonating it, so brrr won't send your file", uri.host)
            } else {
                format!("encrypted connection to {} failed: {e}", uri.host)
            }
        })?;
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fingerprint_is_stable_hex() {
        let f = fingerprint(b"abc");
        assert_eq!(f, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }
}
