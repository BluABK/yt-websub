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
    pub state: String,  // pending | active | failed
    pub lease_seconds: u64,
    pub expires_at: u64, // unix; 0 until verified active
    pub last_subscribe_at: u64,
    pub fail_count: u32,
    pub next_attempt_at: u64,
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
        }
    }
}

pub struct Registry {
    pub subs: HashMap<String, Sub>, // keyed by channel_id
    by_token: HashMap<String, String>,
    path: PathBuf,
}

fn fmt_line(s: &Sub) -> String {
    format!(
        "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
        s.channel_id,
        s.token,
        s.topic,
        s.secret,
        s.state,
        s.lease_seconds,
        s.expires_at,
        s.last_subscribe_at,
        s.fail_count,
        s.next_attempt_at
    )
}

fn parse_line(line: &str) -> Option<Sub> {
    let p: Vec<&str> = line.split('\t').collect();
    if p.len() != 10 {
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

    pub fn remove(&mut self, channel_id: &str) -> Option<Sub> {
        if let Some(s) = self.subs.remove(channel_id) {
            self.by_token.remove(&s.token);
            Some(s)
        } else {
            None
        }
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
}
