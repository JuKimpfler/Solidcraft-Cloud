//! Designs als Dateien in einem Ordner.
//!
//! Geschrieben wird immer in eine temporäre Datei (`.tmp/` im selben Ordner) und dann per
//! `rename` an den Zielort gesetzt: Wer gleichzeitig liest, sieht entweder die alte oder die
//! neue Datei, nie eine halbe. Der ETag ändert sich bei jedem Schreiben (auch bei einer Änderung
//! von außen, z. B. durch Nextcloud), weil er aus Inode, Änderungszeit und Größe besteht.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// Erlaubte Endungen: SolidCraft-Designs (neu und alt) und ihr JSON-Format.
pub const EXTENSIONS: [&str; 3] = ["solidcraft", "solvecraft", "json"];
pub const MAX_NAME_BYTES: usize = 200;

/// Prüft einen Dateinamen streng: keine Pfade, keine versteckten Dateien, nur bekannte Endungen.
pub fn valid_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > MAX_NAME_BYTES {
        return Err(format!("Name leer oder länger als {MAX_NAME_BYTES} Bytes"));
    }
    if name.starts_with('.') || name.starts_with(' ') || name.ends_with(' ') || name.contains("..") {
        return Err("Name darf nicht mit Punkt oder Leerzeichen beginnen, nicht mit Leerzeichen enden und kein \"..\" enthalten".into());
    }
    if let Some(bad) = name.chars().find(|c| !(c.is_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.' | '(' | ')' | ','))) {
        return Err(format!("Zeichen {bad:?} ist im Namen nicht erlaubt"));
    }
    let ext = Path::new(name).extension().and_then(|e| e.to_str()).unwrap_or("");
    if !EXTENSIONS.iter().any(|e| e.eq_ignore_ascii_case(ext)) {
        return Err(format!("Endung muss eine von {} sein", EXTENSIONS.join(", ")));
    }
    Ok(())
}

/// Ein gespeichertes Design.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub size: u64,
    pub modified_ms: u64,
    pub etag: String,
}

fn etag_of(meta: &fs::Metadata) -> String {
    format!("\"{:x}-{:x}.{:x}-{:x}\"", meta.ino(), meta.mtime(), meta.mtime_nsec(), meta.len())
}

fn entry_of(name: &str, meta: &fs::Metadata) -> Entry {
    let modified_ms =
        meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).and_then(|d| u64::try_from(d.as_millis()).ok()).unwrap_or(0);
    Entry { name: name.to_string(), size: meta.len(), modified_ms, etag: etag_of(meta) }
}

/// Passt der If-Match-Header (ein ETag, eine Liste davon oder `*`) auf den aktuellen ETag?
pub fn etag_matches(header: &str, current: &str) -> bool {
    header.split(',').map(str::trim).any(|tag| tag == "*" || tag == current)
}

#[derive(Debug)]
pub enum StageError {
    TooLarge,
    Empty,
    Io(io::Error),
}

impl From<io::Error> for StageError {
    fn from(e: io::Error) -> Self {
        StageError::Io(e)
    }
}

#[derive(Debug)]
pub enum CommitError {
    /// Die Datei gibt es schon, aber der Client hat keinen If-Match geschickt.
    PreconditionRequired,
    /// Der If-Match passt nicht (jemand hat die Datei inzwischen geändert, oder sie ist weg).
    PreconditionFailed,
    Io(io::Error),
}

impl From<io::Error> for CommitError {
    fn from(e: io::Error) -> Self {
        CommitError::Io(e)
    }
}

/// Eine vollständig geschriebene, noch nicht veröffentlichte Datei. Wird sie nicht übernommen,
/// verschwindet sie beim Verwerfen.
pub struct Staged {
    path: PathBuf,
}

impl Drop for Staged {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub struct Store {
    dir: PathBuf,
    tmp: PathBuf,
}

impl Store {
    /// Öffnet den Ordner (legt ihn an) und räumt übrig gebliebene temporäre Dateien weg.
    pub fn open(dir: &Path) -> io::Result<Store> {
        let tmp = dir.join(".tmp");
        fs::create_dir_all(&tmp)?;
        for leftover in fs::read_dir(&tmp)?.flatten() {
            let _ = fs::remove_file(leftover.path());
        }
        Ok(Store { dir: dir.to_path_buf(), tmp })
    }

    /// Alle Designs, nach Name sortiert. Versteckte Dateien, Ordner und fremde Endungen fehlen.
    pub fn list(&self) -> io::Result<Vec<Entry>> {
        let mut entries = Vec::new();
        for item in fs::read_dir(&self.dir)?.flatten() {
            let Ok(name) = item.file_name().into_string() else { continue };
            if valid_name(&name).is_err() {
                continue;
            }
            let Ok(meta) = item.metadata() else { continue };
            if meta.is_file() {
                entries.push(entry_of(&name, &meta));
            }
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    pub fn stat(&self, name: &str) -> io::Result<Option<Entry>> {
        match fs::metadata(self.dir.join(name)) {
            Ok(meta) if meta.is_file() => Ok(Some(entry_of(name, &meta))),
            Ok(_) => Ok(None),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Datei zum Lesen öffnen, mit den Metadaten desselben geöffneten Handles (ETag und Inhalt
    /// passen zusammen, auch wenn die Datei gleich danach ersetzt wird).
    pub fn open_read(&self, name: &str) -> io::Result<Option<(File, Entry)>> {
        let file = match File::open(self.dir.join(name)) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let meta = file.metadata()?;
        if !meta.is_file() {
            return Ok(None);
        }
        let entry = entry_of(name, &meta);
        Ok(Some((file, entry)))
    }

    /// Den Inhalt (höchstens `max` Bytes) in eine temporäre Datei schreiben und auf die Platte
    /// zwingen. Hier wird noch nichts veröffentlicht.
    pub fn stage(&self, body: &mut dyn Read, max: u64) -> Result<Staged, StageError> {
        let path = self.tmp.join(format!("{}.part", crate::random_hex(8)?));
        let mut file = File::options().write(true).create_new(true).open(&path)?;
        let staged = Staged { path };
        let mut buf = vec![0u8; 64 * 1024];
        let mut total: u64 = 0;
        loop {
            let n = body.read(&mut buf)?;
            if n == 0 {
                break;
            }
            total = total.saturating_add(n as u64);
            if total > max {
                return Err(StageError::TooLarge);
            }
            file.write_all(buf.get(..n).unwrap_or_default())?;
        }
        if total == 0 {
            return Err(StageError::Empty);
        }
        file.sync_all()?;
        Ok(staged)
    }

    /// Eine vorbereitete Datei als `name` veröffentlichen. Gibt den neuen Eintrag und zurück, ob
    /// die Datei neu angelegt wurde. Der Aufrufer muss Schreibzugriffe selbst serialisieren
    /// (Prüfung und `rename` sind sonst nicht atomar zusammen).
    pub fn commit(&self, name: &str, staged: Staged, if_match: Option<&str>) -> Result<(Entry, bool), CommitError> {
        let current = self.stat(name)?;
        match (&current, if_match) {
            (Some(_), None) => return Err(CommitError::PreconditionRequired),
            (Some(cur), Some(header)) if !etag_matches(header, &cur.etag) => return Err(CommitError::PreconditionFailed),
            (None, Some(_)) => return Err(CommitError::PreconditionFailed),
            _ => {}
        }
        fs::rename(&staged.path, self.dir.join(name))?;
        let entry = self.stat(name)?.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Datei nach dem Speichern nicht gefunden"))?;
        Ok((entry, current.is_none()))
    }

    /// Löscht ein Design. `false`: gab es nicht.
    pub fn delete(&self, name: &str) -> io::Result<bool> {
        match fs::remove_file(self.dir.join(name)) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    fn put(store: &Store, name: &str, data: &[u8], if_match: Option<&str>) -> Result<(Entry, bool), CommitError> {
        let staged = store.stage(&mut &data[..], 1024).unwrap();
        store.commit(name, staged, if_match)
    }

    #[test]
    fn names_are_checked_strictly() {
        for ok in ["Teil.solidcraft", "Gehäuse v2 (final).solidcraft", "a-b_c,d.json", "Alt.solvecraft", "UPPER.SOLIDCRAFT"] {
            assert!(valid_name(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            ".versteckt.solidcraft",
            "../x.solidcraft",
            "a/b.solidcraft",
            "a\\b.solidcraft",
            "a..b.solidcraft",
            "ohne-endung",
            "datei.step",
            "datei.exe",
            " vorn.solidcraft",
            "hinten .solidcraft ",
            "nul\0.solidcraft",
            "tab\t.solidcraft",
            "stern*.solidcraft",
            ".solidcraft",
        ] {
            assert!(valid_name(bad).is_err(), "{bad:?}");
        }
        assert!(valid_name(&format!("{}.solidcraft", "a".repeat(MAX_NAME_BYTES))).is_err());
    }

    #[test]
    fn create_then_update_needs_the_current_etag() {
        let dir = TempDir::new();
        let store = Store::open(dir.path()).unwrap();
        let (first, created) = put(&store, "a.solidcraft", b"eins", None).unwrap();
        assert!(created);
        assert_eq!(first.size, 4);

        assert!(matches!(put(&store, "a.solidcraft", b"zwei", None), Err(CommitError::PreconditionRequired)));
        assert!(matches!(put(&store, "a.solidcraft", b"zwei", Some("\"falsch\"")), Err(CommitError::PreconditionFailed)));
        let (second, created) = put(&store, "a.solidcraft", b"zwei!", Some(&first.etag)).unwrap();
        assert!(!created);
        assert_ne!(first.etag, second.etag);

        // der alte ETag ist jetzt veraltet
        assert!(matches!(put(&store, "a.solidcraft", b"drei", Some(&first.etag)), Err(CommitError::PreconditionFailed)));
        // eine Liste und * gehen
        assert!(put(&store, "a.solidcraft", b"drei", Some(&format!("\"x\", {}", second.etag))).is_ok());
        let current = store.stat("a.solidcraft").unwrap().unwrap();
        assert!(put(&store, "a.solidcraft", b"vier", Some("*")).is_ok());
        assert_ne!(store.stat("a.solidcraft").unwrap().unwrap().etag, current.etag);
    }

    #[test]
    fn if_match_on_a_missing_file_fails() {
        let dir = TempDir::new();
        let store = Store::open(dir.path()).unwrap();
        assert!(matches!(put(&store, "neu.solidcraft", b"x", Some("*")), Err(CommitError::PreconditionFailed)));
        assert!(store.stat("neu.solidcraft").unwrap().is_none());
    }

    #[test]
    fn content_is_what_was_written_and_no_temp_files_stay() {
        let dir = TempDir::new();
        let store = Store::open(dir.path()).unwrap();
        put(&store, "a.solidcraft", b"inhalt", None).unwrap();
        let (mut file, entry) = store.open_read("a.solidcraft").unwrap().unwrap();
        let mut text = String::new();
        file.read_to_string(&mut text).unwrap();
        assert_eq!(text, "inhalt");
        assert_eq!(entry.etag, store.stat("a.solidcraft").unwrap().unwrap().etag);
        assert_eq!(fs::read_dir(dir.path().join(".tmp")).unwrap().count(), 0);

        // abgelehnte Schreibzugriffe räumen ihre temporäre Datei auf
        let _ = put(&store, "a.solidcraft", b"x", None);
        assert_eq!(fs::read_dir(dir.path().join(".tmp")).unwrap().count(), 0);
    }

    #[test]
    fn size_limit_and_empty_bodies_are_refused() {
        let dir = TempDir::new();
        let store = Store::open(dir.path()).unwrap();
        assert!(matches!(store.stage(&mut &[0u8; 11][..], 10), Err(StageError::TooLarge)));
        assert!(store.stage(&mut &[0u8; 10][..], 10).is_ok());
        assert!(matches!(store.stage(&mut &[][..], 10), Err(StageError::Empty)));
        assert_eq!(fs::read_dir(dir.path().join(".tmp")).unwrap().count(), 0);
    }

    #[test]
    fn list_skips_hidden_foreign_and_directories() {
        let dir = TempDir::new();
        let store = Store::open(dir.path()).unwrap();
        put(&store, "b.solidcraft", b"b", None).unwrap();
        put(&store, "a.json", b"a", None).unwrap();
        fs::write(dir.path().join("notiz.txt"), b"x").unwrap();
        fs::write(dir.path().join(".versteckt.solidcraft"), b"x").unwrap();
        fs::create_dir(dir.path().join("ordner.solidcraft")).unwrap();
        let names: Vec<_> = store.list().unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(names, ["a.json", "b.solidcraft"]);
    }

    #[test]
    fn delete_reports_whether_the_file_existed() {
        let dir = TempDir::new();
        let store = Store::open(dir.path()).unwrap();
        put(&store, "a.solidcraft", b"x", None).unwrap();
        assert!(store.delete("a.solidcraft").unwrap());
        assert!(!store.delete("a.solidcraft").unwrap());
        assert!(store.open_read("a.solidcraft").unwrap().is_none());
    }

    #[test]
    fn external_change_changes_the_etag() {
        let dir = TempDir::new();
        let store = Store::open(dir.path()).unwrap();
        let (first, _) = put(&store, "a.solidcraft", b"eins", None).unwrap();
        fs::write(dir.path().join("a.solidcraft"), b"von aussen geaendert").unwrap();
        assert_ne!(store.stat("a.solidcraft").unwrap().unwrap().etag, first.etag);
        assert!(matches!(put(&store, "a.solidcraft", b"x", Some(&first.etag)), Err(CommitError::PreconditionFailed)));
    }

    #[test]
    fn leftover_temp_files_are_removed_on_open() {
        let dir = TempDir::new();
        fs::create_dir_all(dir.path().join(".tmp")).unwrap();
        fs::write(dir.path().join(".tmp").join("alt.part"), b"x").unwrap();
        Store::open(dir.path()).unwrap();
        assert_eq!(fs::read_dir(dir.path().join(".tmp")).unwrap().count(), 0);
    }
}
