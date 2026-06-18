//! Background thread: reconcile subscriptions against the desired channel set,
//! renew leases before expiry, retry failures with backoff, and compact the log.
//! All hub network calls happen outside the registry lock.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::app::App;
use crate::util::now_unix;
use crate::{hub, resolve, subs};

const RENEW_TICK: u64 = 60; // seconds between wakeups
const RENEW_LEAD: u64 = 24 * 3600; // renew when < 1 day of lease remains
const PENDING_TIMEOUT: u64 = 600; // re-send if a subscribe was never verified
const COMPACT_EVERY_TICKS: u64 = 60; // ~hourly

fn backoff(fail_count: u32) -> u64 {
    let factor = 1u64 << fail_count.min(6); // 1,2,4,...,64
    (30 * factor).min(1800) // 30s .. 30m
}

fn channels_mtime(app: &App) -> u64 {
    fs::metadata(&app.cfg.channels_file)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn save_cache(dir: &str, cache: &HashMap<String, String>) {
    let mut out = String::new();
    for (k, v) in cache {
        out.push_str(k);
        out.push('\t');
        out.push_str(v);
        out.push('\n');
    }
    let _ = fs::write(Path::new(dir).join("resolve.cache"), out);
}

/// Read channels.txt and resolve every entry to a `UCxxxx` id.
fn desired_set(app: &App) -> Vec<String> {
    let content = fs::read_to_string(&app.cfg.channels_file).unwrap_or_default();
    let mut cache = app.resolve_cache.lock().unwrap();
    let mut out: Vec<String> = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        match resolve::resolve(line, &mut cache) {
            Some(uc) if !out.contains(&uc) => out.push(uc),
            Some(_) => {}
            None => eprintln!("[reconcile] could not resolve channel: {}", line),
        }
    }
    save_cache(&app.cfg.storage_dir, &cache);
    out
}

/// Diff the desired set against the registry, subscribing new channels and
/// unsubscribing removed ones. Returns (subscribed, unsubscribed, active_count).
pub fn reconcile(app: &App) -> (usize, usize, usize) {
    let desired = desired_set(app);

    let (to_add, to_remove): (Vec<String>, Vec<subs::Sub>) = {
        let reg = app.subs.lock().unwrap();
        let adds = desired
            .iter()
            .filter(|d| !reg.subs.contains_key(*d))
            .cloned()
            .collect();
        let removes = reg
            .subs
            .values()
            .filter(|s| !desired.contains(&s.channel_id))
            .cloned()
            .collect();
        (adds, removes)
    };

    let mut subscribed = 0;
    for cid in &to_add {
        let mut s = subs::Sub::new(cid);
        s.last_subscribe_at = now_unix();
        match hub::send(&app.cfg, &s, "subscribe") {
            Ok(code) if hub::is_ok(code) => {
                subscribed += 1;
                eprintln!("[reconcile] subscribe {} -> {} (pending verify)", cid, code);
            }
            Ok(code) => {
                eprintln!("[reconcile] subscribe {} -> HTTP {}", cid, code);
                s.state = "failed".into();
                s.fail_count = 1;
                s.next_attempt_at = now_unix() + backoff(1);
            }
            Err(e) => {
                eprintln!("[reconcile] subscribe {} error: {}", cid, e);
                s.state = "failed".into();
                s.fail_count = 1;
                s.next_attempt_at = now_unix() + backoff(1);
            }
        }
        app.subs.lock().unwrap().insert(s);
    }

    let mut unsubscribed = 0;
    for s in &to_remove {
        let _ = hub::send(&app.cfg, s, "unsubscribe");
        app.subs.lock().unwrap().remove(&s.channel_id);
        unsubscribed += 1;
        eprintln!("[reconcile] unsubscribe {}", s.channel_id);
    }

    let reg = app.subs.lock().unwrap();
    let _ = reg.save();
    let active = reg.subs.values().filter(|s| s.state == "active").count();
    (subscribed, unsubscribed, active)
}

/// Re-send subscribe for leases nearing expiry, unverified-too-long subscribes,
/// and failed subscriptions whose backoff has elapsed.
fn renew_due(app: &App) {
    let now = now_unix();
    let candidates: Vec<subs::Sub> = {
        let reg = app.subs.lock().unwrap();
        reg.subs
            .values()
            .filter(|s| {
                if now < s.next_attempt_at {
                    return false;
                }
                match s.state.as_str() {
                    "active" => s.expires_at > 0 && now + RENEW_LEAD >= s.expires_at,
                    "pending" => now.saturating_sub(s.last_subscribe_at) > PENDING_TIMEOUT,
                    "failed" => true,
                    _ => false,
                }
            })
            .cloned()
            .collect()
    };

    for mut s in candidates {
        let was_active = s.state == "active";
        s.last_subscribe_at = now_unix();
        match hub::send(&app.cfg, &s, "subscribe") {
            Ok(code) if hub::is_ok(code) => {
                // The verify GET will (re)set active + expires_at. An active sub
                // stays active in the meantime, so there is no coverage gap.
                if !was_active {
                    s.state = "pending".into();
                }
                s.fail_count = 0;
                s.next_attempt_at = 0;
            }
            Ok(code) => {
                eprintln!("[renew] {} -> HTTP {}", s.channel_id, code);
                s.fail_count += 1;
                if !was_active {
                    s.state = "failed".into();
                }
                s.next_attempt_at = now_unix() + backoff(s.fail_count);
            }
            Err(e) => {
                eprintln!("[renew] {} error: {}", s.channel_id, e);
                s.fail_count += 1;
                if !was_active {
                    s.state = "failed".into();
                }
                s.next_attempt_at = now_unix() + backoff(s.fail_count);
            }
        }
        let mut reg = app.subs.lock().unwrap();
        // Skip if a concurrent reconcile removed it.
        if reg.subs.contains_key(&s.channel_id) {
            reg.update(s);
            let _ = reg.save();
        }
    }
}

pub fn run(app: Arc<App>) {
    reconcile(&app);
    let mut last_mtime = channels_mtime(&app);
    let mut ticks = 0u64;
    loop {
        thread::sleep(Duration::from_secs(RENEW_TICK));
        ticks += 1;

        let m = channels_mtime(&app);
        if m != last_mtime {
            last_mtime = m;
            eprintln!("[reconcile] channels file changed; reconciling");
            reconcile(&app);
        }

        renew_due(&app);

        if ticks % COMPACT_EVERY_TICKS == 0 {
            if let Err(e) = app.store.lock().unwrap().maybe_compact() {
                eprintln!("[store] compaction error: {}", e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_caps() {
        assert_eq!(backoff(0), 30);
        assert_eq!(backoff(1), 60);
        assert_eq!(backoff(2), 120);
        assert_eq!(backoff(10), 1800); // capped
    }
}
