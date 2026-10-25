//! SolidCraft_Cloud, desktop app: the calls to the file server. Blocking (ureq), so they run on
//! worker threads, never on the window thread.

use std::time::Duration;

use serde_json::Value;

use super::names;

/// The largest answer taken: the server's own limit for a design (`SOLIDCRAFT_CLOUD_MAX_BYTES`).
const MAX_BODY: u64 = 256 << 20;

/// Where the file server is and who asks.
#[derive(Clone, Debug)]
pub struct Conn {
    /// `https://cad.example.eu`, no trailing slash (see [`names::clean_url`]).
    pub base: String,
    pub user: String,
    pub password: String,
}

/// A server's answer.
#[derive(Debug)]
pub struct Reply {
    pub status: u16,
    pub etag: Option<String>,
    pub body: Vec<u8>,
}

impl Reply {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_default()
    }

    /// The server's own words for what went wrong (`{"error": …}`).
    pub fn error(&self) -> Option<String> {
        self.json()["error"].as_str().map(str::to_string)
    }
}

fn agent(timeout: Duration) -> ureq::Agent {
    let tls = ureq::tls::TlsConfig::builder()
        .root_certs(ureq::tls::RootCerts::PlatformVerifier)
        .unversioned_rustls_crypto_provider(std::sync::Arc::new(rustls::crypto::ring::default_provider()))
        .build();
    ureq::Agent::config_builder()
        .tls_config(tls)
        // A 4xx or 5xx is an answer to read (423 says who has the design), not an error.
        .http_status_as_error(false)
        .timeout_global(Some(timeout))
        .timeout_connect(Some(Duration::from_secs(10)))
        .user_agent("SolidCraft-Cloud")
        // The password must not follow a redirect to another site.
        .max_redirects(0)
        .build()
        .into()
}

impl Conn {
    /// One request. `path` is below the site (`api/designs`); `headers` come on top of the login.
    pub fn call(&self, method: &str, path: &str, headers: &[(&str, &str)], body: Option<&[u8]>) -> Result<Reply, String> {
        let timeout = if body.is_some() || method == "GET" { Duration::from_secs(15 * 60) } else { Duration::from_secs(20) };
        let url = format!("{}/{path}", self.base);
        let mut request =
            ureq::http::Request::builder().method(method).uri(url.as_str()).header("Authorization", names::basic_auth(&self.user, &self.password));
        for (k, v) in headers {
            request = request.header(*k, *v);
        }
        let request = request.body(body.map(<[u8]>::to_vec).unwrap_or_default()).map_err(|e| e.to_string())?;
        let mut response = agent(timeout).run(request).map_err(|e| e.to_string())?;
        let status = response.status().as_u16();
        let etag = response.headers().get("etag").and_then(|v| v.to_str().ok()).map(str::to_string);
        let body = response.body_mut().with_config().limit(MAX_BODY).read_to_vec().map_err(|e| e.to_string())?;
        Ok(Reply { status, etag, body })
    }

    /// Does the site have the file server, and does it take this login? `Ok(())` or why not.
    pub fn check(&self) -> Result<(), String> {
        let r = self.call("GET", &names::health_path(), &[], None).map_err(|e| format!("Server nicht erreichbar: {e}"))?;
        match r.status {
            200 if r.json()["ok"].as_bool() == Some(true) => Ok(()),
            200 => Err("Unter dieser Adresse antwortet kein SolidCraft-Datei-Server (die Adresse zeigt wohl nur auf die Web-App).".into()),
            401 | 403 => Err("Benutzer oder Passwort stimmen nicht.".into()),
            404 => Err("Unter dieser Adresse gibt es keinen Datei-Server (/api/health fehlt).".into()),
            s => Err(format!("Der Server antwortet {s}{}", r.error().map(|e| format!(" ({e})")).unwrap_or_default())),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;

    use super::*;

    /// A one-request server: reads the request, sends it back through the channel, answers `reply`.
    fn serve(reply: &'static str) -> (String, mpsc::Receiver<(String, Vec<u8>)>) {
        let listener = TcpListener::bind("127.0.0.1:0");
        let Ok(listener) = listener else { return (String::new(), mpsc::channel().1) };
        let base = listener.local_addr().map(|a| format!("http://{a}")).unwrap_or_default();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else { return };
            let mut reader = BufReader::new(&stream);
            let mut head = String::new();
            let mut length = 0usize;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap_or(0);
                }
                head.push_str(&line);
            }
            let mut body = vec![0u8; length];
            let _ = reader.read_exact(&mut body);
            let _ = tx.send((head, body));
            let _ = stream.write_all(reply.as_bytes());
        });
        (base, rx)
    }

    fn conn(base: String) -> Conn {
        Conn { base, user: "alice".into(), password: "pw1".into() }
    }

    #[test]
    fn a_save_sends_the_login_the_etag_the_lock_token_and_the_bytes() {
        let (base, seen) = serve(
            "HTTP/1.1 201 Created\r\nETag: \"new\"\r\nContent-Type: application/json\r\nContent-Length: 14\r\nConnection: close\r\n\r\n{\"etag\":\"new\"}",
        );
        let r = conn(base).call("PUT", "api/designs/A.solidcraft", &[("If-Match", "\"old\""), ("X-Lock-Token", "tok")], Some(b"bytes"));
        let Ok(r) = r else { return assert!(r.is_ok(), "{r:?}") };
        assert_eq!(r.status, 201);
        assert_eq!(r.etag.as_deref(), Some("\"new\""));
        assert_eq!(r.json()["etag"], "new");
        let (head, body) = seen.recv().unwrap_or_default();
        let head = head.to_ascii_lowercase();
        assert!(head.starts_with("put /api/designs/a.solidcraft "), "{head}");
        assert!(head.contains("authorization: basic ywxpy2u6chcx"), "{head}");
        assert!(head.contains("if-match: \"old\""), "{head}");
        assert!(head.contains("x-lock-token: tok"), "{head}");
        assert_eq!(body, b"bytes");
    }

    #[test]
    fn a_refusal_is_an_answer_with_the_servers_words() {
        let (base, _) =
            serve("HTTP/1.1 423 Locked\r\nContent-Length: 47\r\nConnection: close\r\n\r\n{\"error\":\"Gesperrt\",\"owner\":\"bob\",\"mine\":false}");
        let r = conn(base).call("PUT", "api/designs/A.solidcraft", &[], Some(b"x"));
        let Ok(r) = r else { return assert!(r.is_ok(), "{r:?}") };
        assert_eq!(r.status, 423);
        assert_eq!(r.error().as_deref(), Some("Gesperrt"));
        assert_eq!(r.json()["owner"], "bob");
    }

    #[test]
    fn the_check_tells_a_web_app_from_a_file_server() {
        let (base, _) = serve("HTTP/1.1 200 OK\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}");
        assert_eq!(conn(base).check(), Ok(()));
        let (base, _) = serve("HTTP/1.1 200 OK\r\nContent-Length: 15\r\nConnection: close\r\n\r\n<html></html>\r\n");
        assert!(conn(base).check().is_err_and(|e| e.contains("kein SolidCraft-Datei-Server")));
        let (base, _) = serve("HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        assert!(conn(base).check().is_err_and(|e| e.contains("Passwort")));
        let (base, _) = serve("HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        assert!(conn(base).check().is_err_and(|e| e.contains("/api/health")));
    }

    #[test]
    fn an_unreachable_server_is_an_error_not_a_panic() {
        let c = conn("http://127.0.0.1:1".into());
        assert!(c.check().is_err_and(|e| e.contains("nicht erreichbar")));
    }
}
