//! [`TrustStore`] over `app.sqlite`: permanently trusted certificates survive a restart.
//!
//! The transport asks [`TrustStore::pinned`] from inside the TLS handshake, which must not wait
//! for the database. The pins are therefore loaded into memory once at start-up and every change
//! goes to the map first; the database is the durable copy.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};

use svn_ra::transport::TrustStore;

use crate::db::{AppDb, DbError};

type PinMap = HashMap<(String, u16), Vec<[u8; 32]>>;

/// Pinned certificate fingerprints, kept in memory and persisted in `trusted_cert`.
pub struct AppDbTrustStore {
    db: AppDb,
    pins: Mutex<PinMap>,
}

fn key(host: &str, port: u16) -> (String, u16) {
    (host.to_ascii_lowercase(), port)
}

/// Lower-case hex of a fingerprint, as stored in `trusted_cert.sha256`.
pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 || !s.is_ascii() {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

impl AppDbTrustStore {
    /// Loads every trusted certificate of `db` into memory. Rows that do not hold a SHA-256
    /// fingerprint are skipped (they could never match a certificate).
    pub fn load(db: AppDb) -> Result<Self, DbError> {
        let mut pins = PinMap::new();
        for cert in db.certs_list()? {
            match unhex(&cert.sha256) {
                Some(fp) => pins.entry(key(&cert.host, cert.port)).or_default().push(fp),
                None => tracing::warn!(host = %cert.host, "ignoring malformed trusted certificate"),
            }
        }
        Ok(Self {
            db,
            pins: Mutex::new(pins),
        })
    }

    /// Store without pins (used when the table cannot be read; certificates are asked again).
    pub fn empty(db: AppDb) -> Self {
        Self {
            db,
            pins: Mutex::default(),
        }
    }

    fn map(&self) -> std::sync::MutexGuard<'_, PinMap> {
        self.pins.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Stops trusting one certificate (in memory; the caller removed the row).
    pub fn forget(&self, host: &str, port: u16, sha256_hex: &str) {
        let Some(fp) = unhex(&sha256_hex.to_ascii_lowercase()) else {
            return;
        };
        let mut pins = self.map();
        let k = key(host, port);
        if let Some(list) = pins.get_mut(&k) {
            list.retain(|p| *p != fp);
            if list.is_empty() {
                pins.remove(&k);
            }
        }
    }
}

impl TrustStore for AppDbTrustStore {
    fn pinned(&self, host: &str, port: u16) -> Vec<[u8; 32]> {
        self.map()
            .get(&key(host, port))
            .cloned()
            .unwrap_or_default()
    }

    fn pin(&self, host: &str, port: u16, sha256: [u8; 32]) {
        {
            let mut pins = self.map();
            let list = pins.entry(key(host, port)).or_default();
            if !list.contains(&sha256) {
                list.push(sha256);
            }
        }
        // The broker normally stored the row already (with subject and expiry) when the user chose
        // "accept always"; this covers any other prompter.
        let fp = hex(&sha256);
        let host = host.to_ascii_lowercase();
        if !self.db.cert_is_trusted(&host, port, &fp).unwrap_or(false)
            && let Err(e) = self.db.cert_trust(&host, port, &fp, "", "")
        {
            tracing::warn!(error = %e, "could not store trusted certificate");
        }
    }

    fn replace_pins(&self, host: &str, port: u16, sha256: [u8; 32]) {
        self.map().insert(key(host, port), vec![sha256]);
        let fp = hex(&sha256);
        let host = host.to_ascii_lowercase();
        if !self.db.cert_is_trusted(&host, port, &fp).unwrap_or(false)
            && let Err(e) = self.db.cert_replace_pin(&host, port, &fp, "", "")
        {
            tracing::warn!(error = %e, "could not replace the pinned certificate");
        }
    }
}
