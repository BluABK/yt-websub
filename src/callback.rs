//! The public callback endpoint the hub talks to.
//! - GET  = subscription verification: echo `hub.challenge`, mark sub `active`.
//! - POST = content notification: verify HMAC, parse Atom, durably append.
//!
//! The `<token>` path segment identifies the subscription (and thus its secret),
//! so we never trust the body to decide which key to verify against.

use std::io::Read;

use tiny_http::{Method, Request, Response};

use crate::app::App;
use crate::util::{now_unix, query_get};
use crate::{atom, sig};

const MAX_BODY: u64 = 4 * 1024 * 1024;

pub fn handle(app: &App, req: Request, token: &str, query: &str) {
    // Identify the subscription, then release the lock before doing any work.
    let sub = {
        let reg = app.subs.lock().unwrap();
        match reg.by_token(token) {
            Some(s) => s,
            None => {
                drop(reg);
                let _ = req.respond(Response::empty(404u16));
                return;
            }
        }
    };

    match req.method() {
        Method::Get => handle_verify(app, req, &sub, query),
        Method::Post => handle_notify(app, req, &sub),
        _ => {
            let _ = req.respond(Response::empty(405u16));
        }
    }
}

fn handle_verify(app: &App, req: Request, sub: &crate::subs::Sub, query: &str) {
    let mode = query_get(query, "hub.mode").unwrap_or_default();
    let topic = query_get(query, "hub.topic").unwrap_or_default();
    let challenge = query_get(query, "hub.challenge").unwrap_or_default();
    let vtoken = query_get(query, "hub.verify_token").unwrap_or_default();

    // Reject anything that doesn't match the subscription we actually requested.
    if topic != sub.topic
        || challenge.is_empty()
        || (!vtoken.is_empty() && vtoken != sub.token)
    {
        let _ = req.respond(Response::empty(404u16));
        return;
    }

    if mode == "subscribe" {
        let lease = query_get(query, "hub.lease_seconds")
            .and_then(|s| s.parse().ok())
            .unwrap_or(app.cfg.lease_seconds);
        let mut reg = app.subs.lock().unwrap();
        if let Some(mut s) = reg.subs.get(&sub.channel_id).cloned() {
            s.state = "active".to_string();
            s.lease_seconds = lease;
            s.expires_at = now_unix() + lease;
            s.fail_count = 0;
            s.next_attempt_at = 0;
            reg.update(s);
            let _ = reg.save();
        }
        drop(reg);
        eprintln!(
            "[verify] active channel={} lease={}s",
            sub.channel_id, lease
        );
        let _ = req.respond(Response::from_string(challenge));
    } else if mode == "unsubscribe" {
        // Removal already handled by reconcile; just confirm to the hub.
        let _ = req.respond(Response::from_string(challenge));
    } else {
        let _ = req.respond(Response::empty(404u16));
    }
}

fn handle_notify(app: &App, mut req: Request, sub: &crate::subs::Sub) {
    let header_sig = req
        .headers()
        .iter()
        .find(|h| h.field.equiv("X-Hub-Signature"))
        .map(|h| h.value.as_str().to_string());

    let mut body = Vec::new();
    let _ = req.as_reader().take(MAX_BODY).read_to_end(&mut body);

    let ok = match &header_sig {
        Some(h) => sig::verify(sub.secret.as_bytes(), &body, h),
        None => false,
    };
    if !ok {
        eprintln!(
            "[callback] dropping notification with bad/missing signature (channel {})",
            sub.channel_id
        );
        let _ = req.respond(Response::empty(204u16)); // ack but ignore (no retry storm)
        return;
    }

    let text = String::from_utf8_lossy(&body);
    let entries = atom::parse(&text);
    {
        let mut store = app.store.lock().unwrap();
        for e in &entries {
            match store.append(&e.kind, &e.channel_id, &e.video_id, &e.ts, &e.title) {
                Ok(Some(seq)) => eprintln!(
                    "[event] seq={} kind={} channel={} video={} title={:?}",
                    seq, e.kind, e.channel_id, e.video_id, e.title
                ),
                Ok(None) => {}
                Err(err) => eprintln!("[store] append error: {}", err),
            }
        }
    }
    let _ = req.respond(Response::empty(204u16));
}
