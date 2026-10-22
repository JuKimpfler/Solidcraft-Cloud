//! Startet den Datei-Server. Einstellungen kommen aus Umgebungsvariablen (siehe `config.rs`).
#![forbid(unsafe_code)]

use std::io::Read;
use std::process::ExitCode;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use solidcraft_cloud_server::api::{self, App, Body, Request};
use solidcraft_cloud_server::config::Config;
use solidcraft_cloud_server::store::Store;
use tiny_http::{Header, Response, StatusCode};

/// Gleichzeitig bearbeitete Anfragen. Für ein Team von drei Leuten großzügig; schützt davor, dass
/// ein Fehler (oder ein offener Port) den Pi mit Threads überschwemmt.
const MAX_IN_FLIGHT: usize = 64;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("solidcraft-cloud-server: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let cfg = Config::from_env()?;
    let store = Store::open(&cfg.data_dir).map_err(|e| format!("Ordner {} nicht nutzbar: {e}", cfg.data_dir.display()))?;
    let server = tiny_http::Server::http(cfg.bind).map_err(|e| format!("kann {} nicht öffnen: {e}", cfg.bind))?;
    eprintln!("solidcraft-cloud-server: lauscht auf http://{}, Designs in {}", cfg.bind, cfg.data_dir.display());
    if !cfg.bind.ip().is_loopback() {
        eprintln!(
            "WARNUNG: Der Server vertraut dem Header X-Remote-User. Er darf nur hinter dem Reverse Proxy erreichbar sein, nie direkt aus dem Netz."
        );
    }

    let app = Arc::new(App::new(cfg, store));
    let in_flight = Arc::new(AtomicUsize::new(0));
    for request in server.incoming_requests() {
        if in_flight.fetch_add(1, Ordering::SeqCst) >= MAX_IN_FLIGHT {
            in_flight.fetch_sub(1, Ordering::SeqCst);
            respond_plain(request, 503);
            continue;
        }
        let (app, in_flight) = (Arc::clone(&app), Arc::clone(&in_flight));
        std::thread::spawn(move || {
            serve(&app, request);
            in_flight.fetch_sub(1, Ordering::SeqCst);
        });
    }
    Ok(())
}

fn respond_plain(request: tiny_http::Request, status: u16) {
    if let Err(e) = request.respond(Response::empty(StatusCode(status))) {
        eprintln!("Antwort nicht gesendet: {e}");
    }
}

fn http_header(name: &str, value: &str) -> Option<Header> {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).ok()
}

fn serve(app: &App, mut request: tiny_http::Request) {
    let method = request.method().as_str().to_string();
    let url = request.url().to_string();
    let headers: Vec<(String, String)> = request.headers().iter().map(|h| (h.field.to_string(), h.value.to_string())).collect();
    let content_length = request.body_length().and_then(|n| u64::try_from(n).ok());
    let user = headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("x-remote-user")).map_or("-", |(_, v)| v.as_str()).to_string();

    let started = Instant::now();
    let reply = {
        let body: &mut dyn Read = request.as_reader();
        api::handle(app, Request { method: &method, url: &url, headers: &headers, content_length, body }, started)
    };
    eprintln!("{user} {method} {} -> {}", url.split('?').next().unwrap_or(""), reply.status);

    let mut extra: Vec<Header> = reply.headers.iter().filter_map(|(k, v)| http_header(k, v)).collect();
    let status = StatusCode(reply.status);
    let sent = match reply.body {
        Body::Empty => request.respond(Response::new(status, extra, std::io::empty(), Some(0), None)),
        Body::Json(value) => {
            let bytes = value.to_string().into_bytes();
            extra.extend(http_header("Content-Type", "application/json; charset=utf-8"));
            extra.extend(http_header("Cache-Control", "no-store"));
            let len = bytes.len();
            request.respond(Response::new(status, extra, std::io::Cursor::new(bytes), Some(len), None))
        }
        Body::File(file, len) => request.respond(Response::new(status, extra, file, usize::try_from(len).ok(), None)),
    };
    if let Err(e) = sent {
        eprintln!("Antwort nicht gesendet: {e}");
    }
}
