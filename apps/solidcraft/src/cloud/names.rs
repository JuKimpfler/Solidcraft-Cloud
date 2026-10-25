//! SolidCraft_Cloud, desktop app: the rules and words that need no network. They follow the file
//! server (`cloud/server/src/store.rs`) and the browser build (`apps/solidcraft-web/src/server.rs`),
//! so a name that one accepts the other accepts too.

/// Where a design on the server appears on the start page (as a "recent" path). Choosing one goes
/// through `restore_file`; the design itself is a file in the cache folder.
pub const PREFIX: &str = "/server/";
/// The file server's address below the site.
const API: &str = "api/";
/// The extensions the server keeps.
const EXTENSIONS: [&str; 3] = ["solidcraft", "solvecraft", "json"];
/// The longest name the server takes.
const MAX_NAME_BYTES: usize = 200;

/// The start page's path of the design `name`.
pub fn start_path(name: &str) -> String {
    format!("{PREFIX}{name}")
}

/// The name of a design on the server, from a path made by [`start_path`].
pub fn parse_start_path(path: &str) -> Option<&str> {
    let name = path.strip_prefix(PREFIX)?;
    valid_name(name).is_ok().then_some(name)
}

/// The server's rules for a name: no paths, no hidden files, a design's extension.
pub fn valid_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > MAX_NAME_BYTES {
        return Err(format!("Der Name ist leer oder länger als {MAX_NAME_BYTES} Zeichen."));
    }
    if name.starts_with('.') || name.starts_with(' ') || name.ends_with(' ') || name.contains("..") {
        return Err("Der Name darf nicht mit Punkt oder Leerzeichen beginnen, nicht mit Leerzeichen enden und kein „..“ enthalten.".into());
    }
    if let Some(bad) = name.chars().find(|c| !(c.is_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.' | '(' | ')' | ','))) {
        return Err(format!("Das Zeichen „{bad}“ ist im Namen nicht erlaubt (erlaubt: Buchstaben, Ziffern, Leerzeichen und - _ . ( ) ,)."));
    }
    if !has_design_extension(name) {
        return Err(format!("Die Endung muss eine von .{} sein.", EXTENSIONS.join(", .")));
    }
    Ok(())
}

fn has_design_extension(name: &str) -> bool {
    std::path::Path::new(name).extension().and_then(|e| e.to_str()).is_some_and(|e| EXTENSIONS.iter().any(|x| x.eq_ignore_ascii_case(e)))
}

/// A name typed for Save As, with `.solidcraft` added when it has no design extension.
pub fn with_extension(name: &str) -> String {
    let name = name.trim();
    if has_design_extension(name) { name.to_string() } else { format!("{name}.{}", solidcraft_engine::io::DESIGN_EXT) }
}

/// Is `name` one of `names`? Case is ignored: two designs that differ only in case would be
/// mistaken for each other (and are one file on some systems).
pub fn taken<'a>(mut names: impl Iterator<Item = &'a str>, name: &str) -> bool {
    names.any(|n| n.to_lowercase() == name.to_lowercase())
}

/// `name` as one segment of a URL path: everything but letters, digits and `-_.~` percent-encoded.
pub fn encode(name: &str) -> String {
    let mut out = String::with_capacity(name.len() * 3);
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The path (below the site) of the list of designs.
pub fn list_path() -> String {
    format!("{API}designs")
}

/// The path of the design `name`.
pub fn design_path(name: &str) -> String {
    format!("{API}designs/{}", encode(name))
}

/// The path of the lock on the design `name`.
pub fn lock_path(name: &str) -> String {
    format!("{API}designs/{}/lock", encode(name))
}

/// The path of the health check.
pub fn health_path() -> String {
    format!("{API}health")
}

/// The site's address as typed, cleaned up: `https://` or `http://`, no trailing slash, no path
/// the user may have copied along (`/api/...`). `None` for anything that is not an address.
pub fn clean_url(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let (scheme, rest) = if let Some(r) = raw.strip_prefix("https://") {
        ("https://", r)
    } else if let Some(r) = raw.strip_prefix("http://") {
        ("http://", r)
    } else if raw.contains("://") || raw.is_empty() {
        return None;
    } else {
        ("https://", raw)
    };
    let host = rest.split('/').next().unwrap_or_default();
    if host.is_empty() || host.contains(char::is_whitespace) || host.contains('@') {
        return None;
    }
    Some(format!("{scheme}{host}"))
}

/// HTTP Basic credentials as the header value (`Basic …`).
pub fn basic_auth(user: &str, password: &str) -> String {
    format!("Basic {}", base64(format!("{user}:{password}").as_bytes()))
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk.first().copied().unwrap_or(0), chunk.get(1).copied().unwrap_or(0), chunk.get(2).copied().unwrap_or(0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                let index = ((n >> (18 - 6 * i)) & 63) as usize;
                out.push(char::from(TABLE.get(index).copied().unwrap_or(b'A')));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Who has a design open: this window, you somewhere else, or someone in the team.
pub fn lock_text(owner: &str, mine: bool, here: bool) -> String {
    match (here, mine) {
        (true, _) => "hier geöffnet".into(),
        (false, true) => "von dir in einem anderen Fenster geöffnet".into(),
        (false, false) => format!("geöffnet von {owner}"),
    }
}

/// What to tell the user who opened a design somebody has: this is a copy, and saving it asks for
/// a new name (the design on the server stays as it is).
pub fn copy_notice(name: &str, owner: &str, mine: bool) -> String {
    let who =
        if mine { format!("„{name}“ hast du in einem anderen Fenster geöffnet") } else { format!("„{name}“ ist von {owner} geöffnet") };
    format!("{who}. Hier ist nur eine Kopie: Mit „Speichern unter“ sicherst du sie unter einem neuen Namen.")
}

/// The lock ran out and somebody else took the design meanwhile.
pub fn lock_lost(name: &str, owner: &str) -> String {
    format!("Die Sperre für „{name}“ ist abgelaufen, {owner} hat das Design jetzt. Mit „Speichern unter“ sicherst du deine Änderungen.")
}

/// The line under a design's name on the start page.
pub fn detail_text(lock: Option<&str>, size: u64, when: &str) -> String {
    let what = lock.map_or_else(|| size_text(size), str::to_string);
    if when.is_empty() { what } else { format!("{what} · {when}") }
}

/// An ETag as the server made it: a proxy that compresses the response (Apache's mod_deflate)
/// marks it weak or adds `-gzip` inside the quotes, which the server would not recognise.
pub fn normalize_etag(raw: &str) -> String {
    let tag = raw.trim();
    let tag = tag.strip_prefix("W/").unwrap_or(tag);
    for suffix in ["-gzip\"", "-br\"", "-deflate\"", "-zstd\""] {
        if let Some(stem) = tag.strip_suffix(suffix) {
            return format!("{stem}\"");
        }
    }
    tag.to_string()
}

/// What to tell the user when the server refused to save `name` (`status`; the server's own
/// `error` and the `owner` of a lock, from its answer).
pub fn save_failure(status: u16, name: &str, error: Option<&str>, owner: Option<&str>) -> String {
    let keep = "Mit „Speichern unter“ unter einem neuen Namen sichern.";
    match status {
        412 => format!("„{name}“ wurde auf dem Server inzwischen geändert. Nicht hochgeladen, damit nichts verloren geht. {keep}"),
        428 => format!("„{name}“ gibt es auf dem Server schon, die geladene Version ist unbekannt. Nicht überschrieben. {keep}"),
        423 => format!("„{name}“ ist gesperrt von {}. Nicht hochgeladen. {keep}", owner.unwrap_or("jemand anderem")),
        413 => format!("„{name}“ ist zu groß für den Server. Nicht hochgeladen."),
        401 | 403 => "Der Server lehnt die Anmeldung ab. Nicht hochgeladen: unter „Öffnen“ neu anmelden und erneut speichern.".into(),
        _ => format!("„{name}“ nicht hochgeladen: Der Server antwortet {status}{}", error.map(|e| format!(" ({e})")).unwrap_or_default()),
    }
}

/// How long ago a design was written (`modified_ms` and `now_ms` in ms since 1970); nothing when
/// the server gave no time.
pub fn ago_text(now_ms: f64, modified_ms: f64) -> String {
    if modified_ms <= 0.0 || now_ms <= 0.0 {
        return String::new();
    }
    let s = ((now_ms - modified_ms) / 1000.0).max(0.0) as u64;
    match s {
        0..=59 => "gerade eben".into(),
        60..=3599 => format!("vor {} Min", s / 60),
        3600..=86_399 => format!("vor {} Std", s / 3600),
        86_400..=172_799 => "vor 1 Tag".into(),
        _ => format!("vor {} Tagen", s / 86_400),
    }
}

/// A file size for the list.
pub fn size_text(bytes: u64) -> String {
    let b = bytes as f64;
    if b < 1024.0 * 1024.0 { format!("{:.1} KB", b / 1024.0) } else { format!("{:.1} MB", b / 1024.0 / 1024.0) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_start_path_round_trips_its_name() {
        assert_eq!(parse_start_path(&start_path("Mein Gehäuse.solidcraft")), Some("Mein Gehäuse.solidcraft"));
        assert_eq!(parse_start_path("/opfs/Plate.solidcraft"), None);
        assert_eq!(parse_start_path("/server/../x.solidcraft"), None);
    }

    #[test]
    fn names_follow_the_servers_rules() {
        for ok in ["Plate.solidcraft", "Mein Gehäuse (2).solidcraft", "a,b-c_d.json", "X.SOLIDCRAFT"] {
            assert!(valid_name(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            ".hidden.solidcraft",
            " lead.solidcraft",
            "trail.solidcraft ",
            "a/b.solidcraft",
            "a\\b.solidcraft",
            "a..b.solidcraft",
            "Plate.txt",
            "Plate",
            "a:b.solidcraft",
        ] {
            assert!(valid_name(bad).is_err(), "{bad}");
        }
        assert!(valid_name(&format!("{}.solidcraft", "a".repeat(200))).is_err());
    }

    #[test]
    fn save_as_adds_the_design_extension_when_it_is_missing() {
        assert_eq!(with_extension("  Plate "), "Plate.solidcraft");
        assert_eq!(with_extension("Plate.json"), "Plate.json");
        assert_eq!(with_extension("Plate.SolidCraft"), "Plate.SolidCraft");
    }

    #[test]
    fn a_name_is_taken_whatever_its_case() {
        assert!(taken(["Plate.solidcraft"].into_iter(), "plate.SOLIDCRAFT"));
        assert!(!taken(["Plate.solidcraft"].into_iter(), "Other.solidcraft"));
    }

    #[test]
    fn names_are_percent_encoded_in_the_path() {
        assert_eq!(design_path("Mein Gehäuse.solidcraft"), "api/designs/Mein%20Geh%C3%A4use.solidcraft");
        assert_eq!(lock_path("a b.json"), "api/designs/a%20b.json/lock");
        assert_eq!(list_path(), "api/designs");
        assert_eq!(health_path(), "api/health");
    }

    #[test]
    fn an_address_is_cleaned_up() {
        assert_eq!(clean_url("cad.example.eu").as_deref(), Some("https://cad.example.eu"));
        assert_eq!(clean_url(" https://cad.example.eu/ ").as_deref(), Some("https://cad.example.eu"));
        assert_eq!(clean_url("https://cad.example.eu/api/health").as_deref(), Some("https://cad.example.eu"));
        assert_eq!(clean_url("http://localhost:8088").as_deref(), Some("http://localhost:8088"));
        for bad in ["", "   ", "ftp://x.eu", "https://", "https://user:pw@x.eu", "a b.eu"] {
            assert_eq!(clean_url(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn basic_credentials_are_base64() {
        // RFC 7617's example, and the padding cases.
        assert_eq!(basic_auth("Aladdin", "open sesame"), "Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ==");
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
    }

    #[test]
    fn a_compressing_proxys_etag_is_taken_back_to_the_servers() {
        assert_eq!(normalize_etag("\"abc-gzip\""), "\"abc\"");
        assert_eq!(normalize_etag("W/\"abc-br\""), "\"abc\"");
        assert_eq!(normalize_etag("\"abc\""), "\"abc\"");
    }

    #[test]
    fn a_refused_save_says_why_and_what_to_do() {
        assert!(save_failure(412, "A.solidcraft", None, None).contains("inzwischen geändert"));
        assert!(save_failure(423, "A.solidcraft", None, Some("bob")).contains("bob"));
        assert!(save_failure(401, "A.solidcraft", None, None).contains("Anmeldung"));
        assert!(save_failure(500, "A.solidcraft", Some("kaputt"), None).contains("(kaputt)"));
    }

    #[test]
    fn a_lock_says_whether_it_is_yours_and_whether_it_is_this_window() {
        assert_eq!(lock_text("alice", true, true), "hier geöffnet");
        assert_eq!(lock_text("alice", true, false), "von dir in einem anderen Fenster geöffnet");
        assert_eq!(lock_text("alice", false, false), "geöffnet von alice");
    }

    #[test]
    fn age_is_told_in_the_largest_fitting_unit() {
        let now = 10_000_000_000.0;
        assert_eq!(ago_text(now, now - 5_000.0), "gerade eben");
        assert_eq!(ago_text(now, now - 5.0 * 60_000.0), "vor 5 Min");
        assert_eq!(ago_text(now, now - 3.0 * 3_600_000.0), "vor 3 Std");
        assert_eq!(ago_text(now, now - 30.0 * 3_600_000.0), "vor 1 Tag");
        assert_eq!(ago_text(now, now - 3.0 * 86_400_000.0), "vor 3 Tagen");
        assert_eq!(ago_text(now, 0.0), "");
        assert_eq!(ago_text(now, now + 1_000.0), "gerade eben");
    }

    #[test]
    fn the_start_pages_line_says_who_has_it_or_how_big_it_is() {
        assert_eq!(detail_text(Some("geöffnet von bob"), 10, "12:00"), "geöffnet von bob · 12:00");
        assert_eq!(detail_text(None, 2048, ""), "2.0 KB");
    }
}
