//! Die HTTP-Schnittstelle, unabhängig vom konkreten HTTP-Server (siehe `main.rs`), damit sie sich
//! ohne Netzwerk testen lässt.
//!
//! | Aufruf | Zweck |
//! |---|---|
//! | `GET /api/health` | Lebenszeichen (ohne Anmeldung) |
//! | `GET /api/designs` | Liste mit ETag und Sperre |
//! | `GET /api/designs/{name}` | Datei laden |
//! | `PUT /api/designs/{name}` | Speichern; `If-Match` Pflicht, wenn es die Datei gibt |
//! | `DELETE /api/designs/{name}` | Löschen |
//! | `POST /api/designs/{name}/lock` | Sperre nehmen, liefert ein Token |
//! | `PUT /api/designs/{name}/lock` | Heartbeat (Header `X-Lock-Token`) |
//! | `DELETE /api/designs/{name}/lock` | Freigeben (`X-Lock-Token`, oder `?force=1`) |
//!
//! Wer anfragt, steht im Header `X-Remote-User`. Den setzt der Reverse Proxy (Apache) nach der
//! Anmeldung. Der Server vertraut ihm blind und lauscht deshalb standardmäßig nur lokal.

use std::fs::File;
use std::io::Read;
use std::sync::Mutex;
use std::time::Instant;

use serde_json::{json, Value};

use crate::config::Config;
use crate::locks::{LockInfo, Locks, Release};
use crate::store::{valid_name, CommitError, Entry, StageError, Store};

pub struct App {
    pub cfg: Config,
    pub store: Store,
    pub locks: Locks,
    /// Prüfen und Veröffentlichen eines Schreibzugriffs geschieht unter diesem Schloss.
    guard: Mutex<()>,
}

impl App {
    pub fn new(cfg: Config, store: Store) -> App {
        let locks = Locks::new(cfg.lock_ttl);
        App { cfg, store, locks, guard: Mutex::new(()) }
    }
}

pub struct Request<'a> {
    pub method: &'a str,
    /// Pfad samt Query, wie er im HTTP-Request steht.
    pub url: &'a str,
    pub headers: &'a [(String, String)],
    pub content_length: Option<u64>,
    pub body: &'a mut dyn Read,
}

pub enum Body {
    Empty,
    Json(Value),
    File(File, u64),
}

pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Body,
}

fn json(status: u16, value: Value) -> Reply {
    Reply { status, headers: Vec::new(), body: Body::Json(value) }
}

fn error(status: u16, message: impl Into<String>) -> Reply {
    json(status, json!({ "error": message.into() }))
}

fn empty(status: u16) -> Reply {
    Reply { status, headers: Vec::new(), body: Body::Empty }
}

fn method_not_allowed(allow: &str) -> Reply {
    let mut reply = error(405, "Methode nicht erlaubt");
    reply.headers.push(("Allow".into(), allow.into()));
    reply
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.trim())
}

fn valid_user(user: &str) -> bool {
    !user.is_empty() && user.chars().count() <= 64 && !user.chars().any(char::is_control)
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// `%XX` auflösen (kein `+`: das ist in Pfaden ein Pluszeichen).
fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        if b == b'%' {
            let hi = hex_digit(*bytes.get(i + 1)?)?;
            let lo = hex_digit(*bytes.get(i + 2)?)?;
            out.push(hi << 4 | lo);
            i += 3;
        } else {
            out.push(b);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn decode_name(raw: &str) -> Result<String, Reply> {
    let name = percent_decode(raw).ok_or_else(|| error(400, "Name ist nicht gültig kodiert"))?;
    valid_name(&name).map_err(|why| error(400, why))?;
    Ok(name)
}

fn wants_force(query: &str) -> bool {
    query.split('&').any(|pair| matches!(pair, "force=1" | "force=true"))
}

fn lock_json(info: &LockInfo, user: &str) -> Value {
    json!({ "owner": info.owner, "expires_in_s": info.expires_in.as_secs(), "mine": info.owner == user })
}

fn busy(status: u16, info: &LockInfo, user: &str) -> Reply {
    let message = if info.owner == user { "Von dir in einer anderen Sitzung gesperrt" } else { "Gesperrt" };
    let mut value = lock_json(info, user);
    if let Some(object) = value.as_object_mut() {
        object.insert("error".into(), message.into());
    }
    json(status, value)
}

fn entry_json(entry: &Entry, lock: Option<&LockInfo>, user: &str) -> Value {
    json!({
        "name": entry.name,
        "size": entry.size,
        "modified_ms": entry.modified_ms,
        "etag": entry.etag,
        "lock": lock.map(|l| lock_json(l, user)),
    })
}

fn io_error(what: &str, e: &std::io::Error) -> Reply {
    // Details gehören ins Protokoll des Servers, nicht zum Client (Pfade, Systemmeldungen).
    eprintln!("{what}: {e}");
    error(500, "Interner Fehler beim Zugriff auf den Speicher")
}

pub fn handle(app: &App, req: Request<'_>, now: Instant) -> Reply {
    let url = req.url;
    let (path, query) = url.split_once('?').unwrap_or((url, ""));
    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();

    if segments.as_slice() == ["api", "health"] {
        return if req.method == "GET" { json(200, json!({ "ok": true })) } else { method_not_allowed("GET") };
    }

    let user = match header(req.headers, "x-remote-user").filter(|u| valid_user(u)) {
        Some(u) => u.to_string(),
        None => return error(401, "Nicht angemeldet (Header X-Remote-User fehlt oder ist ungültig)"),
    };
    app.locks.purge(now);

    match segments.as_slice() {
        ["api", "designs"] => match req.method {
            "GET" => list(app, &user, now),
            _ => method_not_allowed("GET"),
        },
        ["api", "designs", raw] => {
            let name = match decode_name(raw) {
                Ok(n) => n,
                Err(reply) => return reply,
            };
            match req.method {
                "GET" => get(app, &name),
                "PUT" => put(app, &user, &name, req, now),
                "DELETE" => delete(app, &name, &req, now),
                _ => method_not_allowed("GET, PUT, DELETE"),
            }
        }
        ["api", "designs", raw, "lock"] => {
            let name = match decode_name(raw) {
                Ok(n) => n,
                Err(reply) => return reply,
            };
            match req.method {
                "POST" => lock_acquire(app, &user, &name, now),
                "PUT" => lock_renew(app, &name, &req, now),
                "DELETE" => lock_release(app, &name, &req, query, now),
                _ => method_not_allowed("POST, PUT, DELETE"),
            }
        }
        _ => error(404, "Nicht gefunden"),
    }
}

fn list(app: &App, user: &str, now: Instant) -> Reply {
    let entries = match app.store.list() {
        Ok(e) => e,
        Err(e) => return io_error("Liste lesen", &e),
    };
    let locks = app.locks.all(now);
    let items: Vec<Value> = entries.iter().map(|e| entry_json(e, locks.get(&e.name), user)).collect();
    json(200, json!({ "designs": items, "max_bytes": app.cfg.max_bytes }))
}

fn get(app: &App, name: &str) -> Reply {
    match app.store.open_read(name) {
        Ok(Some((file, entry))) => Reply {
            status: 200,
            headers: vec![
                ("Content-Type".into(), "application/octet-stream".into()),
                ("ETag".into(), entry.etag),
                ("Cache-Control".into(), "no-store".into()),
            ],
            body: Body::File(file, entry.size),
        },
        Ok(None) => error(404, "Design nicht gefunden"),
        Err(e) => io_error("Design lesen", &e),
    }
}

fn put(app: &App, user: &str, name: &str, req: Request<'_>, now: Instant) -> Reply {
    let max = app.cfg.max_bytes;
    if req.content_length.is_some_and(|len| len > max) {
        return error(413, format!("Datei größer als {max} Bytes"));
    }
    let if_match = header(req.headers, "if-match").map(str::to_string);
    let token = header(req.headers, "x-lock-token").map(str::to_string);

    // Erst den ganzen Inhalt in eine temporäre Datei lesen (das kann dauern), dann kurz unter dem
    // Schloss prüfen und veröffentlichen.
    let staged = match app.store.stage(req.body, max) {
        Ok(s) => s,
        Err(StageError::TooLarge) => return error(413, format!("Datei größer als {max} Bytes")),
        Err(StageError::Empty) => return error(400, "Leerer Inhalt"),
        Err(StageError::Io(e)) => return io_error("Inhalt empfangen", &e),
    };

    let _guard = app.guard.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Err(info) = app.locks.check_write(name, token.as_deref(), now) {
        return busy(423, &info, user);
    }
    match app.store.commit(name, staged, if_match.as_deref()) {
        Ok((entry, created)) => {
            let lock = app.locks.status(name, now);
            let mut reply = json(if created { 201 } else { 200 }, entry_json(&entry, lock.as_ref(), user));
            reply.headers.push(("ETag".into(), entry.etag));
            reply
        }
        Err(CommitError::PreconditionRequired) => {
            error(428, "If-Match fehlt: Die Datei gibt es schon, zum Überschreiben den ETag der geladenen Version angeben")
        }
        Err(CommitError::PreconditionFailed) => error(412, "Die Datei wurde inzwischen geändert (oder gibt es nicht mehr)"),
        Err(CommitError::Io(e)) => io_error("Design speichern", &e),
    }
}

fn delete(app: &App, name: &str, req: &Request<'_>, now: Instant) -> Reply {
    let token = header(req.headers, "x-lock-token");
    let _guard = app.guard.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Err(info) = app.locks.check_write(name, token, now) {
        // Für Löschen ist "mine" ohne Bedeutung, die Antwort nennt nur den Besitzer.
        return busy(423, &info, "");
    }
    if let Some(expected) = header(req.headers, "if-match") {
        match app.store.stat(name) {
            Ok(Some(current)) if !crate::store::etag_matches(expected, &current.etag) => {
                return error(412, "Die Datei wurde inzwischen geändert");
            }
            Ok(_) => {}
            Err(e) => return io_error("Design prüfen", &e),
        }
    }
    match app.store.delete(name) {
        Ok(true) => {
            app.locks.force_release(name);
            empty(204)
        }
        Ok(false) => error(404, "Design nicht gefunden"),
        Err(e) => io_error("Design löschen", &e),
    }
}

fn lock_acquire(app: &App, user: &str, name: &str, now: Instant) -> Reply {
    let token = match crate::random_hex(16) {
        Ok(t) => t,
        Err(e) => return io_error("Token erzeugen", &e),
    };
    match app.locks.acquire(name, user, &token, now) {
        Ok(ttl) => json(200, json!({ "token": token, "owner": user, "ttl_s": ttl.as_secs(), "expires_in_s": ttl.as_secs() })),
        Err(info) => busy(409, &info, user),
    }
}

fn lock_renew(app: &App, name: &str, req: &Request<'_>, now: Instant) -> Reply {
    let Some(token) = header(req.headers, "x-lock-token") else { return error(400, "Header X-Lock-Token fehlt") };
    match app.locks.renew(name, token, now) {
        Some(ttl) => json(200, json!({ "ttl_s": ttl.as_secs(), "expires_in_s": ttl.as_secs() })),
        None => json(409, json!({ "error": "Sperre verloren (abgelaufen oder übernommen)", "lost": true })),
    }
}

fn lock_release(app: &App, name: &str, req: &Request<'_>, query: &str, now: Instant) -> Reply {
    if wants_force(query) {
        app.locks.force_release(name);
        return empty(204);
    }
    let Some(token) = header(req.headers, "x-lock-token") else {
        return error(400, "Header X-Lock-Token fehlt (oder ?force=1 zum Aufheben einer fremden Sperre)");
    };
    match app.locks.release(name, token, now) {
        Release::Released | Release::NotHeld => empty(204),
        Release::Mismatch => error(409, "Die Sperre gehört einer anderen Sitzung"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use std::time::Duration;

    struct Fixture {
        _dir: TempDir,
        app: App,
        now: Instant,
    }

    impl Fixture {
        fn new() -> Fixture {
            let dir = TempDir::new();
            let cfg = Config {
                data_dir: dir.path().to_path_buf(),
                bind: "127.0.0.1:0".parse().unwrap(),
                max_bytes: 1000,
                lock_ttl: Duration::from_secs(60),
            };
            let store = Store::open(dir.path()).unwrap();
            Fixture { _dir: dir, app: App::new(cfg, store), now: Instant::now() }
        }

        fn call(&self, user: Option<&str>, method: &str, url: &str, headers: &[(&str, &str)], body: &[u8]) -> (u16, Value, Vec<(String, String)>) {
            self.call_at(self.now, user, method, url, headers, body)
        }

        fn call_at(
            &self,
            now: Instant,
            user: Option<&str>,
            method: &str,
            url: &str,
            headers: &[(&str, &str)],
            body: &[u8],
        ) -> (u16, Value, Vec<(String, String)>) {
            let mut all: Vec<(String, String)> = headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
            if let Some(u) = user {
                all.push(("X-Remote-User".into(), u.into()));
            }
            let mut reader = body;
            let req = Request { method, url, headers: &all, content_length: Some(body.len() as u64), body: &mut reader };
            let reply = handle(&self.app, req, now);
            let value = match reply.body {
                Body::Json(v) => v,
                Body::Empty => Value::Null,
                Body::File(mut f, len) => {
                    let mut data = Vec::new();
                    f.read_to_end(&mut data).unwrap();
                    assert_eq!(data.len() as u64, len);
                    Value::String(String::from_utf8(data).unwrap())
                }
            };
            (reply.status, value, reply.headers)
        }
    }

    fn etag(headers: &[(String, String)]) -> String {
        headers.iter().find(|(k, _)| k == "ETag").unwrap().1.clone()
    }

    #[test]
    fn health_needs_no_login_everything_else_does() {
        let f = Fixture::new();
        assert_eq!(f.call(None, "GET", "/api/health", &[], b"").0, 200);
        assert_eq!(f.call(None, "GET", "/api/designs", &[], b"").0, 401);
        assert_eq!(f.call(Some(""), "GET", "/api/designs", &[], b"").0, 401);
        assert_eq!(f.call(Some("a\nb"), "GET", "/api/designs", &[], b"").0, 401);
        assert_eq!(f.call(Some("alice"), "GET", "/api/designs", &[], b"").0, 200);
    }

    #[test]
    fn save_load_update_and_conflict() {
        let f = Fixture::new();
        let (status, _, headers) = f.call(Some("alice"), "PUT", "/api/designs/Teil.solidcraft", &[], b"eins");
        assert_eq!(status, 201);
        let first = etag(&headers);

        let (status, body, headers) = f.call(Some("bob"), "GET", "/api/designs/Teil.solidcraft", &[], b"");
        assert_eq!((status, body), (200, Value::String("eins".into())));
        assert_eq!(etag(&headers), first);

        assert_eq!(f.call(Some("alice"), "PUT", "/api/designs/Teil.solidcraft", &[], b"zwei").0, 428);
        let (status, _, headers) = f.call(Some("alice"), "PUT", "/api/designs/Teil.solidcraft", &[("If-Match", &first)], b"zwei");
        assert_eq!(status, 200);
        let second = etag(&headers);
        assert_ne!(first, second);

        // bob hat noch die erste Version geladen und will speichern
        assert_eq!(f.call(Some("bob"), "PUT", "/api/designs/Teil.solidcraft", &[("If-Match", &first)], b"bobs").0, 412);
        let (_, body, _) = f.call(Some("bob"), "GET", "/api/designs/Teil.solidcraft", &[], b"");
        assert_eq!(body, Value::String("zwei".into()));
    }

    #[test]
    fn list_shows_etag_and_lock_with_mine() {
        let f = Fixture::new();
        f.call(Some("alice"), "PUT", "/api/designs/A.solidcraft", &[], b"a");
        f.call(Some("alice"), "PUT", "/api/designs/B.solidcraft", &[], b"b");
        assert_eq!(f.call(Some("alice"), "POST", "/api/designs/A.solidcraft/lock", &[], b"").0, 200);

        let (_, body, _) = f.call(Some("bob"), "GET", "/api/designs", &[], b"");
        let designs = body["designs"].as_array().unwrap();
        assert_eq!(designs.len(), 2);
        assert_eq!(designs[0]["name"], "A.solidcraft");
        assert_eq!(designs[0]["lock"]["owner"], "alice");
        assert_eq!(designs[0]["lock"]["mine"], false);
        assert!(designs[1]["lock"].is_null());
        assert!(designs[0]["etag"].as_str().unwrap().starts_with('"'));
        assert_eq!(body["max_bytes"], 1000);

        let (_, body, _) = f.call(Some("alice"), "GET", "/api/designs", &[], b"");
        assert_eq!(body["designs"][0]["lock"]["mine"], true);
    }

    #[test]
    fn lock_blocks_other_writers_until_released() {
        let f = Fixture::new();
        let (_, _, headers) = f.call(Some("alice"), "PUT", "/api/designs/A.solidcraft", &[], b"eins");
        let etag1 = etag(&headers);

        let (status, lock, _) = f.call(Some("alice"), "POST", "/api/designs/A.solidcraft/lock", &[], b"");
        assert_eq!(status, 200);
        let token = lock["token"].as_str().unwrap().to_string();

        // bob bekommt keine Sperre und darf nicht schreiben oder löschen
        let (status, busy, _) = f.call(Some("bob"), "POST", "/api/designs/A.solidcraft/lock", &[], b"");
        assert_eq!(status, 409);
        assert_eq!(busy["owner"], "alice");
        assert_eq!(f.call(Some("bob"), "PUT", "/api/designs/A.solidcraft", &[("If-Match", &etag1)], b"bob").0, 423);
        assert_eq!(f.call(Some("bob"), "DELETE", "/api/designs/A.solidcraft", &[], b"").0, 423);
        // alice in einer anderen Sitzung (ohne Token) auch nicht
        assert_eq!(f.call(Some("alice"), "PUT", "/api/designs/A.solidcraft", &[("If-Match", &etag1)], b"x").0, 423);

        // mit dem Token darf alice schreiben
        let (status, _, headers) =
            f.call(Some("alice"), "PUT", "/api/designs/A.solidcraft", &[("If-Match", &etag1), ("X-Lock-Token", &token)], b"zwei");
        assert_eq!(status, 200);
        let etag2 = etag(&headers);

        // freigeben: jetzt darf bob (mit aktuellem ETag)
        assert_eq!(f.call(Some("alice"), "DELETE", "/api/designs/A.solidcraft/lock", &[("X-Lock-Token", &token)], b"").0, 204);
        assert_eq!(f.call(Some("bob"), "PUT", "/api/designs/A.solidcraft", &[("If-Match", &etag2)], b"bobs").0, 200);
    }

    #[test]
    fn lock_expires_without_heartbeat_and_heartbeat_keeps_it() {
        let f = Fixture::new();
        f.call(Some("alice"), "PUT", "/api/designs/A.solidcraft", &[], b"a");
        let (_, lock, _) = f.call(Some("alice"), "POST", "/api/designs/A.solidcraft/lock", &[], b"");
        let token = lock["token"].as_str().unwrap().to_string();
        let t = |secs| f.now + Duration::from_secs(secs);

        // Heartbeat nach 40 s verlängert bis 100 s
        assert_eq!(f.call_at(t(40), Some("alice"), "PUT", "/api/designs/A.solidcraft/lock", &[("X-Lock-Token", &token)], b"").0, 200);
        assert_eq!(f.call_at(t(90), Some("bob"), "POST", "/api/designs/A.solidcraft/lock", &[], b"").0, 409);
        // danach läuft sie ab, und bob übernimmt
        assert_eq!(f.call_at(t(101), Some("bob"), "POST", "/api/designs/A.solidcraft/lock", &[], b"").0, 200);
        // alices altes Token ist wertlos
        let (status, body, _) = f.call_at(t(102), Some("alice"), "PUT", "/api/designs/A.solidcraft/lock", &[("X-Lock-Token", &token)], b"");
        assert_eq!(status, 409);
        assert_eq!(body["lost"], true);
    }

    #[test]
    fn heartbeat_and_release_validate_their_input() {
        let f = Fixture::new();
        assert_eq!(f.call(Some("alice"), "PUT", "/api/designs/A.solidcraft/lock", &[], b"").0, 400);
        assert_eq!(f.call(Some("alice"), "DELETE", "/api/designs/A.solidcraft/lock", &[], b"").0, 400);
        let (_, lock, _) = f.call(Some("alice"), "POST", "/api/designs/A.solidcraft/lock", &[], b"");
        let token = lock["token"].as_str().unwrap().to_string();
        assert_eq!(f.call(Some("bob"), "DELETE", "/api/designs/A.solidcraft/lock", &[("X-Lock-Token", "falsch")], b"").0, 409);
        // ?force=1 hebt eine fremde Sperre auf (Notbremse)
        assert_eq!(f.call(Some("bob"), "DELETE", "/api/designs/A.solidcraft/lock?force=1", &[], b"").0, 204);
        let (status, bobs, _) = f.call(Some("bob"), "POST", "/api/designs/A.solidcraft/lock", &[], b"");
        assert_eq!(status, 200);
        let bobs_token = bobs["token"].as_str().unwrap().to_string();
        // alices altes Token gehört zu einer aufgehobenen Sperre und stört bobs neue nicht
        assert_eq!(f.call(Some("alice"), "DELETE", "/api/designs/A.solidcraft/lock", &[("X-Lock-Token", &token)], b"").0, 409);
        assert_eq!(f.call(Some("bob"), "POST", "/api/designs/A.solidcraft/lock", &[], b"").0, 409);
        // freigeben ist wiederholbar
        assert_eq!(f.call(Some("bob"), "DELETE", "/api/designs/A.solidcraft/lock", &[("X-Lock-Token", &bobs_token)], b"").0, 204);
        assert_eq!(f.call(Some("bob"), "DELETE", "/api/designs/A.solidcraft/lock", &[("X-Lock-Token", &bobs_token)], b"").0, 204);
    }

    #[test]
    fn delete_removes_file_and_lock() {
        let f = Fixture::new();
        let (_, _, headers) = f.call(Some("alice"), "PUT", "/api/designs/A.solidcraft", &[], b"a");
        let tag = etag(&headers);
        assert_eq!(f.call(Some("alice"), "DELETE", "/api/designs/A.solidcraft", &[("If-Match", "\"alt\"")], b"").0, 412);
        assert_eq!(f.call(Some("alice"), "DELETE", "/api/designs/A.solidcraft", &[("If-Match", &tag)], b"").0, 204);
        assert_eq!(f.call(Some("alice"), "DELETE", "/api/designs/A.solidcraft", &[], b"").0, 404);
        assert_eq!(f.call(Some("alice"), "GET", "/api/designs/A.solidcraft", &[], b"").0, 404);
    }

    #[test]
    fn names_are_decoded_and_dangerous_ones_refused() {
        let f = Fixture::new();
        assert_eq!(f.call(Some("alice"), "PUT", "/api/designs/Mein%20Geh%C3%A4use.solidcraft", &[], b"x").0, 201);
        let (_, body, _) = f.call(Some("alice"), "GET", "/api/designs", &[], b"");
        assert_eq!(body["designs"][0]["name"], "Mein Gehäuse.solidcraft");

        for url in [
            "/api/designs/..%2F..%2Fetc%2Fpasswd",
            "/api/designs/%2e%2e",
            "/api/designs/a%2Fb.solidcraft",
            "/api/designs/.versteckt.solidcraft",
            "/api/designs/a.txt",
            "/api/designs/%ZZ.solidcraft",
            "/api/designs/%FF.solidcraft",
            "/api/designs/a%00.solidcraft",
        ] {
            assert_eq!(f.call(Some("alice"), "PUT", url, &[], b"x").0, 400, "{url}");
        }
        // nichts außerhalb des Ordners oder Versteckes ist entstanden
        assert_eq!(f.app.store.list().unwrap().len(), 1);
    }

    #[test]
    fn size_limit_and_empty_body() {
        let f = Fixture::new();
        assert_eq!(f.call(Some("alice"), "PUT", "/api/designs/A.solidcraft", &[], &[b'x'; 1001]).0, 413);
        assert_eq!(f.call(Some("alice"), "PUT", "/api/designs/A.solidcraft", &[], &[b'x'; 1000]).0, 201);
        assert_eq!(f.call(Some("alice"), "PUT", "/api/designs/B.solidcraft", &[], b"").0, 400);

        // Ohne Content-Length (chunked) greift das Limit beim Lesen
        let mut body: &[u8] = &[b'x'; 1500];
        let headers = vec![("X-Remote-User".to_string(), "alice".to_string())];
        let req = Request { method: "PUT", url: "/api/designs/C.solidcraft", headers: &headers, content_length: None, body: &mut body };
        assert_eq!(handle(&f.app, req, f.now).status, 413);
        assert!(f.app.store.stat("C.solidcraft").unwrap().is_none());
    }

    #[test]
    fn unknown_paths_and_methods() {
        let f = Fixture::new();
        assert_eq!(f.call(Some("alice"), "GET", "/", &[], b"").0, 404);
        assert_eq!(f.call(Some("alice"), "GET", "/api/anderes", &[], b"").0, 404);
        let (status, _, headers) = f.call(Some("alice"), "POST", "/api/designs", &[], b"");
        assert_eq!(status, 405);
        assert!(headers.iter().any(|(k, v)| k == "Allow" && v == "GET"));
        assert_eq!(f.call(Some("alice"), "PATCH", "/api/designs/A.solidcraft", &[], b"").0, 405);
    }
}
