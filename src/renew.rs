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
const RENEW_COOLDOWN: u64 = 300; // wait after a (re)subscribe send before retrying
const COMPACT_EVERY_TICKS: u64 = 60; // ~hourly

fn backoff(fail_count: u32) -> u64 {
    let factor = 1u64 << fail_count.min(6); // 1,2,4,...,64
    (30 * factor).min(1800) // 30s .. 30m
}

/// Change signature of channels.txt: (mtime_secs, len). Including the length as
/// well as the second-granularity mtime avoids missing a same-second edit that
/// changes the file size (which nearly every real edit does).
fn channels_sig(app: &App) -> (u64, u64) {
    match fs::metadata(&app.cfg.channels_file) {
        Ok(m) => {
            let mtime = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            (mtime, m.len())
        }
        Err(_) => (0, 0),
    }
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

/// Read channels.txt and resolve every entry to a `UCxxxx` id. The bool is
/// `complete`: false if any non-comment line failed to resolve, in which case
/// the caller must NOT treat the set as authoritative for removals (a transient
/// resolution failure must never unsubscribe a healthy channel).
fn desired_set(app: &App) -> (Vec<String>, bool) {
    // A missing/unreadable channels file must NOT be treated as "zero desired
    // channels" — that would make reconcile unsubscribe everything. Treat it as
    // incomplete (skip removals) instead.
    let content = match fs::read_to_string(&app.cfg.channels_file) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "[reconcile] cannot read channels file {}: {}; skipping removals this cycle",
                app.cfg.channels_file, e
            );
            return (Vec::new(), false);
        }
    };

    // Resolve against a snapshot of the cache so we hold no lock across network
    // I/O (per the app-wide no-network-under-lock invariant), then merge back.
    let mut cache = app.resolve_cache.lock().unwrap().clone();
    let mut out: Vec<String> = Vec::new();
    let mut complete = true;
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        match resolve::resolve(line, &mut cache) {
            Some(uc) if !out.contains(&uc) => out.push(uc),
            Some(_) => {}
            None => {
                complete = false;
                eprintln!("[reconcile] could not resolve channel: {}", line);
            }
        }
    }
    {
        let mut live = app.resolve_cache.lock().unwrap();
        for (k, v) in &cache {
            live.entry(k.clone()).or_insert_with(|| v.clone());
        }
        save_cache(&app.cfg.storage_dir, &live);
    }
    (out, complete)
}

/// Diff the desired set against the registry, subscribing new channels and
/// unsubscribing removed ones. Returns (subscribed, unsubscribed, active_count).
pub fn reconcile(app: &App) -> (usize, usize, usize) {
    // Serialize whole reconciles so API-driven and timer-driven runs don't race.
    let _guard = app.reconcile_lock.lock().unwrap();
    reconcile_locked(app)
}

/// Like `reconcile`, but returns `None` immediately if another reconcile is
/// already running rather than blocking. The API handler uses this so a POST
/// cannot pin an HTTP worker for the duration of another reconcile's network I/O.
pub fn try_reconcile(app: &App) -> Option<(usize, usize, usize)> {
    match app.reconcile_lock.try_lock() {
        Ok(_guard) => Some(reconcile_locked(app)),
        Err(_) => None,
    }
}

/// Write back the outcome of a (re)subscribe attempt, but only if the stored sub
/// is still the same one (same token — a concurrent reconcile may have replaced
/// it) and without clobbering an activation/expiry a verify GET landed while we
/// were sending.
fn merge_attempt(app: &App, mut s: subs::Sub) {
    let mut reg = app.subs.lock().unwrap();
    if let Some(cur) = reg.subs.get(&s.channel_id) {
        if cur.token != s.token {
            return; // replaced concurrently; drop our stale write
        }
        if cur.state == "active" {
            // A verify GET already (re)activated it — keep that state and its
            // fresher expiry, don't revert to our pre-send snapshot.
            s.state = "active".to_string();
            s.expires_at = cur.expires_at;
            s.lease_seconds = cur.lease_seconds;
        }
        reg.update(s);
        let _ = reg.save();
    }
}

fn reconcile_locked(app: &App) -> (usize, usize, usize) {
    let (desired, complete) = desired_set(app);

    let (to_add, to_remove): (Vec<String>, Vec<subs::Sub>) = {
        let reg = app.subs.lock().unwrap();
        let adds = desired
            .iter()
            .filter(|d| !reg.subs.contains_key(*d))
            .cloned()
            .collect();
        // Only remove subs when the desired set is complete; otherwise a failed
        // resolution this cycle would wrongly drop a still-wanted channel.
        let removes = if complete {
            reg.subs
                .values()
                .filter(|s| !desired.contains(&s.channel_id))
                .cloned()
                .collect()
        } else {
            eprintln!("[reconcile] desired set incomplete (resolution failures); skipping removals");
            Vec::new()
        };
        (adds, removes)
    };

    let mut subscribed = 0;
    for cid in &to_add {
        let mut s = subs::Sub::new(cid);
        s.last_subscribe_at = now_unix();
        // Register the token BEFORE contacting the hub so a fast async verify GET
        // (which carries this token) resolves instead of 404ing. The lock is held
        // only for the map insert, never across the network call below.
        app.subs.lock().unwrap().insert(s.clone());
        match hub::send(&app.cfg, &s, "subscribe") {
            Ok(code) if hub::is_ok(code) => {
                subscribed += 1;
                eprintln!("[reconcile] subscribe {} -> {} (pending verify)", cid, code);
                // Leave it pending; the verify GET will activate it.
            }
            Ok(code) => {
                eprintln!("[reconcile] subscribe {} -> HTTP {}", cid, code);
                s.state = "failed".into();
                s.fail_count = 1;
                s.next_attempt_at = now_unix() + backoff(1);
                merge_attempt(app, s);
            }
            Err(e) => {
                eprintln!("[reconcile] subscribe {} error: {}", cid, e);
                s.state = "failed".into();
                s.fail_count = 1;
                s.next_attempt_at = now_unix() + backoff(1);
                merge_attempt(app, s);
            }
        }
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
                // stays active in the meantime, so there is no coverage gap. Set
                // a cooldown so we don't re-send every tick while the verify GET
                // is in flight (or if the callback is briefly unreachable).
                if !was_active {
                    s.state = "pending".into();
                }
                s.fail_count = 0;
                s.next_attempt_at = now_unix() + RENEW_COOLDOWN;
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
        // Merge the outcome without clobbering a concurrent verify/replacement
        // (also skips if a reconcile removed the sub or swapped its token).
        merge_attempt(app, s);
    }
}

pub fn run(app: Arc<App>) {
    reconcile(&app);
    let mut last_sig = channels_sig(&app);
    let mut ticks = 0u64;
    loop {
        thread::sleep(Duration::from_secs(RENEW_TICK));
        ticks += 1;

        let sig = channels_sig(&app);
        if sig != last_sig {
            last_sig = sig;
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
