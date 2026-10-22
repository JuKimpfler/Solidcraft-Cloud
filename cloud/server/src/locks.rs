//! Sperren auf Designs: höchstens eine Person (genauer: eine Sitzung) bearbeitet ein Design.
//!
//! Eine Sperre gilt `ttl` ab dem letzten Heartbeat. Wer sie nimmt, bekommt ein Token; nur damit
//! lässt sie sich verlängern, freigeben oder zum Schreiben nutzen. So kann dieselbe Person in
//! einem zweiten Tab das Design nicht versehentlich parallel ändern. Alle Zeiten werden von außen
//! übergeben (`now`), damit sich der Ablauf ohne Warten testen lässt.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// Abgelaufene Sperren bleiben so lange erhalten, damit ein Heartbeat nach kurzem Ausfall (Notebook
/// im Ruhezustand) sie noch verlängern kann, solange niemand sonst sie genommen hat.
const KEEP_EXPIRED: Duration = Duration::from_secs(600);

struct Lock {
    owner: String,
    token: String,
    expires: Instant,
}

impl Lock {
    fn live(&self, now: Instant) -> bool {
        self.expires > now
    }

    fn info(&self, now: Instant) -> LockInfo {
        LockInfo { owner: self.owner.clone(), expires_in: self.expires.saturating_duration_since(now) }
    }
}

/// Wer sperrt und wie lange noch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LockInfo {
    pub owner: String,
    pub expires_in: Duration,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Release {
    Released,
    /// Es gab keine (gültige) Sperre: nichts zu tun.
    NotHeld,
    /// Die Sperre gehört einer anderen Sitzung.
    Mismatch,
}

pub struct Locks {
    ttl: Duration,
    map: Mutex<HashMap<String, Lock>>,
}

impl Locks {
    pub fn new(ttl: Duration) -> Locks {
        Locks { ttl, map: Mutex::new(HashMap::new()) }
    }

    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    fn map(&self) -> MutexGuard<'_, HashMap<String, Lock>> {
        self.map.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Sperre nehmen. Ist das Design gültig gesperrt (von wem auch immer, auch von dir in einer
    /// anderen Sitzung), kommt der Besitzer zurück.
    pub fn acquire(&self, name: &str, user: &str, token: &str, now: Instant) -> Result<Duration, LockInfo> {
        let mut map = self.map();
        if let Some(lock) = map.get(name).filter(|l| l.live(now)) {
            return Err(lock.info(now));
        }
        map.insert(name.to_string(), Lock { owner: user.to_string(), token: token.to_string(), expires: now + self.ttl });
        Ok(self.ttl)
    }

    /// Heartbeat. `None`: Das Token passt zu keiner Sperre mehr (abgelaufen und weggeräumt oder
    /// von einer anderen Sitzung übernommen).
    pub fn renew(&self, name: &str, token: &str, now: Instant) -> Option<Duration> {
        let mut map = self.map();
        let lock = map.get_mut(name).filter(|l| l.token == token)?;
        lock.expires = now + self.ttl;
        Some(self.ttl)
    }

    /// Freigeben mit dem Token der Sperre.
    pub fn release(&self, name: &str, token: &str, now: Instant) -> Release {
        let mut map = self.map();
        match map.get(name) {
            None => Release::NotHeld,
            Some(lock) if lock.token == token => {
                map.remove(name);
                Release::Released
            }
            Some(lock) if lock.live(now) => Release::Mismatch,
            Some(_) => Release::NotHeld,
        }
    }

    /// Eine Sperre von Hand aufheben (z. B. Notebook zugeklappt, Sperre soll nicht ablaufen müssen).
    pub fn force_release(&self, name: &str) -> bool {
        self.map().remove(name).is_some()
    }

    /// Darf mit `token` geschrieben werden? Ja, wenn das Design nicht gültig gesperrt ist oder das
    /// Token zur Sperre gehört. Sonst kommt die Sperre zurück.
    pub fn check_write(&self, name: &str, token: Option<&str>, now: Instant) -> Result<(), LockInfo> {
        let map = self.map();
        match map.get(name).filter(|l| l.live(now)) {
            Some(lock) if token != Some(lock.token.as_str()) => Err(lock.info(now)),
            _ => Ok(()),
        }
    }

    pub fn status(&self, name: &str, now: Instant) -> Option<LockInfo> {
        self.map().get(name).filter(|l| l.live(now)).map(|l| l.info(now))
    }

    /// Alle gültigen Sperren, für die Liste der Designs.
    pub fn all(&self, now: Instant) -> HashMap<String, LockInfo> {
        self.map().iter().filter(|(_, l)| l.live(now)).map(|(n, l)| (n.clone(), l.info(now))).collect()
    }

    /// Lange abgelaufene Sperren wegräumen.
    pub fn purge(&self, now: Instant) {
        self.map().retain(|_, l| l.expires + KEEP_EXPIRED > now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TTL: Duration = Duration::from_secs(60);

    fn locks() -> (Locks, Instant) {
        (Locks::new(TTL), Instant::now())
    }

    #[test]
    fn second_acquire_is_refused_with_owner() {
        let (l, t0) = locks();
        assert_eq!(l.acquire("a.solidcraft", "alice", "t1", t0), Ok(TTL));
        let busy = l.acquire("a.solidcraft", "bob", "t2", t0 + Duration::from_secs(10)).unwrap_err();
        assert_eq!(busy.owner, "alice");
        assert_eq!(busy.expires_in, Duration::from_secs(50));
        // dieselbe Person in einer anderen Sitzung ebenso
        assert_eq!(l.acquire("a.solidcraft", "alice", "t3", t0).unwrap_err().owner, "alice");
    }

    #[test]
    fn lock_expires_and_can_be_taken_over() {
        let (l, t0) = locks();
        l.acquire("a", "alice", "t1", t0).unwrap();
        let later = t0 + TTL + Duration::from_secs(1);
        assert!(l.status("a", later).is_none());
        assert_eq!(l.acquire("a", "bob", "t2", later), Ok(TTL));
        assert_eq!(l.status("a", later).unwrap().owner, "bob");
    }

    #[test]
    fn heartbeat_extends_and_needs_the_token() {
        let (l, t0) = locks();
        l.acquire("a", "alice", "t1", t0).unwrap();
        assert!(l.renew("a", "falsch", t0).is_none());
        let t1 = t0 + Duration::from_secs(40);
        assert_eq!(l.renew("a", "t1", t1), Some(TTL));
        // ohne Heartbeat wäre sie bei t0 + 60 s abgelaufen
        assert!(l.status("a", t0 + Duration::from_secs(70)).is_some());
    }

    #[test]
    fn heartbeat_after_short_outage_still_works_until_someone_else_takes_over() {
        let (l, t0) = locks();
        l.acquire("a", "alice", "t1", t0).unwrap();
        let later = t0 + Duration::from_secs(120);
        assert!(l.status("a", later).is_none());
        assert_eq!(l.renew("a", "t1", later), Some(TTL));
        // übernimmt jemand, ist das alte Token wertlos
        let much_later = later + TTL + Duration::from_secs(1);
        l.acquire("a", "bob", "t2", much_later).unwrap();
        assert!(l.renew("a", "t1", much_later).is_none());
    }

    #[test]
    fn release_needs_the_token_or_force() {
        let (l, t0) = locks();
        l.acquire("a", "alice", "t1", t0).unwrap();
        assert_eq!(l.release("a", "falsch", t0), Release::Mismatch);
        assert!(l.status("a", t0).is_some());
        assert_eq!(l.release("a", "t1", t0), Release::Released);
        assert_eq!(l.release("a", "t1", t0), Release::NotHeld);
        l.acquire("a", "alice", "t3", t0).unwrap();
        assert!(l.force_release("a"));
        assert!(!l.force_release("a"));
    }

    #[test]
    fn writes_need_the_lock_token_while_locked() {
        let (l, t0) = locks();
        assert!(l.check_write("a", None, t0).is_ok(), "ohne Sperre darf jeder schreiben");
        l.acquire("a", "alice", "t1", t0).unwrap();
        assert!(l.check_write("a", Some("t1"), t0).is_ok());
        assert_eq!(l.check_write("a", Some("t2"), t0).unwrap_err().owner, "alice");
        assert_eq!(l.check_write("a", None, t0).unwrap_err().owner, "alice");
        assert!(l.check_write("a", None, t0 + TTL + Duration::from_secs(1)).is_ok(), "abgelaufen: frei");
    }

    #[test]
    fn all_lists_only_live_locks_and_purge_forgets_old_ones() {
        let (l, t0) = locks();
        l.acquire("a", "alice", "t1", t0).unwrap();
        l.acquire("b", "bob", "t2", t0 + Duration::from_secs(50)).unwrap();
        let now = t0 + Duration::from_secs(70);
        let all = l.all(now);
        assert_eq!(all.len(), 1);
        assert!(all.contains_key("b"));
        l.purge(t0 + TTL + KEEP_EXPIRED + Duration::from_secs(1));
        assert!(l.renew("a", "t1", t0 + TTL + KEEP_EXPIRED + Duration::from_secs(2)).is_none());
    }
}
