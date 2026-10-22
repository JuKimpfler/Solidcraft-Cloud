//! Einstellungen aus Umgebungsvariablen.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

pub const ENV_DATA: &str = "SOLIDCRAFT_CLOUD_DATA";
pub const ENV_BIND: &str = "SOLIDCRAFT_CLOUD_BIND";
pub const ENV_MAX_BYTES: &str = "SOLIDCRAFT_CLOUD_MAX_BYTES";
pub const ENV_LOCK_TTL: &str = "SOLIDCRAFT_CLOUD_LOCK_TTL_SECS";

/// Wie `MAX_DESIGN_BYTES` in SolidCraft (`crates/io`): größere Designs öffnet die App ohnehin nicht.
pub const DEFAULT_MAX_BYTES: u64 = 256 << 20;
pub const DEFAULT_BIND: &str = "127.0.0.1:8081";
pub const DEFAULT_LOCK_TTL_SECS: u64 = 60;

#[derive(Clone, Debug)]
pub struct Config {
    /// Ordner mit den Designs (z. B. ein in Nextcloud eingebundener lokaler Speicher).
    pub data_dir: PathBuf,
    /// Adresse, auf der der Server lauscht. Standard: nur lokal, hinter dem Reverse Proxy.
    pub bind: SocketAddr,
    /// Größte erlaubte Datei in Bytes.
    pub max_bytes: u64,
    /// Wie lange eine Sperre ohne Heartbeat gilt.
    pub lock_ttl: Duration,
}

impl Config {
    pub fn from_env() -> Result<Config, String> {
        Config::from_lookup(|key| std::env::var(key).ok())
    }

    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Config, String> {
        let data_dir = get(ENV_DATA).filter(|v| !v.trim().is_empty()).ok_or_else(|| format!("{ENV_DATA} fehlt (Ordner mit den Designs)"))?;
        let bind = get(ENV_BIND).unwrap_or_else(|| DEFAULT_BIND.to_string());
        let bind: SocketAddr = bind.trim().parse().map_err(|_| format!("{ENV_BIND}: \"{bind}\" ist keine Adresse der Form ip:port"))?;
        let max_bytes = number(&get, ENV_MAX_BYTES, DEFAULT_MAX_BYTES, 1, 4 << 30)?;
        let ttl = number(&get, ENV_LOCK_TTL, DEFAULT_LOCK_TTL_SECS, 5, 3600)?;
        Ok(Config { data_dir: PathBuf::from(data_dir), bind, max_bytes, lock_ttl: Duration::from_secs(ttl) })
    }
}

fn number(get: &impl Fn(&str) -> Option<String>, key: &str, default: u64, min: u64, max: u64) -> Result<u64, String> {
    let Some(raw) = get(key) else { return Ok(default) };
    let value: u64 = raw.trim().parse().map_err(|_| format!("{key}: \"{raw}\" ist keine Zahl"))?;
    if (min..=max).contains(&value) {
        Ok(value)
    } else {
        Err(format!("{key}: {value} liegt nicht zwischen {min} und {max}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |k| map.get(k).cloned()
    }

    #[test]
    fn defaults_apply() {
        let c = Config::from_lookup(lookup(&[(ENV_DATA, "/srv/designs")])).unwrap();
        assert_eq!(c.data_dir, PathBuf::from("/srv/designs"));
        assert_eq!(c.bind.to_string(), DEFAULT_BIND);
        assert!(c.bind.ip().is_loopback());
        assert_eq!(c.max_bytes, DEFAULT_MAX_BYTES);
        assert_eq!(c.lock_ttl, Duration::from_secs(60));
    }

    #[test]
    fn data_dir_is_required() {
        assert!(Config::from_lookup(lookup(&[])).is_err());
        assert!(Config::from_lookup(lookup(&[(ENV_DATA, "  ")])).is_err());
    }

    #[test]
    fn bad_values_are_refused() {
        assert!(Config::from_lookup(lookup(&[(ENV_DATA, "/d"), (ENV_BIND, "nicht-eine-adresse")])).is_err());
        assert!(Config::from_lookup(lookup(&[(ENV_DATA, "/d"), (ENV_LOCK_TTL, "1")])).is_err());
        assert!(Config::from_lookup(lookup(&[(ENV_DATA, "/d"), (ENV_MAX_BYTES, "viel")])).is_err());
    }

    #[test]
    fn overrides_apply() {
        let c = Config::from_lookup(lookup(&[(ENV_DATA, "/d"), (ENV_BIND, "0.0.0.0:9000"), (ENV_MAX_BYTES, "1000"), (ENV_LOCK_TTL, "30")])).unwrap();
        assert_eq!(c.bind.port(), 9000);
        assert_eq!(c.max_bytes, 1000);
        assert_eq!(c.lock_ttl, Duration::from_secs(30));
    }
}
