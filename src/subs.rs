//! In-memory subscription registry, persisted as a TSV file via atomic rewrite.
//! Each subscription has its own unguessable callback `token` (the `/yt/cb/<token>`
//! path segment) and its own `secret`, so a notification POST identifies which
//! subscription — and thus which secret to verify against — purely from its URL.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::util::rand_hex;

/// Create/truncate `path` for writing, owner-only (0600) on Unix. subs.tsv holds
/// per-subscription HMAC secrets and callback tokens in cleartext, so it must not
/// be world-readable regardless of the process umask.
fn create_private(path: &Path) -> std::io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
}

#[derive(Clone, Debug)]
pub struct Sub {
    pub channel_id: String,
    pub token: String,  // per-sub callback path id
    pub topic: String,  // YouTube feed URL
    pub secret: String, // HMAC key (opaque string)
    pub state: String,  // pending | active | expired | failed
    pub lease_seconds: u64,
    pub expires_at: u64, // unix; 0 until verified active
    pub last_subscribe_at: u64,
    pub fail_count: u32,
    pub next_attempt_at: u64,
    /// What the hub said about the last (re)subscribe attempt: `HTTP 429`, a
    /// transport error, or empty when the last attempt was accepted.
    ///
    /// Without this a persistently refused subscription is indistinguishable
    /// from a healthy one at the API — all you see is a fail_count climbing,
    /// with the actual verdict buried in journald on the host. That gap cost
    /// real time on 2026-09-06, when 14 subscriptions had been refused every
    /// 30 minutes for two days behind a green `/api/health`.
    pub last_error: String,
}

pub fn topic_for(channel_id: &str) -> String {
    format!(
        "https://www.youtube.com/feeds/videos.xml?channel_id={}",
        channel_id
    )
}

impl Sub {
    pub fn new(channel_id: &str) -> Sub {
        Sub {
            channel_id: channel_id.to_string(),
            token: rand_hex(16),
            topic: topic_for(channel_id),
            secret: rand_hex(20),
            state: "pending".to_string(),
            lease_seconds: 0,
            expires_at: 0,
            last_subscribe_at: 0,
            fail_count: 0,
            next_attempt_at: 0,
            last_error: String::new(),
        }
    }

    /// True once the hub's lease has run out. Such a sub is NOT subscribed any
    /// more, however its `state` field reads: the hub stopped delivering the
    /// moment the lease passed.
    pub fn lease_expired(&self, now: u64) -> bool {
        self.expires_at > 0 && now >= self.expires_at
    }
}

/// Longest hub verdict we keep. A transport error can carry a whole chain of
/// causes; the head of it is the part worth storing.
const MAX_ERR: usize = 200;

/// Bound a hub verdict before it is stored. Escaping is `pct_encode`'s job at
/// write time (same as every other string field in this codebase), so this
/// only has to cap the length — on a char boundary, since an error string can
/// carry non-ASCII.
pub fn clamp_error(s: &str) -> String {
    s.chars().take(MAX_ERR).collect()
}

/// How long a removed subscription's callback token stays answerable so the
/// hub can verify the unsubscribe. Google verifies out of band, seconds to
/// minutes after the POST returns.
const UNSUB_GRACE: u64 = 900;

pub struct Registry {
    pub subs: HashMap<String, Sub>, // keyed by channel_id
    by_token: HashMap<String, String>,
    /// Tokens of subs we have asked the hub to drop, `token -> (topic, deadline)`.
    ///
    /// An unsubscribe is verified exactly like a subscribe: the hub GETs the
    /// callback with `hub.mode=unsubscribe` and expects the challenge echoed.
    /// Removing the sub from `subs` the instant the request was sent made that
    /// GET 404, so the hub abandoned the unsubscribe and kept delivering — the
    /// removal only ever took effect locally. Keeping the token answerable for
    /// a grace period is what actually completes it.
    ///
    /// Deliberately not persisted: a restart mid-unsubscribe just falls back
    /// to the old behaviour for that one token, and the alternative is writing
    /// tombstones into a file whose whole job is holding live secrets.
    unsub_pending: HashMap<String, (String, u64)>,
    path: PathBuf,
}

fn fmt_line(s: &Sub) -> String {
    format!(
        "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
        s.channel_id,
        s.token,
        s.topic,
        s.secret,
        s.state,
        s.lease_seconds,
        s.expires_at,
        s.last_subscribe_at,
        s.fail_count,
        s.next_attempt_at,
        crate::util::pct_encode(&clamp_error(&s.last_error))
    )
}

/// Parse one subs.tsv row.
///
/// Trailing columns are OPTIONAL, and must stay that way: the first ten are
/// the original format, and a row written by an older build must keep loading
/// after an upgrade. A strict `len() != N` check would drop every existing row
/// on the first restart after deploy — silently unsubscribing the whole fleet,
/// since a registry that loads empty looks exactly like a fresh install.
fn parse_line(line: &str) -> Option<Sub> {
    let p: Vec<&str> = line.split('\t').collect();
    if p.len() < 10 {
        return None;
    }
    Some(Sub {
        channel_id: p[0].to_string(),
        token: p[1].to_string(),
        topic: p[2].to_string(),
        secret: p[3].to_string(),
        state: p[4].to_string(),
        lease_seconds: p[5].parse().ok()?,
        expires_at: p[6].parse().ok()?,
        last_subscribe_at: p[7].parse().ok()?,
        fail_count: p[8].parse().ok()?,
        next_attempt_at: p[9].parse().ok()?,
        last_error: p.get(10).map(|s| crate::util::pct_decode(s)).unwrap_or_default(),
    })
}

impl Registry {
    pub fn load(path: &Path) -> Registry {
        let mut subs = HashMap::new();
        let mut by_token = HashMap::new();
        if let Ok(content) = fs::read_to_string(path) {
            for line in content.lines() {
                if let Some(s) = parse_line(line) {
                    by_token.insert(s.token.clone(), s.channel_id.clone());
                    subs.insert(s.channel_id.clone(), s);
                }
            }
        }
        Registry {
            subs,
            by_token,
            unsub_pending: HashMap::new(),
            path: path.to_path_buf(),
        }
    }

    pub fn save(&self) -> std::io::Result<()> {
        let tmp = self.path.with_extension("tsv.tmp");
        let mut out = String::new();
        for s in self.subs.values() {
            out.push_str(&fmt_line(s));
        }
        // Write + fsync + atomic rename so a crash/power-loss can't leave subs.tsv
        // torn or empty (which would drop every subscription on reload). Create it
        // 0600 since it stores secrets.
        {
            let mut f = create_private(&tmp)?;
            f.write_all(out.as_bytes())?;
            f.sync_data()?;
        }
        fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    pub fn by_token(&self, token: &str) -> Option<Sub> {
        self.by_token
            .get(token)
            .and_then(|cid| self.subs.get(cid))
            .cloned()
    }

    pub fn insert(&mut self, s: Sub) {
        self.by_token.insert(s.token.clone(), s.channel_id.clone());
        self.subs.insert(s.channel_id.clone(), s);
    }

    /// Replace a sub (keyed by channel_id), keeping the token index consistent.
    pub fn update(&mut self, s: Sub) {
        // If this channel previously had a different token, drop the stale
        // token→channel index entry so a superseded token can't still resolve.
        if let Some(old) = self.subs.get(&s.channel_id) {
            if old.token != s.token {
                self.by_token.remove(&old.token);
            }
        }
        self.by_token.insert(s.token.clone(), s.channel_id.clone());
        self.subs.insert(s.channel_id.clone(), s);
    }

    /// Drop a subscription, keeping its callback token answerable for
    /// [`UNSUB_GRACE`] so the hub's unsubscribe verification can complete.
    pub fn remove(&mut self, channel_id: &str, now: u64) -> Option<Sub> {
        if let Some(s) = self.subs.remove(channel_id) {
            self.by_token.remove(&s.token);
            self.unsub_pending
                .insert(s.token.clone(), (s.topic.clone(), now + UNSUB_GRACE));
            self.sweep_unsub(now);
            Some(s)
        } else {
            None
        }
    }

    /// The topic a removed-but-unverified token was subscribed to, if its
    /// grace period is still running.
    pub fn unsub_topic(&self, token: &str, now: u64) -> Option<String> {
        self.unsub_pending
            .get(token)
            .filter(|(_, deadline)| now < *deadline)
            .map(|(topic, _)| topic.clone())
    }

    /// Called once the hub has confirmed an unsubscribe; also drops any
    /// tombstone whose grace period has run out.
    pub fn forget_unsub(&mut self, token: &str, now: u64) {
        self.unsub_pending.remove(token);
        self.sweep_unsub(now);
    }

    fn sweep_unsub(&mut self, now: u64) {
        self.unsub_pending.retain(|_, (_, deadline)| now < *deadline);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_load_roundtrip() {
        let dir = std::env::temp_dir().join("yt_websub_test_subs");
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("subs.tsv");
        let mut reg = Registry::load(&path);
        let s = Sub::new("UCabcdefghijklmnopqrstuv");
        let token = s.token.clone();
        reg.insert(s);
        reg.save().unwrap();

        let reg2 = Registry::load(&path);
        assert_eq!(reg2.subs.len(), 1);
        assert!(reg2.by_token(&token).is_some());
        assert_eq!(
            reg2.subs.get("UCabcdefghijklmnopqrstuv").unwrap().topic,
            topic_for("UCabcdefghijklmnopqrstuv")
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// A row written by the pre-`last_error` build must still load. If it did
    /// not, the first restart after deploy would read the whole registry as
    /// unparseable, come up empty, and look exactly like a fresh install —
    /// silently dropping every subscription on the box.
    #[test]
    fn a_legacy_ten_field_row_still_loads() {
        let legacy = "UCabcdefghijklmnopqrstuv\ttok123\thttps://example.invalid/feed\t\
                      sec456\tactive\t432000\t1788800000\t1788400000\t3\t1788400300";
        let s = parse_line(legacy).expect("legacy row must parse");
        assert_eq!(s.channel_id, "UCabcdefghijklmnopqrstuv");
        assert_eq!(s.state, "active");
        assert_eq!(s.fail_count, 3);
        assert_eq!(s.last_error, "", "absent column reads as no error");

        // Too few fields is still a reject; a torn line must not become a sub.
        assert!(parse_line("UConly\ttwo").is_none());
    }

    /// The hub verdict survives a save/load round trip even when it carries the
    /// characters that would otherwise tear the TSV apart.
    #[test]
    fn a_hub_verdict_round_trips_through_the_tsv() {
        let dir = std::env::temp_dir().join("yt_websub_test_lasterr");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("subs.tsv");

        let mut reg = Registry::load(&path);
        let mut s = Sub::new("UCabcdefghijklmnopqrstuv");
        s.last_error = "HTTP 503\tTransient error;\nplease try again later".to_string();
        reg.insert(s);
        reg.save().unwrap();

        // One row, not three: the embedded newline must not have split it.
        let raw = fs::read_to_string(&path).unwrap();
        assert_eq!(raw.lines().count(), 1, "verdict tore the file: {raw:?}");

        let reg2 = Registry::load(&path);
        assert_eq!(reg2.subs.len(), 1);
        assert_eq!(
            reg2.subs.get("UCabcdefghijklmnopqrstuv").unwrap().last_error,
            "HTTP 503\tTransient error;\nplease try again later"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// A removed sub's callback token stays answerable for the grace period, so
    /// the hub's unsubscribe verification GET resolves instead of 404ing — the
    /// bug that meant no unsubscribe ever actually completed at the hub.
    #[test]
    fn a_removed_sub_can_still_confirm_its_unsubscribe() {
        let dir = std::env::temp_dir().join("yt_websub_test_tomb");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(&dir);
        let mut reg = Registry::load(&dir.join("subs.tsv"));
        let now = 1_000_000u64;

        let s = Sub::new("UCabcdefghijklmnopqrstuv");
        let (token, topic) = (s.token.clone(), s.topic.clone());
        reg.insert(s);
        reg.remove("UCabcdefghijklmnopqrstuv", now);

        // Gone as a live sub...
        assert!(reg.by_token(&token).is_none());
        // ...but still able to answer the hub's verification.
        assert_eq!(reg.unsub_topic(&token, now + 60), Some(topic));
        // Not forever, and not for a token we never issued.
        assert_eq!(reg.unsub_topic(&token, now + UNSUB_GRACE), None);
        assert_eq!(reg.unsub_topic("not-a-token", now), None);

        // Once the hub confirms, the tombstone goes.
        reg.remove("UCabcdefghijklmnopqrstuv", now); // no-op, already gone
        assert_eq!(reg.unsub_topic(&token, now + 60), Some(topic_for("UCabcdefghijklmnopqrstuv")));
        reg.forget_unsub(&token, now);
        assert_eq!(reg.unsub_topic(&token, now + 60), None);

        let _ = fs::remove_dir_all(&dir);
    }

    /// A lease is what decides whether the hub is delivering, not the state
    /// field next to it.
    #[test]
    fn lease_expiry_is_read_from_the_clock() {
        let mut s = Sub::new("UCabcdefghijklmnopqrstuv");
        assert!(!s.lease_expired(1_000_000), "expires_at 0 means never verified");
        s.expires_at = 1_000_000;
        assert!(!s.lease_expired(999_999));
        assert!(s.lease_expired(1_000_000));
        assert!(s.lease_expired(1_000_001));
    }
}
