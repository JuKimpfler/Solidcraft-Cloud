//! Datei-Server für SolidCraft_Cloud.
//!
//! Speichert Designs als Dateien in einem Ordner, schützt Schreibzugriffe mit ETags (kein
//! stilles Überschreiben) und kennt Sperren mit Ablauf, damit immer nur eine Person ein Design
//! bearbeitet. Der Entwurf steht in `cloud/ARCHITECTURE.md`.
#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod api;
pub mod config;
pub mod locks;
pub mod store;

use std::io;

/// `n` Zufallsbytes als Hexzeichen (Sperr-Token, Namen temporärer Dateien).
pub(crate) fn random_hex(n: usize) -> io::Result<String> {
    let mut bytes = vec![0u8; n];
    getrandom::fill(&mut bytes).map_err(|e| io::Error::other(e.to_string()))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
pub(crate) mod testutil {
    use std::path::{Path, PathBuf};

    /// Ein Ordner, der beim Verlassen des Tests verschwindet.
    pub struct TempDir(PathBuf);

    impl TempDir {
        pub fn new() -> TempDir {
            let name = format!("solidcraft-cloud-test-{}", crate::random_hex(8).unwrap());
            let path = std::env::temp_dir().join(name);
            std::fs::create_dir_all(&path).unwrap();
            TempDir(path)
        }

        pub fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
