//! SolidCraft_Cloud: designs on the team's file server (`cloud/server`, see `cloud/ARCHITECTURE.md`).
//! When the page's own site answers `GET /api/health`, Open lists the designs on the server and
//! Save As puts a design there. To the engine such a design is the file `/server/<name>`: it is
//! fetched into the engine's files when it is opened, and [`flush`](crate::files::flush) sends
//! what a save wrote back to the server with the ETag it was loaded with (`If-Match`), so a design
//! someone else changed meanwhile is never overwritten. A design from the server is kept in memory
//! only while a tab shows it: opening it again fetches the server's current version.
//!
//! Opening a design also takes its **lock** on the server, renewed every 20 s while the tab shows
//! it and let go when the tab closes or the page goes away. A design somebody else has open is
//! opened as a copy with no file of its own, so a save asks for a new name instead of writing
//! over their work. With a file server the start page lists its designs in place of the built-in
//! samples, with a picture read from each design that is small enough to fetch for one.
//!
//! The team works in German, so the messages are German. The path and name logic is plain Rust
//! and tested natively; the browser calls are wasm-only.
#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

/// Where a design on the server appears to the engine.
pub const PREFIX: &str = "/server/";
/// The file server, on the page's own site (the reverse proxy forwards it).
pub const API: &str = "/api/";
/// The extensions the server keeps (`cloud/server/src/store.rs`).
const EXTENSIONS: [&str; 3] = ["solidcraft", "solvecraft", "json"];
/// The longest name the server takes.
const MAX_NAME_BYTES: usize = 200;
/// How often the locks this page holds are renewed. The server lets one run out after 60 s
/// (`SOLIDCRAFT_CLOUD_LOCK_TTL_SECS`), so a missed beat still leaves two more.
const HEARTBEAT_MS: i32 = 20_000;
/// How long the start page's list may be before it is fetched again.
const LIST_FRESH_MS: f64 = 10_000.0;
/// The largest design the start page fetches for its picture: the picture sits inside the design,
/// so a bigger one would mean downloading it whole just to show a card.
const PREVIEW_MAX_BYTES: u64 = 4 << 20;

/// The engine path of the design `name` on the server.
pub fn path(name: &str) -> String {
    format!("{PREFIX}{name}")
}

/// The name of a design on the server, from its engine path made by [`path`].
pub fn parse(path: &str) -> Option<&str> {
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

/// The URL of the design `name`.
pub fn design_url(name: &str) -> String {
    format!("{API}designs/{}", encode(name))
}

/// The URL of the lock on the design `name`.
pub fn lock_url(name: &str) -> String {
    format!("{API}designs/{}/lock", encode(name))
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
        412 => format!("„{name}“ wurde auf dem Server inzwischen geändert. Nicht gespeichert, damit nichts verloren geht. {keep}"),
        428 => format!("„{name}“ gibt es auf dem Server schon, die geladene Version ist unbekannt. Nicht überschrieben. {keep}"),
        423 => format!("„{name}“ ist gesperrt von {}. Nicht gespeichert. {keep}", owner.unwrap_or("jemand anderem")),
        413 => format!("„{name}“ ist zu groß für den Server. Nicht gespeichert."),
        401 => "Nicht am Server angemeldet. Nicht gespeichert: die Seite in einem neuen Tab öffnen, anmelden und erneut speichern.".into(),
        _ => format!("„{name}“ nicht gespeichert: Der Server antwortet {status}{}", error.map(|e| format!(" ({e})")).unwrap_or_default()),
    }
}

/// A file size for the list.
pub fn size_text(bytes: u64) -> String {
    let b = bytes as f64;
    if b < 1024.0 * 1024.0 { format!("{:.1} KB", b / 1024.0) } else { format!("{:.1} MB", b / 1024.0 / 1024.0) }
}

#[cfg(target_arch = "wasm32")]
pub use web::*;

#[cfg(target_arch = "wasm32")]
mod web {
    use std::cell::{Cell, RefCell};
    use std::collections::{HashMap, VecDeque};
    use std::rc::Rc;

    use js_sys::{Function, Promise, Uint8Array};
    use serde_json::Value;
    use solidcraft_engine::io::vfs;
    use solidcraft_ui_egui::theme::Tokens;
    use solidcraft_ui_egui::{ServerDesign, ServerDesigns, SolveApp, documents, ui_kit};
    use wasm_bindgen::{JsCast as _, JsValue};
    use wasm_bindgen_futures::{JsFuture, spawn_local};

    use super::{
        API, HEARTBEAT_MS, LIST_FRESH_MS, PREFIX, PREVIEW_MAX_BYTES, copy_notice, design_url, detail_text, lock_lost, lock_text, lock_url,
        normalize_etag, parse, path, save_failure, size_text, taken, valid_name, with_extension,
    };
    use crate::disk;
    use crate::files::{self, Files, Intent, err};

    thread_local! {
        /// The page's site has the file server.
        static ACTIVE: Cell<bool> = const { Cell::new(false) };
        /// The ETag of each design as it was loaded or last saved, by name.
        static ETAGS: RefCell<HashMap<String, String>> = RefCell::new(HashMap::new());
        /// Saves waiting for their turn (one at a time, so each sends the ETag of the one before),
        /// latest per design.
        static QUEUE: RefCell<VecDeque<(String, Vec<u8>)>> = const { RefCell::new(VecDeque::new()) };
        static BUSY: Cell<bool> = const { Cell::new(false) };
        /// The lock this page holds on each design it has open: the server's token, which every
        /// save and the renewals send back (`cloud/server/src/locks.rs`).
        static LOCKS: RefCell<HashMap<String, String>> = RefCell::new(HashMap::new());
    }

    /// Said about Open for a design somebody has.
    const COPY_HINT: &str = "Gesperrt: Das Design wird als Kopie geöffnet, Speichern geht nur unter einem neuen Namen.";
    /// Said about lifting somebody else's lock.
    const FORCE_HINT: &str = "Nur wenn niemand mehr daran arbeitet (zugeklapptes Notebook). Danach kann es jemand anderes öffnen.";

    /// Does this page hold the lock on `name`?
    fn holds(name: &str) -> bool {
        LOCKS.with(|l| l.borrow().contains_key(name))
    }

    /// Does this page save designs on the file server?
    pub fn active() -> bool {
        ACTIVE.with(Cell::get)
    }

    /// Saves to the server still under way (leaving the page asks first).
    pub fn uploading() -> bool {
        BUSY.with(Cell::get) || QUEUE.with(|q| !q.borrow().is_empty())
    }

    /// A design on the server, as the list shows it.
    #[derive(Clone, Debug)]
    pub struct Entry {
        pub name: String,
        pub size: u64,
        /// Last written (ms since 1970).
        pub modified_ms: f64,
        /// The version the list saw: a picture read from it is stale once this changes.
        pub etag: String,
        /// Who has it open.
        pub lock_owner: Option<String>,
        /// That is the person at this page (in this or another window).
        pub mine: bool,
    }

    /// A design fetched to be opened.
    struct Opened {
        /// Its path for the engine.
        path: String,
        /// Somebody has the design: this is a copy with no file of its own, and what to say
        /// about it.
        copy: Option<String>,
    }

    /// A design's picture for the start page, read from the design itself.
    struct Preview {
        /// The version it was read from.
        etag: String,
        /// The PNG inside the design; `None`: it has none (or it could not be fetched).
        png: Option<Vec<u8>>,
        /// The start page has been given it and keeps it as a texture.
        sent: bool,
    }

    /// Save As: the name being typed, and the answer the app waits for.
    struct SaveAsk {
        name: String,
        error: Option<String>,
        focus: bool,
        resolve: Function,
    }

    /// State shared between the async browser calls and the app's frames.
    pub struct Cloud {
        files: Rc<Files>,
        entries: RefCell<Vec<Entry>>,
        /// The list is being fetched.
        listing: Cell<bool>,
        list_error: RefCell<Option<String>>,
        /// The list window is open.
        panel: Cell<bool>,
        /// An Open waiting for its answer: the list window answers it.
        picking: RefCell<Option<Function>>,
        save_ask: RefCell<Option<SaveAsk>>,
        /// The design being fetched to open.
        loading: RefCell<Option<String>>,
        /// Designs fetched for opening (or open already in a tab).
        opened: RefCell<Vec<Opened>>,
        /// When the list was last fetched (ms since 1970), so the start page stays fresh.
        listed_at: Cell<f64>,
        /// Pictures for the start page, by design name.
        previews: RefCell<HashMap<String, Preview>>,
        /// One picture is being fetched (they go one at a time).
        fetching_preview: Cell<bool>,
        /// A lock the user asked to lift, waiting for the second click.
        unlocking: RefCell<Option<String>>,
    }

    impl Cloud {
        pub fn new(files: Rc<Files>) -> Self {
            Self {
                files,
                entries: RefCell::default(),
                listing: Cell::new(false),
                list_error: RefCell::default(),
                panel: Cell::new(false),
                picking: RefCell::default(),
                save_ask: RefCell::default(),
                loading: RefCell::default(),
                opened: RefCell::default(),
                listed_at: Cell::new(f64::NEG_INFINITY),
                previews: RefCell::default(),
                fetching_preview: Cell::new(false),
                unlocking: RefCell::default(),
            }
        }
    }

    /// Answer a waiting picker with an engine path, or `None` (cancelled).
    fn resolve(f: &Function, path: Option<&str>) {
        let _ = f.call1(&JsValue::NULL, &path.map_or(JsValue::NULL, JsValue::from_str));
    }

    /// A promise for a picker's answer, and the function that gives it.
    fn answer() -> (Promise, Option<Function>) {
        let mut given = None;
        let promise = Promise::new(&mut |resolve, _reject| given = Some(resolve));
        (promise, given)
    }

    async fn picked(promise: Promise) -> Option<String> {
        JsFuture::from(promise).await.ok().and_then(|v| v.as_string())
    }

    /// A server's answer.
    struct Reply {
        status: u16,
        etag: Option<String>,
        body: Vec<u8>,
    }

    impl Reply {
        fn json(&self) -> Value {
            serde_json::from_slice(&self.body).unwrap_or_default()
        }
    }

    async fn call(method: &str, url: &str, headers: &[(&str, &str)], body: Option<&[u8]>) -> Result<Reply, String> {
        let window = web_sys::window().ok_or("no window")?;
        let init = web_sys::RequestInit::new();
        init.set_method(method);
        // Never a copy from the browser's cache: the server's file may have changed.
        init.set_cache(web_sys::RequestCache::NoStore);
        init.set_credentials(web_sys::RequestCredentials::SameOrigin);
        let h = web_sys::Headers::new().map_err(err)?;
        for (k, v) in headers {
            h.set(k, v).map_err(err)?;
        }
        init.set_headers(&h);
        if let Some(b) = body {
            init.set_body(&Uint8Array::from(b));
        }
        let response: web_sys::Response =
            JsFuture::from(window.fetch_with_str_and_init(url, &init)).await.map_err(err)?.dyn_into().map_err(|_| "no response".to_string())?;
        let etag = response.headers().get("ETag").ok().flatten();
        let buf = JsFuture::from(response.array_buffer().map_err(err)?).await.map_err(err)?;
        Ok(Reply { status: response.status(), etag, body: Uint8Array::new(&buf).to_vec() })
    }

    /// Ask the page's site for the file server; with one, Open and Save As use it.
    pub fn start(cloud: &Rc<Cloud>) {
        let cloud = cloud.clone();
        spawn_local(async move {
            let found = match call("GET", &format!("{API}health"), &[], None).await {
                Ok(r) => r.status == 200 && r.json()["ok"].as_bool() == Some(true),
                Err(e) => {
                    log::info!("no file server: {e}");
                    false
                }
            };
            ACTIVE.with(|a| a.set(found));
            if found {
                log::info!("file server found: designs are opened from and saved to it");
                refresh(&cloud);
                start_heartbeat(&cloud);
                release_on_unload();
            }
            cloud.files.wake();
        });
    }

    /// Renew the locks this page holds every 20 s, whatever the window is doing: a design can be
    /// open and untouched for minutes, and egui draws only when something happens.
    fn start_heartbeat(cloud: &Rc<Cloud>) {
        let Some(window) = web_sys::window() else { return };
        let cloud = cloud.clone();
        let beat = wasm_bindgen::closure::Closure::<dyn FnMut()>::new(move || heartbeat(&cloud));
        if window.set_interval_with_callback_and_timeout_and_arguments_0(beat.as_ref().unchecked_ref(), HEARTBEAT_MS).is_ok() {
            // The timer lives as long as the page.
            beat.forget();
        }
    }

    /// Renew every lock. One the server does not know any more (it ran out while the notebook
    /// slept, or the server was restarted) is taken again; where that fails the design belongs to
    /// someone else now and the user is told, so the work can be saved under another name.
    fn heartbeat(cloud: &Rc<Cloud>) {
        let held: Vec<(String, String)> = LOCKS.with(|l| l.borrow().iter().map(|(name, token)| (name.clone(), token.clone())).collect());
        for (name, token) in held {
            let cloud = cloud.clone();
            spawn_local(async move {
                match call("PUT", &lock_url(&name), &[("X-Lock-Token", token.as_str())], None).await {
                    Ok(r) if r.status != 409 => return,
                    Err(e) => {
                        log::info!("lock not renewed ({name}): {e}");
                        return;
                    }
                    Ok(_) => {}
                }
                match lock(&name).await {
                    Ok(None) => log::info!("lock taken again: {name}"),
                    Ok(Some((owner, _))) => {
                        LOCKS.with(|l| l.borrow_mut().remove(&name));
                        cloud.files.say(lock_lost(&name, &owner), true);
                        refresh(&cloud);
                        cloud.files.wake();
                    }
                    Err(e) => log::info!("lock not taken again ({name}): {e}"),
                }
            });
        }
    }

    /// Let go of every lock when the page goes away, so a closed tab doesn't keep designs locked
    /// for a minute. After a crash the locks run out by themselves.
    fn release_on_unload() {
        let Some(window) = web_sys::window() else { return };
        let going = wasm_bindgen::closure::Closure::<dyn FnMut()>::new(move || {
            let held: Vec<(String, String)> = LOCKS.with(|l| l.borrow_mut().drain().collect());
            for (name, token) in held {
                send_release(&name, &token);
            }
        });
        let _ = window.add_event_listener_with_callback("pagehide", going.as_ref().unchecked_ref());
        // The listener lives as long as the page.
        going.forget();
    }

    /// Give up the lock on `name` with a request the browser finishes even as the page closes
    /// (`keepalive`, which web-sys has no setter for).
    fn send_release(name: &str, token: &str) {
        let Some(window) = web_sys::window() else { return };
        let init = web_sys::RequestInit::new();
        init.set_method("DELETE");
        init.set_credentials(web_sys::RequestCredentials::SameOrigin);
        let _ = js_sys::Reflect::set(init.as_ref(), &JsValue::from_str("keepalive"), &JsValue::TRUE);
        let Ok(headers) = web_sys::Headers::new() else { return };
        if headers.set("X-Lock-Token", token).is_err() {
            return;
        }
        init.set_headers(&headers);
        let sent = window.fetch_with_str_and_init(&lock_url(name), &init);
        spawn_local(async move {
            let _ = JsFuture::from(sent).await;
        });
    }

    /// Take the lock on `name`. `Ok(None)`: it is this page's now. `Ok(Some((owner, mine)))`:
    /// somebody has the design open (`mine`: the same person, in another window).
    async fn lock(name: &str) -> Result<Option<(String, bool)>, String> {
        let r = call("POST", &lock_url(name), &[], None).await.map_err(|e| format!("Server nicht erreichbar: {e}"))?;
        let v = r.json();
        match r.status {
            200 => match v["token"].as_str() {
                Some(token) => {
                    LOCKS.with(|l| l.borrow_mut().insert(name.to_string(), token.to_string()));
                    Ok(None)
                }
                None => Err(format!("„{name}“ nicht gesperrt: Der Server nennt kein Token.")),
            },
            409 => Ok(Some((v["owner"].as_str().unwrap_or("jemand anderem").to_string(), v["mine"].as_bool() == Some(true)))),
            s => Err(format!(
                "„{name}“ nicht gesperrt: Der Server antwortet {s}{}",
                v["error"].as_str().map(|e| format!(" ({e})")).unwrap_or_default()
            )),
        }
    }

    /// Let go of the lock on `name` (no tab shows the design any more).
    fn release(name: &str) {
        let Some(token) = LOCKS.with(|l| l.borrow_mut().remove(name)) else { return };
        let url = lock_url(name);
        spawn_local(async move {
            if let Err(e) = call("DELETE", &url, &[("X-Lock-Token", token.as_str())], None).await {
                log::info!("lock not released: {e}");
            }
        });
    }

    /// Fetch the list of designs.
    fn refresh(cloud: &Rc<Cloud>) {
        if cloud.listing.replace(true) {
            return;
        }
        cloud.listed_at.set(js_sys::Date::now());
        let cloud = cloud.clone();
        spawn_local(async move {
            let r = call("GET", &format!("{API}designs"), &[], None).await;
            let result = match r {
                Ok(r) if r.status == 200 => {
                    let v = r.json();
                    let mut entries: Vec<Entry> = v["designs"]
                        .as_array()
                        .map(|a| a.as_slice())
                        .unwrap_or_default()
                        .iter()
                        .take(10_000)
                        .filter_map(|d| {
                            Some(Entry {
                                name: d["name"].as_str()?.to_string(),
                                size: d["size"].as_u64().unwrap_or(0),
                                modified_ms: d["modified_ms"].as_f64().unwrap_or(0.0),
                                etag: d["etag"].as_str().map(normalize_etag).unwrap_or_default(),
                                lock_owner: d["lock"]["owner"].as_str().map(str::to_string),
                                mine: d["lock"]["mine"].as_bool() == Some(true),
                            })
                        })
                        .collect();
                    entries.sort_by_key(|e| e.name.to_lowercase());
                    Ok(entries)
                }
                Ok(r) => {
                    Err(format!("Der Server antwortet {}{}", r.status, r.json()["error"].as_str().map(|e| format!(": {e}")).unwrap_or_default()))
                }
                Err(e) => Err(format!("Server nicht erreichbar: {e}")),
            };
            match result {
                Ok(entries) => {
                    *cloud.entries.borrow_mut() = entries;
                    *cloud.list_error.borrow_mut() = None;
                }
                Err(e) => *cloud.list_error.borrow_mut() = Some(e),
            }
            cloud.listing.set(false);
            cloud.files.wake();
        });
    }

    /// Fetch a design's bytes and the ETag of the version that came.
    async fn download(name: &str) -> Result<(Vec<u8>, Option<String>), String> {
        let r = call("GET", &design_url(name), &[], None).await.map_err(|e| format!("Server nicht erreichbar: {e}"))?;
        match r.status {
            200 => {
                let tag = r.etag.as_deref().map(normalize_etag);
                Ok((r.body, tag))
            }
            404 => Err(format!("„{name}“ gibt es auf dem Server nicht mehr.")),
            s => Err(format!(
                "„{name}“ nicht geladen: Der Server antwortet {s}{}",
                r.json()["error"].as_str().map(|e| format!(" ({e})")).unwrap_or_default()
            )),
        }
    }

    /// Open a design from the server: take its lock and put it into the engine's files, so saving
    /// writes it back over the version that came (`If-Match`). Does somebody have it, the design
    /// is opened as a **copy** with no file of its own: a save then asks for a new name and their
    /// work stays as it is.
    async fn fetch(cloud: &Rc<Cloud>, name: &str) -> Result<Opened, String> {
        let held = lock(name).await?;
        let fetched = download(name).await;
        if fetched.is_err() && held.is_none() {
            release(name);
        }
        let (bytes, tag) = fetched?;
        // The start page shows this design: its picture comes free with the design.
        if let Some(etag) = tag.clone() {
            let png = solidcraft_engine::io::read_design_thumbnail(&bytes);
            cloud.previews.borrow_mut().insert(name.to_string(), Preview { etag, png, sent: false });
        }
        let Some((owner, mine)) = held else {
            ETAGS.with(|t| match tag {
                Some(tag) => t.borrow_mut().insert(name.to_string(), tag),
                None => t.borrow_mut().remove(name),
            });
            let p = path(name);
            vfs::insert(&p, bytes);
            return Ok(Opened { path: p, copy: None });
        };
        // A copy is kept in memory only (`disk::route`), and never written back to the server.
        let p = format!("{}{name}", files::UPLOAD);
        vfs::insert(&p, bytes);
        Ok(Opened { path: p, copy: Some(copy_notice(name, &owner, mine)) })
    }

    /// Open (File > Open, the start page's Open): the list of designs on the server answers it.
    pub fn pick_open(cloud: &Rc<Cloud>) -> impl std::future::Future<Output = Option<String>> + use<> {
        let (promise, given) = answer();
        if let Some(old) = cloud.picking.replace(given) {
            resolve(&old, None);
        }
        show(cloud);
        picked(promise)
    }

    /// Show the list of designs on the server.
    pub fn show(cloud: &Rc<Cloud>) {
        cloud.panel.set(true);
        refresh(cloud);
        cloud.files.wake();
    }

    /// Save As for a design: ask for its name on the server.
    pub fn pick_save(cloud: &Rc<Cloud>, name: &str) -> impl std::future::Future<Output = Option<String>> + use<> {
        let (promise, given) = answer();
        if let Some(resolve_with) = given {
            let ask = SaveAsk { name: name.to_string(), error: None, focus: true, resolve: resolve_with };
            if let Some(old) = cloud.save_ask.replace(Some(ask)) {
                resolve(&old.resolve, None);
            }
        }
        refresh(cloud);
        cloud.files.wake();
        picked(promise)
    }

    /// A design on the server chosen on the start page, or a recent one after a reload: fetch it
    /// and open it.
    pub fn restore(cloud: &Rc<Cloud>, engine_path: &str) {
        let Some(name) = parse(engine_path) else { return };
        if cloud.loading.borrow().is_some() {
            return;
        }
        *cloud.loading.borrow_mut() = Some(name.to_string());
        let (cloud, name, engine_path) = (cloud.clone(), name.to_string(), engine_path.to_string());
        spawn_local(async move {
            match fetch(&cloud, &name).await {
                Ok(opened) => cloud.opened.borrow_mut().push(opened),
                Err(e) => {
                    cloud.files.gone.borrow_mut().push(engine_path);
                    cloud.files.say(e, true);
                }
            }
            *cloud.loading.borrow_mut() = None;
            cloud.files.wake();
        });
    }

    // ---- the start page ------------------------------------------------------------------

    /// The designs on the server for the start page (`Services::server_designs`), which shows
    /// them in place of the built-in samples. The page asks for them while it is on screen, so
    /// this is also where the list is kept fresh and the pictures are read.
    pub fn home_designs(cloud: &Rc<Cloud>) -> Option<ServerDesigns> {
        if !active() {
            return None;
        }
        if js_sys::Date::now() - cloud.listed_at.get() > LIST_FRESH_MS {
            refresh(cloud);
        }
        // egui draws when something happens, and the start page just sits there: without asking
        // for the next frame the list would stand still until the mouse moves, and a design
        // somebody opened or saved would never show up by itself.
        ask_again(cloud, LIST_FRESH_MS - (js_sys::Date::now() - cloud.listed_at.get()));
        want_preview(cloud);
        let entries = cloud.entries.borrow();
        let mut previews = cloud.previews.borrow_mut();
        let designs = entries
            .iter()
            .map(|e| {
                let lock = e.lock_owner.as_ref().map(|owner| lock_text(owner, e.mine, holds(&e.name)));
                let picture = previews.get_mut(&e.name).filter(|p| p.etag == e.etag && !p.sent).and_then(|p| {
                    p.sent = true;
                    p.png.clone()
                });
                ServerDesign { path: path(&e.name), detail: detail_text(lock.as_deref(), e.size, &local_time(e.modified_ms)), picture }
            })
            .collect();
        // Shown while there are none: reading, not reachable, or really none yet.
        let failed = cloud.list_error.borrow().clone();
        let empty = match failed {
            Some(e) => format!("Die Liste kam nicht: {e}"),
            None if cloud.listing.get() => "Lese die Liste vom Server …".to_string(),
            None => "Noch keine Designs auf dem Server. Mit „Speichern unter“ legst du eines an.".to_string(),
        };
        Some(ServerDesigns { title: "Designs auf dem Server".into(), empty, recent_label: "Auf dem Server".into(), prefix: PREFIX.into(), designs })
    }

    /// Draw the start page again in `ms` milliseconds, so it comes back for the list on its own.
    /// At most one frame per [`LIST_FRESH_MS`], and never a busy loop while a fetch is running.
    fn ask_again(cloud: &Rc<Cloud>, ms: f64) {
        if let Some(ctx) = cloud.files.ctx.borrow().as_ref() {
            ctx.request_repaint_after(std::time::Duration::from_secs_f64(ms.clamp(1_000.0, LIST_FRESH_MS) / 1_000.0));
        }
    }

    /// Read one design's picture for the start page: the picture sits inside the design, so each
    /// one is fetched whole, one at a time and only up to [`PREVIEW_MAX_BYTES`].
    fn want_preview(cloud: &Rc<Cloud>) {
        if cloud.fetching_preview.get() || cloud.loading.borrow().is_some() || uploading() {
            return;
        }
        let next = {
            let entries = cloud.entries.borrow();
            let previews = cloud.previews.borrow();
            entries
                .iter()
                .find(|e| !e.etag.is_empty() && e.size > 0 && e.size <= PREVIEW_MAX_BYTES && !previews.get(&e.name).is_some_and(|p| p.etag == e.etag))
                .map(|e| (e.name.clone(), e.etag.clone()))
        };
        let Some((name, etag)) = next else { return };
        cloud.fetching_preview.set(true);
        let cloud = cloud.clone();
        spawn_local(async move {
            let png = match download(&name).await {
                Ok((bytes, _)) => solidcraft_engine::io::read_design_thumbnail(&bytes),
                Err(e) => {
                    log::info!("no picture for {name}: {e}");
                    None
                }
            };
            // Kept under the version it was read from, so it is read again once that changes and
            // a design it could not be read from is not fetched over and over.
            cloud.previews.borrow_mut().insert(name, Preview { etag, png, sent: false });
            cloud.fetching_preview.set(false);
            cloud.files.wake();
        });
    }

    // ---- saving --------------------------------------------------------------------------

    /// Queue what a save wrote to a server path.
    pub fn enqueue(files: &Rc<Files>, engine_path: &str, bytes: Vec<u8>) {
        QUEUE.with(|q| {
            let mut q = q.borrow_mut();
            q.retain(|(p, _)| p != engine_path);
            q.push_back((engine_path.to_string(), bytes));
        });
        if BUSY.with(|b| b.replace(true)) {
            return;
        }
        let files = files.clone();
        spawn_local(async move {
            while let Some((engine_path, bytes)) = QUEUE.with(|q| q.borrow_mut().pop_front()) {
                match upload(&engine_path, &bytes).await {
                    Ok(name) => files.say(format!("Auf dem Server gespeichert: {name}"), false),
                    Err(e) => {
                        // The design is not saved after all.
                        files.unsaved.borrow_mut().push(engine_path);
                        files.say(e, true);
                    }
                }
            }
            BUSY.with(|b| b.set(false));
            files.wake();
        });
    }

    /// Save a design on the server: over the version it was loaded as (`If-Match`), or as a new
    /// file (no ETag: the server refuses to overwrite one that is there).
    async fn upload(engine_path: &str, bytes: &[u8]) -> Result<String, String> {
        let name = parse(engine_path).ok_or("kein gültiger Name für den Server")?.to_string();
        let etag = ETAGS.with(|t| t.borrow().get(&name).cloned());
        let token = LOCKS.with(|t| t.borrow().get(&name).cloned());
        let mut headers = vec![("Content-Type", "application/octet-stream")];
        if let Some(tag) = &etag {
            headers.push(("If-Match", tag.as_str()));
        }
        // Our own lock has to be shown too: the server takes a write only with its token.
        if let Some(t) = &token {
            headers.push(("X-Lock-Token", t.as_str()));
        }
        let r = call("PUT", &design_url(&name), &headers, Some(bytes))
            .await
            .map_err(|e| format!("„{name}“ nicht gespeichert: Server nicht erreichbar ({e})."))?;
        let v = r.json();
        match r.status {
            200 | 201 => {
                // The server's ETag in its JSON: a proxy may rewrite the header's.
                let tag = v["etag"].as_str().map(str::to_string).or(r.etag).map(|t| normalize_etag(&t));
                ETAGS.with(|t| match tag {
                    Some(tag) => t.borrow_mut().insert(name.clone(), tag),
                    None => t.borrow_mut().remove(&name),
                });
                // Saved under a new name (Save As): the design is open here now, so take its lock.
                if token.is_none() {
                    let name = name.clone();
                    spawn_local(async move {
                        if let Err(e) = lock(&name).await {
                            log::info!("{e}");
                        }
                    });
                }
                Ok(name)
            }
            s => Err(save_failure(s, &name, v["error"].as_str(), v["owner"].as_str())),
        }
    }

    // ---- the app's frames ----------------------------------------------------------------

    /// Open fetched designs (a design open already: its tab), and let go of the designs no tab
    /// shows any more (opening one again fetches the server's version).
    pub fn tick(app: &mut SolveApp, cloud: &Rc<Cloud>) {
        if app.file_picker_busy() || !app.file_load.idle() {
            return;
        }
        let opened: Vec<Opened> = cloud.opened.borrow_mut().drain(..).collect();
        for Opened { path: p, copy } in opened {
            match documents::find_path(app, &p) {
                Some(i) => {
                    documents::switch(app, i);
                    app.home.open = false;
                }
                None => {
                    app.open_path(&p);
                    let Some(notice) = copy else { continue };
                    // A copy of a design somebody has: it keeps no file, so Save asks for a name
                    // on the server instead of writing over their work.
                    if app.session.path.as_deref() == Some(p.as_str()) {
                        app.session.path = None;
                        let title = format!("{} (Kopie)", app.session.doc.name);
                        app.session.doc_mut().name = title;
                        app.session.mark_saved();
                        app.home.recent.retain(|r| *r != p);
                        app.home.opened_at.remove(&p);
                    }
                    cloud.files.say(notice, true);
                }
            }
        }
        if cloud.loading.borrow().is_some() || uploading() {
            return;
        }
        for (p, _) in vfs::list(PREFIX) {
            if documents::find_path(app, &p).is_none() {
                let _ = vfs::remove(&p);
                if let Some(name) = parse(&p) {
                    ETAGS.with(|t| t.borrow_mut().remove(name));
                    // Nobody here shows it any more: the others may have it.
                    release(name);
                }
            }
        }
        // A lock whose design left the engine's files another way is let go of as well.
        let held: Vec<String> = LOCKS.with(|l| l.borrow().keys().cloned().collect());
        for name in held {
            if !vfs::exists(&path(&name)) {
                release(&name);
            }
        }
    }

    /// The list of designs on the server, and the Save As name.
    pub fn ui(app: &mut SolveApp, ctx: &egui::Context, cloud: &Rc<Cloud>) {
        if !active() {
            return;
        }
        panel(app, ctx, cloud);
        save_dialog(ctx, cloud);
    }

    fn close_panel(cloud: &Rc<Cloud>) {
        cloud.panel.set(false);
        if let Some(f) = cloud.picking.borrow_mut().take() {
            resolve(&f, None);
        }
    }

    fn local_time(ms: f64) -> String {
        if ms <= 0.0 {
            return String::new();
        }
        String::from(js_sys::Date::new(&JsValue::from_f64(ms)).to_locale_string("de-DE", &JsValue::UNDEFINED))
    }

    fn panel(app: &mut SolveApp, ctx: &egui::Context, cloud: &Rc<Cloud>) {
        if !cloud.panel.get() {
            return;
        }
        let picking = cloud.picking.borrow().is_some();
        let mut act: Option<(&'static str, String)> = None;
        let error = Tokens::get().error;
        let title = if picking { "Design öffnen (Server)" } else { "Designs auf dem Server" };
        let closed = ui_kit::Window::new(egui::Id::new("solidcraft_cloud_designs"), "open", title).cards(3.5).show_body(ctx, |ui| {
            ui.horizontal(|ui| {
                if ui_kit::button(ui, "Aktualisieren", ui_kit::Button::Normal).clicked() {
                    act = Some(("refresh", String::new()));
                }
                if ui_kit::button(ui, "Vom Computer öffnen…", ui_kit::Button::Normal)
                    .on_hover_text("Eine Datei von diesem Computer öffnen. Mit „Speichern unter“ kommt sie auf den Server.")
                    .clicked()
                {
                    act = Some(("computer", String::new()));
                }
                if ui_kit::button(ui, "Dateien in diesem Browser…", ui_kit::Button::Normal).clicked() {
                    act = Some(("browser", String::new()));
                }
            });
            ui.separator();
            if let Some(e) = cloud.list_error.borrow().as_ref() {
                ui.colored_label(error, e);
            }
            if let Some(name) = cloud.loading.borrow().as_ref() {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(format!("Lade „{name}“ …"));
                });
            }
            let entries = cloud.entries.borrow().clone();
            if entries.is_empty() {
                if cloud.listing.get() {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Lese die Liste vom Server …");
                    });
                } else if cloud.list_error.borrow().is_none() {
                    ui.label("Noch keine Designs auf dem Server. Mit „Speichern unter“ legst du eines an.");
                }
            }
            let ready = cloud.loading.borrow().is_none();
            // The window's text is cut to fit; the list's names, dates and buttons are shown whole.
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
            let asked = cloud.unlocking.borrow().clone();
            egui::Grid::new("cloud_designs").num_columns(5).striped(true).spacing([12.0, 6.0]).show(ui, |ui| {
                for e in &entries {
                    let here = holds(&e.name);
                    let open_elsewhere = e.lock_owner.is_some() && !here;
                    ui.horizontal(|ui| {
                        let open = ui_kit::button(ui, "Öffnen", ui_kit::Button::Enabled(ready));
                        if open_elsewhere {
                            open.clone().on_hover_text(COPY_HINT);
                        }
                        if open.clicked() {
                            act = Some(("open", e.name.clone()));
                        }
                        if ui_kit::button(ui, "Herunterladen", ui_kit::Button::Enabled(ready)).clicked() {
                            act = Some(("download", e.name.clone()));
                        }
                    });
                    ui.label(&e.name);
                    ui.label(size_text(e.size));
                    ui.label(local_time(e.modified_ms));
                    match &e.lock_owner {
                        Some(owner) => {
                            ui.horizontal(|ui| {
                                ui.weak(lock_text(owner, e.mine, here));
                                if !open_elsewhere {
                                    return;
                                }
                                if asked.as_deref() == Some(e.name.as_str()) {
                                    if ui_kit::button(ui, "Wirklich aufheben", ui_kit::Button::Normal).clicked() {
                                        act = Some(("force", e.name.clone()));
                                    }
                                    if ui_kit::button(ui, "Abbrechen", ui_kit::Button::Normal).clicked() {
                                        act = Some(("keep", String::new()));
                                    }
                                } else if ui_kit::button(ui, "Sperre aufheben", ui_kit::Button::Normal).on_hover_text(FORCE_HINT).clicked() {
                                    act = Some(("ask_force", e.name.clone()));
                                }
                            });
                        }
                        None => {
                            ui.label("");
                        }
                    }
                    ui.end_row();
                }
            });
        });
        if closed {
            close_panel(cloud);
            return;
        }
        let Some((what, name)) = act else { return };
        match what {
            "refresh" => refresh(cloud),
            "ask_force" => *cloud.unlocking.borrow_mut() = Some(name),
            "keep" => *cloud.unlocking.borrow_mut() = None,
            "force" => {
                *cloud.unlocking.borrow_mut() = None;
                let cloud = cloud.clone();
                spawn_local(async move {
                    let lifted = match call("DELETE", &format!("{}?force=1", lock_url(&name)), &[], None).await {
                        Ok(r) if r.status == 204 || r.status == 200 => Ok(()),
                        Ok(r) => Err(format!("Der Server antwortet {}", r.status)),
                        Err(e) => Err(format!("Server nicht erreichbar ({e})")),
                    };
                    match lifted {
                        Ok(()) => cloud.files.say(format!("Die Sperre für „{name}“ ist aufgehoben."), false),
                        Err(e) => cloud.files.say(format!("Die Sperre für „{name}“ blieb: {e}."), true),
                    }
                    refresh(&cloud);
                    cloud.files.wake();
                });
            }
            "browser" => {
                close_panel(cloud);
                cloud.files.panel.set(true);
            }
            "computer" => {
                let waiting = cloud.picking.borrow_mut().take();
                cloud.panel.set(false);
                match waiting {
                    // An Open (or an Insert STEP…) waits: the browser's open dialog answers it
                    // (in the click), as it would without the server.
                    Some(f) if disk::supported() => {
                        let chosen = disk::pick_open(&cloud.files);
                        spawn_local(async move {
                            let p = chosen.await;
                            resolve(&f, p.as_deref());
                        });
                    }
                    // Without that dialog, the browser's files window, as without the server.
                    Some(f) => {
                        resolve(&f, None);
                        cloud.files.panel.set(true);
                    }
                    None => files::pick_from_disk(&cloud.files, Intent::Open),
                }
            }
            "open" => {
                let p = path(&name);
                // Open already: its tab, not a second copy that would save over it.
                if documents::find_path(app, &p).is_some() {
                    close_panel(cloud);
                    cloud.opened.borrow_mut().push(Opened { path: p, copy: None });
                    return;
                }
                *cloud.loading.borrow_mut() = Some(name.clone());
                let cloud = cloud.clone();
                spawn_local(async move {
                    match fetch(&cloud, &name).await {
                        Ok(opened) => {
                            cloud.panel.set(false);
                            // A copy has no file: an Open waiting for a path is answered with the
                            // copy's, and the tab lets go of it (see `tick`).
                            match cloud.picking.borrow_mut().take() {
                                Some(f) if opened.copy.is_none() => resolve(&f, Some(&opened.path)),
                                Some(f) => {
                                    resolve(&f, None);
                                    cloud.opened.borrow_mut().push(opened);
                                }
                                None => cloud.opened.borrow_mut().push(opened),
                            }
                        }
                        Err(e) => {
                            cloud.files.say(e, true);
                            refresh(&cloud);
                        }
                    }
                    *cloud.loading.borrow_mut() = None;
                    cloud.files.wake();
                });
            }
            "download" => {
                let cloud = cloud.clone();
                spawn_local(async move {
                    let r = match call("GET", &design_url(&name), &[], None).await {
                        Ok(r) if r.status == 200 => files::download(&name, &r.body),
                        Ok(r) => Err(format!("Der Server antwortet {}", r.status)),
                        Err(e) => Err(format!("Server nicht erreichbar: {e}")),
                    };
                    if let Err(e) = r {
                        cloud.files.say(format!("„{name}“ nicht heruntergeladen: {e}"), true);
                    }
                });
            }
            _ => {}
        }
    }

    fn save_dialog(ctx: &egui::Context, cloud: &Rc<Cloud>) {
        let mut slot = cloud.save_ask.borrow_mut();
        let Some(ask) = slot.as_mut() else { return };
        let mut answer: Option<Option<String>> = None;
        let error = Tokens::get().error;
        let closed = ui_kit::Window::new(egui::Id::new("solidcraft_cloud_save"), "save", "Auf dem Server speichern").show_body(ctx, |ui| {
            ui.label("Name des Designs:");
            let r = ui_kit::cell_editor(ui, egui::Id::new("solidcraft_cloud_save_name"), &mut ask.name, "", ask.error.is_some());
            if std::mem::take(&mut ask.focus) {
                r.request_focus();
            }
            ui.weak("Ohne Endung wird .solidcraft ergänzt.");
            ui.weak("Alle im Team sehen das Design.");
            if let Some(e) = &ask.error {
                ui.colored_label(error, e);
            }
            let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            ui.horizontal(|ui| {
                if ui_kit::button(ui, "Speichern", ui_kit::Button::Normal).clicked() || enter {
                    let name = with_extension(&ask.name);
                    let entries = cloud.entries.borrow();
                    answer = match valid_name(&name) {
                        Err(e) => {
                            ask.error = Some(e);
                            None
                        }
                        Ok(()) if taken(entries.iter().map(|e| e.name.as_str()), &name) => {
                            ask.error = Some(format!("„{name}“ gibt es auf dem Server schon. Bitte einen anderen Namen wählen."));
                            None
                        }
                        Ok(()) => Some(Some(path(&name))),
                    };
                }
                if ui_kit::button(ui, "Abbrechen", ui_kit::Button::Normal).clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    answer = Some(None);
                }
            });
        });
        if closed {
            answer = Some(None);
        }
        if let Some(chosen) = answer
            && let Some(ask) = slot.take()
        {
            resolve(&ask.resolve, chosen.as_deref());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_server_path_round_trips_its_name() {
        let p = path("Mein Gehäuse (2).solidcraft");
        assert_eq!(p, "/server/Mein Gehäuse (2).solidcraft");
        assert_eq!(parse(&p), Some("Mein Gehäuse (2).solidcraft"));
    }

    /// Only names the server would take parse: other prefixes, folders, hidden files and other
    /// extensions don't, so nothing else is ever sent there.
    #[test]
    fn only_server_paths_with_valid_names_parse() {
        for not in [
            "/opfs/a.solidcraft",
            "/disk/id/a.solidcraft",
            "/server/",
            "/server/a/b.solidcraft",
            "/server/../a.solidcraft",
            "/server/.hidden.solidcraft",
            "/server/part.step",
            "/server/a\\b.solidcraft",
            "server/a.solidcraft",
        ] {
            assert_eq!(parse(not), None, "{not}");
        }
        assert_eq!(parse("/server/a.json"), Some("a.json"));
        assert_eq!(parse("/server/old.solvecraft"), Some("old.solvecraft"));
    }

    /// The same rules as the server's `valid_name` (cloud/server/src/store.rs).
    #[test]
    fn names_follow_the_servers_rules() {
        assert!(valid_name("Halter v2, links (neu)_final-3.solidcraft").is_ok());
        assert!(valid_name("Größe.SOLIDCRAFT").is_ok());
        for bad in ["", ".solidcraft", " a.solidcraft", "a .solidcraft ", "a..b.solidcraft", "a/b.solidcraft", "a:b.solidcraft", "a.txt", "a"] {
            assert!(valid_name(bad).is_err(), "{bad:?}");
        }
        assert!(valid_name(&format!("{}.solidcraft", "a".repeat(200))).is_err());
    }

    #[test]
    fn save_as_adds_the_design_extension_when_it_is_missing() {
        assert_eq!(with_extension(" Halter "), "Halter.solidcraft");
        assert_eq!(with_extension("Halter.solidcraft"), "Halter.solidcraft");
        assert_eq!(with_extension("Halter.JSON"), "Halter.JSON");
        assert_eq!(with_extension("Halter v1.2"), "Halter v1.2.solidcraft");
    }

    #[test]
    fn a_name_is_taken_whatever_its_case() {
        let names = ["Halter.solidcraft", "Deckel.solidcraft"];
        assert!(taken(names.iter().copied(), "halter.SOLIDCRAFT"));
        assert!(!taken(names.iter().copied(), "Halter 2.solidcraft"));
    }

    /// Names go into the URL path percent-encoded as UTF-8, as the server decodes them.
    #[test]
    fn names_are_percent_encoded_in_the_url() {
        assert_eq!(design_url("Mein Gehäuse (2).solidcraft"), "/api/designs/Mein%20Geh%C3%A4use%20%282%29.solidcraft");
        assert_eq!(encode("a,b_c-d.e~f"), "a%2Cb_c-d.e~f");
        assert_eq!(encode("x/../y"), "x%2F..%2Fy");
    }

    #[test]
    fn a_compressing_proxys_etag_is_taken_back_to_the_servers() {
        assert_eq!(normalize_etag("\"1a-2b.3c-4d\""), "\"1a-2b.3c-4d\"");
        assert_eq!(normalize_etag(" \"1a-2b.3c-4d-gzip\" "), "\"1a-2b.3c-4d\"");
        assert_eq!(normalize_etag("W/\"1a-2b.3c-4d-br\""), "\"1a-2b.3c-4d\"");
    }

    /// Every refusal says that the design is not saved and how to keep the changes.
    #[test]
    fn a_refused_save_says_why_and_what_to_do() {
        let changed = save_failure(412, "Halter.solidcraft", None, None);
        assert!(changed.contains("inzwischen geändert") && changed.contains("Speichern unter"));
        assert!(save_failure(428, "Halter.solidcraft", None, None).contains("Nicht überschrieben"));
        assert!(save_failure(423, "Halter.solidcraft", Some("Gesperrt"), Some("alice")).contains("gesperrt von alice"));
        assert!(save_failure(413, "Halter.solidcraft", None, None).contains("zu groß"));
        assert!(save_failure(500, "Halter.solidcraft", Some("Interner Fehler"), None).contains("500 (Interner Fehler)"));
    }

    /// A lock lives under the design it belongs to, with the same percent-encoded name.
    #[test]
    fn a_locks_url_is_the_designs_url_and_lock() {
        assert_eq!(lock_url("Mein Gehäuse (2).solidcraft"), "/api/designs/Mein%20Geh%C3%A4use%20%282%29.solidcraft/lock");
        assert_eq!(lock_url("A.solidcraft"), format!("{}/lock", design_url("A.solidcraft")));
    }

    /// The list says who has a design: this window, you somewhere else (a second tab locks too),
    /// or someone in the team.
    #[test]
    fn a_lock_says_whether_it_is_yours_and_whether_it_is_this_window() {
        assert_eq!(lock_text("alice", true, true), "hier geöffnet");
        assert_eq!(lock_text("alice", true, false), "von dir in einem anderen Fenster geöffnet");
        assert_eq!(lock_text("bob", false, false), "geöffnet von bob");
    }

    /// Opening a design somebody has gives a copy: the message names them and says that the
    /// changes are kept under a new name, never written over theirs.
    #[test]
    fn a_copy_says_who_has_the_design_and_how_to_keep_the_changes() {
        let theirs = copy_notice("Halter.solidcraft", "bob", false);
        assert!(theirs.contains("von bob geöffnet") && theirs.contains("Kopie") && theirs.contains("Speichern unter"));
        let mine = copy_notice("Halter.solidcraft", "alice", true);
        assert!(mine.contains("in einem anderen Fenster") && mine.contains("Speichern unter"));
        assert!(lock_lost("Halter.solidcraft", "bob").contains("abgelaufen") && lock_lost("Halter.solidcraft", "bob").contains("bob"));
    }

    /// The start page says who has a design, or how big it is, and when it was last written.
    #[test]
    fn the_start_pages_line_says_who_has_it_or_how_big_it_is() {
        assert_eq!(detail_text(Some("geöffnet von bob"), 2048, "21.10.2026, 09:00"), "geöffnet von bob · 21.10.2026, 09:00");
        assert_eq!(detail_text(None, 2048, "21.10.2026, 09:00"), "2.0 KB · 21.10.2026, 09:00");
        assert_eq!(detail_text(None, 2048, ""), "2.0 KB");
    }

    #[test]
    fn sizes_read_in_kilobytes_or_megabytes() {
        assert_eq!(size_text(2048), "2.0 KB");
        assert_eq!(size_text(3 * 1024 * 1024 + 512 * 1024), "3.5 MB");
    }
}
