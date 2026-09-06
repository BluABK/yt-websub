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

/// Pause between consecutive hub requests inside one reconcile/renew pass.
///
/// Subscriptions created together expire together, so they also come due
/// together, and the loop used to fire the whole cohort at the hub back to
/// back. Spacing them keeps one bad minute from taking out a whole cohort at
/// once — and keeps us a well-behaved client of a hub we do not control.
const HUB_SPACING_MS: u64 = 400;

/// Exponential backoff for a failed (re)subscribe, plus jitter.
///
/// The jitter is not cosmetic. Without it a cohort that failed together
/// retries together forever: same fail_count, same backoff, same instant,
/// re-colliding on every round. Spreading the retry over a quarter of the
/// interval breaks that lockstep.
fn backoff(fail_count: u32) -> u64 {
    let factor = 1u64 << fail_count.min(6); // 1,2,4,...,64
    let base = (30 * factor).min(1800); // 30s .. 30m
    base + crate::util::rand_below(base / 4 + 1)
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
    for (i, cid) in to_add.iter().enumerate() {
        if i > 0 {
            thread::sleep(Duration::from_millis(HUB_SPACING_MS));
        }
        let mut s = subs::Sub::new(cid);
        s.last_subscribe_at = now_unix();
        // Register the token BEFORE contacting the hub so a fast async verify GET
        // (which carries this token) resolves instead of 404ing. The lock is held
        // only for the map insert, never across the network call below.
        app.subs.lock().unwrap().insert(s.clone());
        match verdict(hub::send(&app.cfg, &s, "subscribe")) {
            None => {
                subscribed += 1;
                eprintln!("[reconcile] subscribe {} -> accepted (pending verify)", cid);
                // Leave it pending; the verify GET will activate it.
            }
            Some(why) => {
                eprintln!("[reconcile] subscribe {} refused: {}", cid, why);
                s.state = "failed".into();
                s.fail_count = 1;
                s.next_attempt_at = now_unix() + backoff(1);
                s.last_error = why;
                merge_attempt(app, s);
            }
        }
    }

    let mut unsubscribed = 0;
    for (i, s) in to_remove.iter().enumerate() {
        if i > 0 {
            thread::sleep(Duration::from_millis(HUB_SPACING_MS));
        }
        let _ = hub::send(&app.cfg, s, "unsubscribe");
        // Removing here drops the sub from the live map but KEEPS its callback
        // token answerable for a grace period, because the hub verifies an
        // unsubscribe with a GET to that same token path. Dropping the token
        // outright made that GET 404, the hub abandoned the unsubscribe, and the
        // removal only ever took effect on our side.
        app.subs.lock().unwrap().remove(&s.channel_id, now_unix());
        unsubscribed += 1;
        eprintln!("[reconcile] unsubscribe {} (pending verify)", s.channel_id);
    }

    let reg = app.subs.lock().unwrap();
    let _ = reg.save();
    (subscribed, unsubscribed, live_count(&reg, now_unix()))
}

/// Reduce a `hub::send` result to "accepted" (`None`) or why it was not.
///
/// A transport failure and an HTTP rejection are the same event to every
/// caller — the request did not land — and both have to end up in the same
/// stored field, so they are collapsed once, here, rather than in each of the
/// four call sites that used to duplicate the pair of match arms.
fn verdict(res: Result<u16, String>) -> Option<String> {
    match res {
        Ok(code) if hub::is_ok(code) => None,
        Ok(code) => Some(format!("HTTP {}", code)),
        Err(e) => Some(subs::clamp_error(&e)),
    }
}

/// Subscriptions the hub is actually delivering to: `active` AND still inside
/// their lease.
///
/// Counting bare `state == "active"` is what let a third of the fleet sit
/// unsubscribed behind a green `/api/health` — the state field is only ever
/// as fresh as the last renewal that managed to complete.
fn live_count(reg: &subs::Registry, now: u64) -> usize {
    reg.subs
        .values()
        .filter(|s| s.state == "active" && !s.lease_expired(now))
        .count()
}

/// Re-send subscribe for leases nearing expiry, unverified-too-long subscribes,
/// and failed subscriptions whose backoff has elapsed.
/// Demote any `active` sub whose lease has quietly run out.
///
/// An active sub that fails renewal keeps `state = "active"` on purpose, so a
/// brief hub blip does not report a coverage gap that isn't there. The bug was
/// that it kept it *forever*: once the lease passed, the hub had stopped
/// delivering, the expiry never advanced again, and `/api/health` still counted
/// the sub as active. On 2026-09-06 twelve subscriptions had been dead for up
/// to 1.8 days that way, one of them behind 73 consecutive failed renewals.
///
/// Demoting to `expired` keeps the sub in the registry and still due for
/// retry — it only stops it claiming to be delivering.
fn expire_lapsed(app: &App, now: u64) {
    let mut reg = app.subs.lock().unwrap();
    let lapsed: Vec<String> = reg
        .subs
        .values()
        .filter(|s| s.state == "active" && s.lease_expired(now))
        .map(|s| s.channel_id.clone())
        .collect();
    if lapsed.is_empty() {
        return;
    }
    for cid in &lapsed {
        if let Some(s) = reg.subs.get_mut(cid) {
            s.state = "expired".into();
            eprintln!(
                "[renew] {} lease expired {}s ago; no longer counted active",
                cid,
                now.saturating_sub(s.expires_at)
            );
        }
    }
    let _ = reg.save();
}

fn renew_due(app: &App) {
    let now = now_unix();
    expire_lapsed(app, now);
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
                    // A lapsed lease is as retryable as an outright failure —
                    // more so, since it is a channel we believed we had.
                    "failed" | "expired" => true,
                    _ => false,
                }
            })
            .cloned()
            .collect()
    };

    for (i, mut s) in candidates.into_iter().enumerate() {
        if i > 0 {
            thread::sleep(Duration::from_millis(HUB_SPACING_MS));
        }
        let was_active = s.state == "active";
        s.last_subscribe_at = now_unix();
        match verdict(hub::send(&app.cfg, &s, "subscribe")) {
            None => {
                // The verify GET will (re)set active + expires_at. An active sub
                // stays active in the meantime, so there is no coverage gap. Set
                // a cooldown so we don't re-send every tick while the verify GET
                // is in flight (or if the callback is briefly unreachable).
                if !was_active {
                    s.state = "pending".into();
                }
                s.fail_count = 0;
                s.last_error.clear();
                s.next_attempt_at = now_unix() + RENEW_COOLDOWN;
            }
            Some(why) => {
                eprintln!("[renew] {} refused: {}", s.channel_id, why);
                s.fail_count += 1;
                if !was_active {
                    s.state = "failed".into();
                }
                s.last_error = why;
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

    /// The backoff still doubles and still caps; jitter only ever extends the
    /// wait, and by at most a quarter — never shortens it, or a hard-failing
    /// cohort would creep towards hammering the hub.
    #[test]
    fn backoff_grows_and_caps() {
        for _ in 0..200 {
            assert!((30..=37).contains(&backoff(0)), "{}", backoff(0));
            assert!((60..=75).contains(&backoff(1)));
            assert!((120..=150).contains(&backoff(2)));
            assert!((1800..=2250).contains(&backoff(10))); // capped base + jitter
        }
    }

    /// A cohort that failed together must not retry in lockstep forever.
    #[test]
    fn backoff_jitter_actually_spreads_a_cohort() {
        let waits: std::collections::HashSet<u64> = (0..50).map(|_| backoff(6)).collect();
        assert!(
            waits.len() > 10,
            "jitter collapsed to {} distinct waits; a cohort would re-collide every round",
            waits.len()
        );
    }

    /// Transport failure and HTTP rejection are the same event to the caller,
    /// and an accepted request must record no error at all.
    #[test]
    fn verdict_collapses_both_failure_shapes() {
        assert_eq!(verdict(Ok(202)), None);
        assert_eq!(verdict(Ok(204)), None);
        assert_eq!(verdict(Ok(503)), Some("HTTP 503".to_string()));
        assert_eq!(verdict(Ok(409)), Some("HTTP 409".to_string()));
        assert_eq!(
            verdict(Err("timed out reading response".into())),
            Some("timed out reading response".to_string())
        );
    }

    /// The count behind `/api/health` must exclude a sub whose lease has run
    /// out, however its state field still reads. This is the whole bug: on
    /// 2026-09-06 twelve subs sat `active` with leases up to 1.8 days dead and
    /// the endpoint reported full coverage.
    #[test]
    fn live_count_excludes_a_lapsed_lease() {
        let dir = std::env::temp_dir().join("yt_websub_test_live_count");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(&dir);
        let mut reg = subs::Registry::load(&dir.join("subs.tsv"));
        let now = 1_000_000u64;

        let mut good = subs::Sub::new("UCgood0000000000000000a");
        good.state = "active".into();
        good.expires_at = now + 60;
        reg.insert(good);

        let mut lapsed = subs::Sub::new("UClapsed00000000000000b");
        lapsed.state = "active".into(); // never demoted: the pre-fix shape
        lapsed.expires_at = now - 1;
        reg.insert(lapsed);

        let mut never = subs::Sub::new("UCnever000000000000000c");
        never.state = "active".into();
        never.expires_at = 0; // verified but no expiry recorded
        reg.insert(never);

        assert_eq!(live_count(&reg, now), 2, "only the lapsed sub should drop out");
        let _ = fs::remove_dir_all(&dir);
    }
}
