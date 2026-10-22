//! SolidCraft_Cloud: designs on the team's file server (`cloud/server`, see `cloud/ARCHITECTURE.md`).
//! When the page's own site answers `GET /api/health`, Open lists the designs on the server and
//! Save As puts a design there. To the engine such a design is the file `/server/<name>`: it is
//! fetched into the engine's files when it is opened, and [`flush`](crate::files::flush) sends
//! what a save wrote back to the server with the ETag it was loaded with (`If-Match`), so a design
//! someone else changed meanwhile is never overwritten. A design from the server is kept in memory
//! only while a tab shows it: opening it again fetches the server's current version.
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
    use solidcraft_ui_egui::{SolveApp, documents};
    use wasm_bindgen::{JsCast as _, JsValue};
    use wasm_bindgen_futures::{JsFuture, spawn_local};

    use super::{API, PREFIX, design_url, normalize_etag, parse, path, save_failure, size_text, taken, valid_name, with_extension};
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
        /// Who has it open (M3 takes the locks; the list shows them already).
        pub lock_owner: Option<String>,
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
        /// Designs fetched for opening (or open already in a tab): their engine paths.
        opened: RefCell<Vec<String>>,
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
            }
            cloud.files.wake();
        });
    }

    /// Fetch the list of designs.
    fn refresh(cloud: &Rc<Cloud>) {
        if cloud.listing.replace(true) {
            return;
        }
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
                                lock_owner: d["lock"]["owner"].as_str().map(str::to_string),
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

    /// Fetch a design into the engine's files, remembering its ETag.
    async fn fetch(name: &str) -> Result<String, String> {
        let r = call("GET", &design_url(name), &[], None).await.map_err(|e| format!("Server nicht erreichbar: {e}"))?;
        match r.status {
            200 => {
                ETAGS.with(|t| match &r.etag {
                    Some(tag) => t.borrow_mut().insert(name.to_string(), normalize_etag(tag)),
                    None => t.borrow_mut().remove(name),
                });
                let p = path(name);
                vfs::insert(&p, r.body);
                Ok(p)
            }
            404 => Err(format!("„{name}“ gibt es auf dem Server nicht mehr.")),
            s => Err(format!(
                "„{name}“ nicht geladen: Der Server antwortet {s}{}",
                r.json()["error"].as_str().map(|e| format!(" ({e})")).unwrap_or_default()
            )),
        }
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

    /// A recent design on the server (after a reload): fetch it and open it.
    pub fn restore(cloud: &Rc<Cloud>, engine_path: &str) {
        let Some(name) = parse(engine_path) else { return };
        let (cloud, name, engine_path) = (cloud.clone(), name.to_string(), engine_path.to_string());
        spawn_local(async move {
            match fetch(&name).await {
                Ok(p) => cloud.opened.borrow_mut().push(p),
                Err(e) => {
                    cloud.files.gone.borrow_mut().push(engine_path);
                    cloud.files.say(e, true);
                }
            }
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
        let mut headers = vec![("Content-Type", "application/octet-stream")];
        if let Some(tag) = &etag {
            headers.push(("If-Match", tag.as_str()));
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
        let opened: Vec<String> = cloud.opened.borrow_mut().drain(..).collect();
        for p in opened {
            match documents::find_path(app, &p) {
                Some(i) => {
                    documents::switch(app, i);
                    app.home.open = false;
                }
                None => app.open_path(&p),
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
                }
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
        let mut open = true;
        let mut act: Option<(&'static str, String)> = None;
        egui::Window::new(if picking { "Design öffnen (Server)" } else { "Designs auf dem Server" })
            .id(egui::Id::new("solidcraft_cloud_designs"))
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_size([640.0, 360.0])
            .pivot(egui::Align2::CENTER_CENTER)
            .default_pos(ctx.content_rect().center())
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if ui.button("Aktualisieren").clicked() {
                        act = Some(("refresh", String::new()));
                    }
                    if ui
                        .button("Vom Computer öffnen…")
                        .on_hover_text("Eine Datei von diesem Computer öffnen. Mit „Speichern unter“ kommt sie auf den Server.")
                        .clicked()
                    {
                        act = Some(("computer", String::new()));
                    }
                    if ui.button("Dateien in diesem Browser…").clicked() {
                        act = Some(("browser", String::new()));
                    }
                });
                ui.separator();
                if let Some(e) = cloud.list_error.borrow().as_ref() {
                    ui.colored_label(ui.visuals().error_fg_color, e);
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
                let busy = cloud.loading.borrow().is_some();
                egui::ScrollArea::vertical().max_height(420.0).show(ui, |ui| {
                    egui::Grid::new("cloud_designs").num_columns(5).striped(true).spacing([12.0, 6.0]).show(ui, |ui| {
                        for e in &entries {
                            ui.horizontal(|ui| {
                                if ui.add_enabled(!busy, egui::Button::new("Öffnen").small()).clicked() {
                                    act = Some(("open", e.name.clone()));
                                }
                                if ui.add_enabled(!busy, egui::Button::new("Herunterladen").small()).clicked() {
                                    act = Some(("download", e.name.clone()));
                                }
                            });
                            ui.label(&e.name);
                            ui.label(size_text(e.size));
                            ui.label(local_time(e.modified_ms));
                            match &e.lock_owner {
                                Some(owner) => ui.weak(format!("geöffnet von {owner}")),
                                None => ui.label(""),
                            };
                            ui.end_row();
                        }
                    });
                });
            });
        if !open {
            close_panel(cloud);
            return;
        }
        let Some((what, name)) = act else { return };
        match what {
            "refresh" => refresh(cloud),
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
                    cloud.opened.borrow_mut().push(p);
                    return;
                }
                *cloud.loading.borrow_mut() = Some(name.clone());
                let cloud = cloud.clone();
                spawn_local(async move {
                    match fetch(&name).await {
                        Ok(p) => {
                            cloud.panel.set(false);
                            match cloud.picking.borrow_mut().take() {
                                Some(f) => resolve(&f, Some(&p)),
                                None => cloud.opened.borrow_mut().push(p),
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
        let mut open = true;
        egui::Window::new("Auf dem Server speichern")
            .id(egui::Id::new("solidcraft_cloud_save"))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label("Name des Designs:");
                let r = ui.add(egui::TextEdit::singleline(&mut ask.name).desired_width(360.0));
                if std::mem::take(&mut ask.focus) {
                    r.request_focus();
                }
                ui.weak("Ohne Endung wird .solidcraft ergänzt. Alle im Team sehen das Design.");
                if let Some(e) = &ask.error {
                    ui.colored_label(ui.visuals().error_fg_color, e);
                }
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                ui.horizontal(|ui| {
                    if ui.button("Speichern").clicked() || enter {
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
                    if ui.button("Abbrechen").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                        answer = Some(None);
                    }
                });
            });
        if !open {
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

    #[test]
    fn sizes_read_in_kilobytes_or_megabytes() {
        assert_eq!(size_text(2048), "2.0 KB");
        assert_eq!(size_text(3 * 1024 * 1024 + 512 * 1024), "3.5 MB");
    }
}
