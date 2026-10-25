//! SolidCraft_Cloud, desktop app: designs on the team's file server (`cloud/server`, see
//! `cloud/ARCHITECTURE.md`), the same as the browser build offers (`apps/solidcraft-web/src/server.rs`).
//!
//! File > Open shows the list of designs on the server (and, below it, the usual way to a file on
//! this computer). A design chosen there is **downloaded into a cache folder** and opened from
//! there as an ordinary file; its **lock** is taken first and renewed every 20 s while a tab shows
//! the design, and let go of when the tab is closed or the app ends. Save writes the cache file as
//! it always did; a moment later the changed file is **uploaded** with the ETag it was loaded with
//! (`If-Match`), so a design somebody else changed meanwhile is never overwritten. A design that
//! somebody else has open is opened as a **copy** with no file of its own: Save asks for a new name.
//! Save As asks for a name on the server. Where the file server is, and the login, are asked for
//! in the Open window; the password lives in the system keychain.
//!
//! Every network call runs on a worker thread. The window thread only reads the shared state in
//! its frame (`tick`, `ui`) and never waits for the network.

mod http;
mod names;
mod ui;

use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant, SystemTime};

use serde_json::Value;
use solidcraft_ui_egui::FilePickerFuture;
use solidcraft_ui_egui::{ServerDesign, ServerDesigns, SolveApp, documents};

use http::Conn;
use names::{
    PREFIX, copy_notice, design_path, detail_text, list_path, lock_lost, lock_path, lock_text, normalize_etag, parse_start_path, save_failure,
    size_text, start_path,
};

/// How often the locks this app holds are renewed. The server lets one run out after 60 s
/// (`SOLIDCRAFT_CLOUD_LOCK_TTL_SECS`), so a missed beat still leaves two more.
const HEARTBEAT: Duration = Duration::from_secs(20);
/// How long the list may be before it is fetched again.
const LIST_FRESH: Duration = Duration::from_secs(10);
/// How often the cache files are looked at for a save.
const WATCH: Duration = Duration::from_millis(500);
/// A design just opened has this long to appear in a tab before the lock is let go of.
const OPEN_GRACE: Duration = Duration::from_secs(20);
/// The largest design uploaded (the server's own limit).
const MAX_UPLOAD: u64 = 256 << 20;

/// Said about Open for a design somebody has.
pub(crate) const COPY_HINT: &str = "Gesperrt: Das Design wird als Kopie geöffnet, Speichern geht nur unter einem neuen Namen.";
/// Said about lifting somebody else's lock.
pub(crate) const FORCE_HINT: &str = "Nur wenn niemand mehr daran arbeitet (zugeklapptes Notebook). Danach kann es jemand anderes öffnen.";

/// An answer a picker waits for: the frame code (or a worker) gives it, the app's future takes it.
#[derive(Clone, Default)]
pub struct Answer(Arc<Mutex<AnswerState>>);

#[derive(Default)]
struct AnswerState {
    value: Option<Option<String>>,
    waker: Option<Waker>,
}

impl Answer {
    /// Answer once; a second answer is ignored.
    pub fn resolve(&self, value: Option<String>) {
        let mut s = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if s.value.is_none() {
            s.value = Some(value);
        }
        if let Some(w) = s.waker.take() {
            w.wake();
        }
    }
}

impl Future for Answer {
    type Output = Option<String>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<String>> {
        let mut s = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        match s.value.take() {
            Some(v) => Poll::Ready(v),
            None => {
                s.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

/// The version of a file as the disk shows it: when it was written, and how big it is.
type Sig = (SystemTime, u64);

fn sig(path: &Path) -> Option<Sig> {
    let m = std::fs::metadata(path).ok()?;
    Some((m.modified().ok()?, m.len()))
}

/// A design on the server, as the list shows it.
#[derive(Clone, Debug)]
pub struct Entry {
    pub name: String,
    pub size: u64,
    /// Last written (ms since 1970).
    pub modified_ms: f64,
    /// Who has it open.
    pub lock_owner: Option<String>,
    /// That is the person at this app (in this or another window).
    pub mine: bool,
}

/// A design of the server that this app has in its cache folder.
struct Tracked {
    path: PathBuf,
    /// The ETag of the version loaded or last uploaded; `None` for a design not on the server yet.
    etag: Option<String>,
    /// The server's token for this app's lock.
    token: Option<String>,
    /// The file as it was loaded or last uploaded.
    uploaded: Option<Sig>,
    /// The file as the last look saw it, when that differs from `uploaded`: a save is complete
    /// when the next look sees the same.
    pending: Option<Sig>,
    /// The server refused this version of the file; it is not offered again until it changes.
    failed: Option<Sig>,
    /// The lock ran out and somebody else has the design: nothing is uploaded.
    lost: bool,
    /// An upload or a release is under way.
    busy: bool,
    since: Instant,
    /// A tab has shown the design.
    seen_open: bool,
}

/// A design ready to be shown: its file, and for a copy what to tell the user.
struct Opened {
    path: String,
    copy: Option<String>,
}

/// Save As: the name being typed, and the answer the app waits for.
pub(crate) struct SaveAsk {
    pub name: String,
    pub error: Option<String>,
    pub focus: bool,
    pub answer: Answer,
}

/// The sign-in form of the Open window.
#[derive(Default)]
pub(crate) struct Form {
    pub url: String,
    pub user: String,
    pub password: String,
    /// The check of the entered address and login is running.
    pub checking: bool,
    pub error: Option<String>,
}

/// What the worker threads and the frames share.
pub(crate) struct Inner {
    /// The login, once the user has given one.
    pub conn: Option<Conn>,
    /// The file server answered and took the login.
    pub active: bool,
    pub form: Form,
    pub entries: Vec<Entry>,
    pub list_error: Option<String>,
    pub listing: bool,
    pub listed_at: Option<Instant>,
    /// The Open window is shown.
    pub panel: bool,
    /// An Open waiting for its answer: the Open window answers it.
    pub picking: Option<Answer>,
    pub save_ask: Option<SaveAsk>,
    /// The design being fetched to open.
    pub loading: Option<String>,
    /// A lock the user asked to lift, waiting for the second click.
    pub unlocking: Option<String>,
    opened: Vec<Opened>,
    copies: Vec<(String, String, Instant)>,
    tracked: HashMap<String, Tracked>,
    /// Words for the status line, with whether they are an error.
    messages: Vec<(String, bool)>,
}

pub(crate) struct Shared {
    inner: Mutex<Inner>,
    ctx: egui::Context,
    /// Where the cache folder and the settings are; `None`: nowhere to keep them.
    dir: Option<PathBuf>,
}

impl Shared {
    pub(crate) fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn say(&self, text: impl Into<String>, error: bool) {
        self.lock().messages.push((text.into(), error));
        self.ctx.request_repaint();
    }

    fn conn(&self) -> Option<Conn> {
        self.lock().conn.clone()
    }

    fn cache_path(&self, sub: &str, name: &str) -> Option<PathBuf> {
        Some(self.dir.as_ref()?.join(sub).join(name))
    }
}

impl Inner {
    /// The designs whose lock this app holds.
    pub(crate) fn tracked_names_with_lock(&self) -> Vec<String> {
        self.tracked.iter().filter(|(_, t)| t.token.is_some()).map(|(n, _)| n.clone()).collect()
    }
}

type PickOpen = Rc<dyn Fn(&[&str]) -> FilePickerFuture>;
type PickSave = Rc<dyn Fn(&str, &[&str]) -> FilePickerFuture>;

/// The cloud features of the desktop app. Built once; the hooks in `Services` and the frame code
/// in `main.rs` hold it.
pub struct Cloud {
    pub(crate) shared: Arc<Shared>,
    native_open: PickOpen,
    native_save: PickSave,
    /// A picker of this computer that the Open window started, and the Open it answers.
    native_wait: RefCell<Option<(FilePickerFuture, Answer)>>,
    last_watch: RefCell<Option<Instant>>,
}

impl Cloud {
    pub fn new(ctx: &egui::Context, dir: Option<PathBuf>, native_open: PickOpen, native_save: PickSave) -> Rc<Self> {
        let saved = dir.as_deref().and_then(settings::load);
        let shared = Arc::new(Shared {
            inner: Mutex::new(Inner {
                conn: None,
                active: false,
                form: Form::default(),
                entries: Vec::new(),
                list_error: None,
                listing: false,
                listed_at: None,
                panel: false,
                picking: None,
                save_ask: None,
                loading: None,
                unlocking: None,
                opened: Vec::new(),
                copies: Vec::new(),
                tracked: HashMap::new(),
                messages: Vec::new(),
            }),
            ctx: ctx.clone(),
            dir,
        });
        if let Some((url, user)) = saved {
            let password = settings::password(&url, &user).unwrap_or_default();
            shared.lock().form = Form { url: url.clone(), user: user.clone(), ..Form::default() };
            // With the password remembered the app signs in by itself; without, the Open window asks.
            if !password.is_empty() {
                connect(&shared, Conn { base: url, user, password }, false);
            }
        }
        start_heartbeat(&shared);
        Rc::new(Self { shared, native_open, native_save, native_wait: RefCell::new(None), last_watch: RefCell::new(None) })
    }

    /// Is a file server in use?
    pub fn active(&self) -> bool {
        self.shared.lock().active
    }

    // ---- the hooks of `Services` -----------------------------------------------------------

    /// File > Open for a design: the Open window answers it. Other kinds of files (STEP, meshes…)
    /// come from this computer as they always did.
    pub fn pick_open(&self, first: &[&str]) -> FilePickerFuture {
        if !opens_design(first) {
            return (self.native_open)(first);
        }
        let answer = Answer::default();
        {
            let mut s = self.shared.lock();
            if let Some(old) = s.picking.replace(answer.clone()) {
                old.resolve(None);
            }
            s.panel = true;
            s.unlocking = None;
        }
        refresh(&self.shared);
        self.shared.ctx.request_repaint();
        Box::pin(answer)
    }

    /// Save As for a design: with a file server, ask for its name there.
    pub fn pick_save(&self, name: &str, exts: &[&str]) -> FilePickerFuture {
        if !self.active() || !exts.contains(&solidcraft_engine::io::DESIGN_EXT) {
            return (self.native_save)(name, exts);
        }
        let answer = Answer::default();
        {
            let mut s = self.shared.lock();
            let ask = SaveAsk { name: name.to_string(), error: None, focus: true, answer: answer.clone() };
            if let Some(old) = s.save_ask.replace(ask) {
                old.answer.resolve(None);
            }
        }
        refresh(&self.shared);
        self.shared.ctx.request_repaint();
        Box::pin(answer)
    }

    /// Can the start page's design (or a recent one) be brought back by fetching it?
    pub fn can_restore(&self, path: &str) -> bool {
        self.active() && parse_start_path(path).is_some()
    }

    /// A design on the server chosen on the start page: fetch it and open it.
    pub fn restore_file(&self, path: &str) {
        if let Some(name) = parse_start_path(path) {
            start_open(&self.shared, name.to_string(), None);
        }
    }

    /// The designs on the server for the start page (`Services::server_designs`), which shows
    /// them in place of the built-in samples. The page asks for them while it is on screen, so
    /// this is also where the list is kept fresh.
    pub fn home_designs(&self) -> Option<ServerDesigns> {
        if !self.active() {
            return None;
        }
        let stale = self.shared.lock().listed_at.is_none_or(|t| t.elapsed() > LIST_FRESH);
        if stale {
            refresh(&self.shared);
        }
        // egui draws when something happens, and the start page just sits there: without asking
        // for the next frame, a design somebody opened or saved would never show up by itself.
        self.shared.ctx.request_repaint_after(LIST_FRESH);
        let s = self.shared.lock();
        let now_ms = now_ms();
        let designs = s
            .entries
            .iter()
            .map(|e| {
                let here = s.tracked.get(&e.name).is_some_and(|t| t.token.is_some());
                let lock = e.lock_owner.as_ref().map(|owner| lock_text(owner, e.mine, here));
                ServerDesign {
                    path: start_path(&e.name),
                    detail: detail_text(lock.as_deref(), e.size, &names::ago_text(now_ms, e.modified_ms)),
                    picture: None,
                }
            })
            .collect();
        let empty = match (&s.list_error, s.listing) {
            (Some(e), _) => format!("Die Liste kam nicht: {e}"),
            (None, true) => "Lese die Liste vom Server …".to_string(),
            (None, false) => "Noch keine Designs auf dem Server. Mit „Speichern unter“ legst du eines an.".to_string(),
        };
        Some(ServerDesigns { title: "Designs auf dem Server".into(), empty, recent_label: "Auf dem Server".into(), prefix: PREFIX.into(), designs })
    }

    // ---- the app's frames -------------------------------------------------------------------

    /// Once a frame, before the window is drawn: show what the workers found, open fetched designs,
    /// upload saved ones and let go of the designs no tab shows any more.
    pub fn tick(&self, app: &mut SolveApp) {
        for (text, error) in std::mem::take(&mut self.shared.lock().messages) {
            app.set_status(text, error);
        }
        self.poll_native();
        let idle = !app.file_picker_busy() && app.file_load.idle();
        let (opened, mut copies) = {
            let mut s = self.shared.lock();
            let opened = if idle { std::mem::take(&mut s.opened) } else { Vec::new() };
            (opened, std::mem::take(&mut s.copies))
        };
        for Opened { path, copy } in opened {
            match documents::find_path(app, &path) {
                Some(i) => {
                    documents::switch(app, i);
                    app.home.open = false;
                }
                None => {
                    app.open_path_async(&path);
                    if let Some(notice) = copy {
                        copies.push((path, notice, Instant::now()));
                    }
                }
            }
        }
        // A copy of a design somebody has keeps no file of its own: Save asks for a name on the
        // server instead of writing over their work.
        copies.retain(|(path, notice, since)| {
            if app.file_load.idle() && app.session.path.as_deref() == Some(path.as_str()) {
                app.session.path = None;
                let title = format!("{} (Kopie)", app.session.doc.name);
                app.session.doc_mut().name = title;
                app.session.mark_saved();
                app.home.recent.retain(|r| r != path);
                app.home.opened_at.remove(path);
                app.set_status(notice.clone(), true);
                return false;
            }
            since.elapsed() < Duration::from_secs(120)
        });
        if !copies.is_empty() {
            self.shared.lock().copies.extend(copies);
        }
        self.watch(app);
    }

    /// An answer from this computer's picker, for an Open the Open window passed on.
    fn poll_native(&self) {
        let mut slot = self.native_wait.borrow_mut();
        let Some((future, answer)) = slot.as_mut() else { return };
        let mut cx = Context::from_waker(Waker::noop());
        if let Poll::Ready(v) = future.as_mut().poll(&mut cx) {
            answer.resolve(v);
            *slot = None;
        }
    }

    /// Hand an Open to this computer's picker (in the click, as the dialogs want it).
    pub(crate) fn open_from_computer(&self) {
        let waiting = {
            let mut s = self.shared.lock();
            s.panel = false;
            s.picking.take()
        };
        let Some(answer) = waiting else { return };
        let future = (self.native_open)(&[]);
        *self.native_wait.borrow_mut() = Some((future, answer));
    }

    /// Save As asked to the picker of this computer instead.
    pub(crate) fn save_on_computer(&self, name: &str) {
        let ask = self.shared.lock().save_ask.take();
        let Some(ask) = ask else { return };
        let future = (self.native_save)(name, &[solidcraft_engine::io::DESIGN_EXT, "json"]);
        *self.native_wait.borrow_mut() = Some((future, ask.answer));
    }

    /// Look at the cache files: a saved design is uploaded, a design no tab shows is let go of.
    fn watch(&self, app: &SolveApp) {
        {
            let mut last = self.last_watch.borrow_mut();
            if last.is_some_and(|t| t.elapsed() < WATCH) {
                return;
            }
            *last = Some(Instant::now());
        }
        let mut uploads: Vec<(String, PathBuf, Sig)> = Vec::new();
        let mut releases: Vec<(String, Option<String>)> = Vec::new();
        {
            let mut s = self.shared.lock();
            for (name, t) in s.tracked.iter_mut() {
                if t.busy {
                    continue;
                }
                let shown = documents::find_path(app, &t.path.to_string_lossy()).is_some();
                t.seen_open |= shown;
                let current = sig(&t.path);
                if let Some(cur) = current.filter(|c| t.uploaded != Some(*c) && t.failed != Some(*c) && !t.lost) {
                    if t.pending == Some(cur) {
                        t.pending = None;
                        t.busy = true;
                        uploads.push((name.clone(), t.path.clone(), cur));
                    } else {
                        t.pending = Some(cur);
                    }
                    continue;
                }
                // Nobody shows it any more, and nothing waits to be uploaded.
                if !shown && (t.seen_open || t.since.elapsed() > OPEN_GRACE) && t.pending.is_none() {
                    t.busy = true;
                    releases.push((name.clone(), t.token.take()));
                }
            }
        }
        for (name, path, sig) in uploads {
            let shared = self.shared.clone();
            let _ = std::thread::Builder::new().name("cloud-upload".into()).spawn(move || upload(&shared, &name, &path, sig));
        }
        for (name, token) in releases {
            let shared = self.shared.clone();
            let _ = std::thread::Builder::new().name("cloud-release".into()).spawn(move || {
                if let (Some(token), Some(conn)) = (token, shared.conn())
                    && let Err(e) = conn.call("DELETE", &lock_path(&name), &[("X-Lock-Token", token.as_str())], None)
                {
                    log::info!("cloud: lock of {name} not released: {e}");
                }
                shared.lock().tracked.remove(&name);
            });
        }
        // The watch needs frames when the app sits idle.
        if !self.shared.lock().tracked.is_empty() {
            self.shared.ctx.request_repaint_after(WATCH);
        }
    }

    /// The app ends: let go of every lock now (after a crash they run out by themselves).
    pub fn shutdown(&self) {
        let (conn, held): (Option<Conn>, Vec<(String, String)>) = {
            let mut s = self.shared.lock();
            let held = s.tracked.iter_mut().filter_map(|(n, t)| t.token.take().map(|tok| (n.clone(), tok))).collect();
            (s.conn.clone(), held)
        };
        let Some(conn) = conn else { return };
        for (name, token) in held {
            let _ = conn.call("DELETE", &lock_path(&name), &[("X-Lock-Token", token.as_str())], None);
        }
    }

    /// Register the file of a design saved under a new name on the server (Save As): once the
    /// save has written it, it is uploaded as a new design and locked.
    pub(crate) fn expect_new(&self, name: &str) -> Option<String> {
        let path = self.shared.cache_path("designs", name)?;
        if let Some(dir) = path.parent()
            && let Err(e) = std::fs::create_dir_all(dir)
        {
            self.shared.say(format!("Der Ordner für Server-Designs ließ sich nicht anlegen: {e}"), true);
            return None;
        }
        // A file of an earlier design with this name is not the new design.
        let _ = std::fs::remove_file(&path);
        let text = path.to_string_lossy().into_owned();
        self.shared.lock().tracked.insert(
            name.to_string(),
            Tracked {
                path,
                etag: None,
                token: None,
                uploaded: None,
                pending: None,
                failed: None,
                lost: false,
                busy: false,
                since: Instant::now(),
                seen_open: false,
            },
        );
        // The watch runs in frames: an idle app has to draw some to see the saved file.
        self.shared.ctx.request_repaint_after(WATCH);
        Some(text)
    }
}

/// Does an Open for these extensions (the file type the command reads) ask for a design?
fn opens_design(exts: &[&str]) -> bool {
    exts.is_empty() || exts.iter().any(|e| ["solidcraft", "solvecraft", "json"].iter().any(|d| e.eq_ignore_ascii_case(d)))
}

fn now_ms() -> f64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0.0, |d| d.as_millis() as f64)
}

// ---- worker threads ------------------------------------------------------------------------

/// Sign in: check the address and login in the background. With `remember`, a good login is kept
/// (address and user in the settings folder, the password in the keychain).
pub(crate) fn connect(shared: &Arc<Shared>, conn: Conn, remember: bool) {
    {
        let mut s = shared.lock();
        s.form.checking = true;
        s.form.error = None;
    }
    let shared = shared.clone();
    let _ = std::thread::Builder::new().name("cloud-connect".into()).spawn(move || {
        let result = conn.check();
        {
            let mut s = shared.lock();
            s.form.checking = false;
            match &result {
                Ok(()) => {
                    s.conn = Some(conn.clone());
                    s.active = true;
                    s.form = Form { url: conn.base.clone(), user: conn.user.clone(), ..Form::default() };
                }
                Err(e) => {
                    s.active = false;
                    s.form.error = Some(e.clone());
                }
            }
        }
        if result.is_ok() {
            if remember
                && let Some(dir) = shared.dir.as_deref()
                && let Err(e) = settings::save(dir, &conn.base, &conn.user, &conn.password)
            {
                shared.say(format!("Die Anmeldung konnte nicht gemerkt werden: {e}"), true);
            }
            refresh(&shared);
        }
        shared.ctx.request_repaint();
    });
}

/// Sign out: forget the login (the password is deleted from the keychain).
pub(crate) fn disconnect(shared: &Arc<Shared>) {
    let (url, user) = {
        let mut s = shared.lock();
        let who = s.conn.take().map(|c| (c.base, c.user));
        s.active = false;
        s.entries.clear();
        s.listed_at = None;
        who.unwrap_or_default()
    };
    if !url.is_empty() {
        settings::forget(&url, &user);
    }
}

/// Fetch the list of designs.
pub(crate) fn refresh(shared: &Arc<Shared>) {
    let Some(conn) = shared.conn() else { return };
    {
        let mut s = shared.lock();
        if s.listing {
            return;
        }
        s.listing = true;
        s.listed_at = Some(Instant::now());
    }
    let shared = shared.clone();
    let _ = std::thread::Builder::new().name("cloud-list".into()).spawn(move || {
        let result = conn.call("GET", &list_path(), &[], None).map_err(|e| format!("Server nicht erreichbar: {e}")).and_then(|r| match r.status {
            200 => Ok(parse_list(&r.json())),
            401 | 403 => Err("Der Server lehnt die Anmeldung ab.".to_string()),
            s => Err(format!("Der Server antwortet {s}{}", r.error().map(|e| format!(": {e}")).unwrap_or_default())),
        });
        let mut s = shared.lock();
        match result {
            Ok(entries) => {
                s.entries = entries;
                s.list_error = None;
            }
            Err(e) => s.list_error = Some(e),
        }
        s.listing = false;
        drop(s);
        shared.ctx.request_repaint();
    });
}

fn parse_list(v: &Value) -> Vec<Entry> {
    let mut entries: Vec<Entry> = v["designs"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .take(10_000)
        .filter_map(|d| {
            Some(Entry {
                name: d["name"].as_str()?.to_string(),
                size: d["size"].as_u64().unwrap_or(0),
                modified_ms: d["modified_ms"].as_f64().unwrap_or(0.0),
                lock_owner: d["lock"]["owner"].as_str().map(str::to_string),
                mine: d["lock"]["mine"].as_bool() == Some(true),
            })
        })
        .collect();
    entries.sort_by_key(|e| e.name.to_lowercase());
    entries
}

/// Fetch a design and open it: with `answer`, an Open waits for the file's path; without, the
/// frame opens it (the start page).
pub(crate) fn start_open(shared: &Arc<Shared>, name: String, answer: Option<Answer>) {
    let Some(conn) = shared.conn() else { return };
    {
        let mut s = shared.lock();
        if s.loading.is_some() {
            return;
        }
        s.loading = Some(name.clone());
    }
    let shared = shared.clone();
    let _ = std::thread::Builder::new().name("cloud-open".into()).spawn(move || {
        let result = fetch(&shared, &conn, &name);
        {
            let mut s = shared.lock();
            s.loading = None;
            match result {
                Ok(opened) => {
                    s.panel = false;
                    let waiting = answer.or_else(|| s.picking.take());
                    s.picking = None;
                    match waiting {
                        Some(a) if opened.copy.is_none() => a.resolve(Some(opened.path)),
                        Some(a) => {
                            // A copy has no file of its own: the frame opens it and takes the file away.
                            a.resolve(None);
                            s.opened.push(opened);
                        }
                        None => s.opened.push(opened),
                    }
                }
                Err(e) => s.messages.push((e, true)),
            }
        }
        refresh(&shared);
        shared.ctx.request_repaint();
    });
}

/// Take the lock on `name`. `Ok(None)`: it is this app's now, with the token. `Ok(Some((owner, mine)))`: somebody has it.
fn take_lock(conn: &Conn, name: &str) -> Result<Result<String, (String, bool)>, String> {
    let r = conn.call("POST", &lock_path(name), &[], None).map_err(|e| format!("Server nicht erreichbar: {e}"))?;
    let v = r.json();
    match r.status {
        200 => match v["token"].as_str() {
            Some(token) => Ok(Ok(token.to_string())),
            None => Err(format!("„{name}“ nicht gesperrt: Der Server nennt kein Token.")),
        },
        409 => Ok(Err((v["owner"].as_str().unwrap_or("jemand anderem").to_string(), v["mine"].as_bool() == Some(true)))),
        s => Err(format!("„{name}“ nicht gesperrt: Der Server antwortet {s}{}", r.error().map(|e| format!(" ({e})")).unwrap_or_default())),
    }
}

/// Download a design into the cache folder with its lock taken. Does somebody have the design, a
/// copy is made that is never written back.
fn fetch(shared: &Arc<Shared>, conn: &Conn, name: &str) -> Result<Opened, String> {
    let nowhere = || "Es gibt keinen Ordner für Server-Designs auf diesem Computer.".to_string();
    // This app holds it already (it is open in a tab): that file, as it is.
    if let Some(t) = shared.lock().tracked.get(name).filter(|t| t.token.is_some()) {
        return Ok(Opened { path: t.path.to_string_lossy().into_owned(), copy: None });
    }
    let held = take_lock(conn, name)?;
    let fetched = (|| {
        let r = conn.call("GET", &design_path(name), &[], None).map_err(|e| format!("Server nicht erreichbar: {e}"))?;
        match r.status {
            200 => Ok(r),
            404 => Err(format!("„{name}“ gibt es auf dem Server nicht mehr.")),
            s => Err(format!("„{name}“ nicht geladen: Der Server antwortet {s}{}", r.error().map(|e| format!(" ({e})")).unwrap_or_default())),
        }
    })();
    let release = |token: &Result<String, (String, bool)>| {
        if let Ok(token) = token {
            let _ = conn.call("DELETE", &lock_path(name), &[("X-Lock-Token", token.as_str())], None);
        }
    };
    let reply = match fetched {
        Ok(r) => r,
        Err(e) => {
            release(&held);
            return Err(e);
        }
    };
    let (sub, tracked_token) = match &held {
        Ok(token) => ("designs", Some(token.clone())),
        Err(_) => ("copies", None),
    };
    let Some(path) = shared.cache_path(sub, name) else {
        release(&held);
        return Err(nowhere());
    };
    if let Err(e) = write_file(&path, &reply.body) {
        release(&held);
        return Err(format!("„{name}“ nicht gespeichert: {e}"));
    }
    let text = path.to_string_lossy().into_owned();
    match held {
        Ok(_) => {
            let etag = reply.etag.as_deref().map(normalize_etag);
            let uploaded = sig(&path);
            shared.lock().tracked.insert(
                name.to_string(),
                Tracked {
                    path,
                    etag,
                    token: tracked_token,
                    uploaded,
                    pending: None,
                    failed: None,
                    lost: false,
                    busy: false,
                    since: Instant::now(),
                    seen_open: false,
                },
            );
            Ok(Opened { path: text, copy: None })
        }
        Err((owner, mine)) => Ok(Opened { path: text, copy: Some(copy_notice(name, &owner, mine)) }),
    }
}

/// Write `bytes` to `path` so that nobody sees half a file.
fn write_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let dir = path.parent().ok_or("kein Ordner")?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let part = path.with_extension("part");
    std::fs::write(&part, bytes).map_err(|e| e.to_string())?;
    std::fs::rename(&part, path).map_err(|e| {
        let _ = std::fs::remove_file(&part);
        e.to_string()
    })
}

/// Save a design on the server: over the version it was loaded as (`If-Match`), or as a new file
/// (no ETag: the server refuses to overwrite one that is there).
fn upload(shared: &Arc<Shared>, name: &str, path: &Path, seen: Sig) {
    let outcome = upload_inner(shared, name, path, seen);
    let mut s = shared.lock();
    if let Some(t) = s.tracked.get_mut(name) {
        t.busy = false;
        if outcome.is_err() {
            t.failed = Some(seen);
        }
    }
    match outcome {
        Ok(()) => s.messages.push((format!("Auf dem Server gespeichert: {name}"), false)),
        Err(e) => s.messages.push((e, true)),
    }
    drop(s);
    refresh(shared);
    shared.ctx.request_repaint();
}

fn upload_inner(shared: &Arc<Shared>, name: &str, path: &Path, seen: Sig) -> Result<(), String> {
    let conn = shared.conn().ok_or("Nicht am Server angemeldet.")?;
    if seen.1 > MAX_UPLOAD {
        return Err(save_failure(413, name, None, None));
    }
    let bytes = std::fs::read(path).map_err(|e| format!("„{name}“ nicht hochgeladen: {e}"))?;
    let (etag, token) = {
        let s = shared.lock();
        let t = s.tracked.get(name).ok_or("Das Design gehört nicht mehr zum Server.")?;
        (t.etag.clone(), t.token.clone())
    };
    let mut headers = vec![("Content-Type", "application/octet-stream")];
    if let Some(tag) = &etag {
        headers.push(("If-Match", tag.as_str()));
    }
    // Our own lock has to be shown too: the server takes a write only with its token.
    if let Some(t) = &token {
        headers.push(("X-Lock-Token", t.as_str()));
    }
    let r = conn
        .call("PUT", &design_path(name), &headers, Some(&bytes))
        .map_err(|e| format!("„{name}“ nicht hochgeladen: Server nicht erreichbar ({e})."))?;
    let v = r.json();
    match r.status {
        200 | 201 => {
            // The server's ETag in its JSON: a proxy may rewrite the header's.
            let tag = v["etag"].as_str().map(str::to_string).or(r.etag).map(|t| normalize_etag(&t));
            {
                let mut s = shared.lock();
                if let Some(t) = s.tracked.get_mut(name) {
                    t.etag = tag;
                    t.uploaded = Some(seen);
                }
            }
            // Saved under a new name (Save As): the design is open here now, so take its lock.
            if token.is_none() {
                match take_lock(&conn, name) {
                    Ok(Ok(token)) => {
                        if let Some(t) = shared.lock().tracked.get_mut(name) {
                            t.token = Some(token);
                        }
                    }
                    Ok(Err(_)) => {}
                    Err(e) => log::info!("cloud: {e}"),
                }
            }
            Ok(())
        }
        s => Err(save_failure(s, name, r.error().as_deref(), v["owner"].as_str())),
    }
}

/// Renew every lock this app holds. One the server does not know any more (it ran out while the
/// notebook slept, or the server was restarted) is taken again; where that fails the design
/// belongs to someone else now and the user is told, so the work can be saved under another name.
fn start_heartbeat(shared: &Arc<Shared>) {
    let shared = shared.clone();
    let _ = std::thread::Builder::new().name("cloud-heartbeat".into()).spawn(move || {
        loop {
            std::thread::sleep(HEARTBEAT);
            let Some(conn) = shared.conn() else { continue };
            let held: Vec<(String, String)> =
                shared.lock().tracked.iter().filter(|(_, t)| !t.busy).filter_map(|(n, t)| t.token.clone().map(|tok| (n.clone(), tok))).collect();
            for (name, token) in held {
                match conn.call("PUT", &lock_path(&name), &[("X-Lock-Token", token.as_str())], None) {
                    Ok(r) if r.status != 409 => continue,
                    Err(e) => {
                        log::info!("cloud: lock not renewed ({name}): {e}");
                        continue;
                    }
                    Ok(_) => {}
                }
                match take_lock(&conn, &name) {
                    Ok(Ok(new_token)) => {
                        if let Some(t) = shared.lock().tracked.get_mut(&name) {
                            t.token = Some(new_token);
                        }
                    }
                    Ok(Err((owner, _))) => {
                        if let Some(t) = shared.lock().tracked.get_mut(&name) {
                            t.token = None;
                            t.lost = true;
                        }
                        shared.say(lock_lost(&name, &owner), true);
                        refresh(&shared);
                    }
                    Err(e) => log::info!("cloud: lock not taken again ({name}): {e}"),
                }
            }
        }
    });
}

/// Lift somebody else's lock (the emergency brake for a notebook that was closed).
pub(crate) fn force_unlock(shared: &Arc<Shared>, name: String) {
    let Some(conn) = shared.conn() else { return };
    let shared = shared.clone();
    let _ = std::thread::Builder::new().name("cloud-unlock".into()).spawn(move || {
        let lifted = match conn.call("DELETE", &format!("{}?force=1", lock_path(&name)), &[], None) {
            Ok(r) if r.status == 204 || r.status == 200 => Ok(()),
            Ok(r) => Err(format!("Der Server antwortet {}", r.status)),
            Err(e) => Err(format!("Server nicht erreichbar ({e})")),
        };
        match lifted {
            Ok(()) => shared.say(format!("Die Sperre für „{name}“ ist aufgehoben."), false),
            Err(e) => shared.say(format!("Die Sperre für „{name}“ blieb: {e}."), true),
        }
        refresh(&shared);
    });
}

/// The server's list as the Open window shows it: sizes in words.
pub(crate) fn size_label(bytes: u64) -> String {
    size_text(bytes)
}

/// Where the address and user are kept, and the password: the system keychain.
mod settings {
    use std::path::Path;

    const FILE: &str = "cloud.json";
    const SERVICE: &str = "SolidCraft Cloud";

    pub fn load(dir: &Path) -> Option<(String, String)> {
        let text = std::fs::read_to_string(dir.join(FILE)).ok()?;
        let v: serde_json::Value = serde_json::from_str(&text).ok()?;
        let url = super::names::clean_url(v["url"].as_str()?)?;
        let user = v["user"].as_str()?.to_string();
        (!user.is_empty()).then_some((url, user))
    }

    fn entry(url: &str, user: &str) -> Result<keyring::Entry, String> {
        keyring::Entry::new(SERVICE, &format!("{user}@{url}")).map_err(|e| e.to_string())
    }

    pub fn password(url: &str, user: &str) -> Option<String> {
        entry(url, user).ok()?.get_password().ok()
    }

    pub fn save(dir: &Path, url: &str, user: &str, password: &str) -> Result<(), String> {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        let text = serde_json::json!({ "url": url, "user": user }).to_string();
        std::fs::write(dir.join(FILE), text).map_err(|e| e.to_string())?;
        entry(url, user)?.set_password(password).map_err(|e| format!("Passwort nicht im Schlüsselbund gespeichert: {e}"))
    }

    pub fn forget(url: &str, user: &str) {
        if let Ok(e) = entry(url, user) {
            let _ = e.delete_credential();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_open_for_a_design_goes_to_the_server_and_other_files_do_not() {
        assert!(opens_design(&[]));
        assert!(opens_design(&["solidcraft"]));
        assert!(opens_design(&["SolidCraft", "json"]));
        assert!(!opens_design(&["step", "stp"]));
        assert!(!opens_design(&["stl"]));
    }

    #[test]
    fn the_list_is_read_sorted_and_bounded() {
        let v = serde_json::json!({"designs": [
            {"name": "b.solidcraft", "size": 5, "modified_ms": 1000.0, "lock": null},
            {"name": "A.solidcraft", "size": 7, "modified_ms": 2000.0, "lock": {"owner": "bob", "mine": false, "expires_in_s": 30}},
            {"size": 1}
        ]});
        let entries = parse_list(&v);
        assert_eq!(entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["A.solidcraft", "b.solidcraft"]);
        assert_eq!(entries.first().and_then(|e| e.lock_owner.as_deref()), Some("bob"));
        assert!(entries.first().is_some_and(|e| !e.mine));
        assert!(parse_list(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn an_answer_is_given_once_and_taken_by_the_waiting_future() {
        let a = Answer::default();
        let mut waiting = Box::pin(a.clone());
        let mut cx = Context::from_waker(Waker::noop());
        assert_eq!(waiting.as_mut().poll(&mut cx), Poll::Pending);
        a.resolve(Some("/x/a.solidcraft".into()));
        a.resolve(None);
        assert_eq!(waiting.as_mut().poll(&mut cx), Poll::Ready(Some("/x/a.solidcraft".into())));
    }

    #[test]
    fn a_file_is_written_whole_or_not_at_all() {
        let dir = std::env::temp_dir().join(format!("solidcraft-cloud-test-{}", std::process::id()));
        let path = dir.join("designs").join("A.solidcraft");
        assert!(write_file(&path, b"one").is_ok());
        assert!(write_file(&path, b"two").is_ok());
        assert_eq!(std::fs::read(&path).ok().as_deref(), Some(&b"two"[..]));
        assert!(!path.with_extension("part").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
