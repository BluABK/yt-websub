//! Token-authenticated control/poll API consumed by streamarchiver.
//! - POST /api/channels : set the desired channel set, reconcile subscriptions.
//! - GET  /api/events   : pull events after a cursor.
//! - POST /api/ack      : advance the compaction horizon.
//! - GET  /api/health   : status.
//!
//! JSON is hand-emitted/parsed for the few fixed shapes to avoid a serde dep.

use std::io::Read;

use tiny_http::{Header, Method, Request, Response};

use crate::app::App;
use crate::util::{json_string, now_unix, query_get};

const MAX_BODY: u64 = 1024 * 1024;

fn json_response(req: Request, code: u16, body: String) {
    let header =
        Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).expect("static header");
    let _ = req.respond(
        Response::from_string(body)
            .with_status_code(code)
            .with_header(header),
    );
}

pub fn handle(app: &App, mut req: Request, path: &str, query: &str) {
    let expected = format!("Bearer {}", app.cfg.bearer_token);
    let authed = req
        .headers()
        .iter()
        .any(|h| h.field.equiv("Authorization") && h.value.as_str() == expected);
    if !authed {
        json_response(req, 401, "{\"error\":\"unauthorized\"}".to_string());
        return;
    }

    match (req.method().clone(), path) {
        (Method::Get, "/api/health") => {
            let active = {
                let reg = app.subs.lock().unwrap();
                reg.subs.values().filter(|s| s.state == "active").count()
            };
            let max_seq = app.store.lock().unwrap().max_seq();
            json_response(
                req,
                200,
                format!(
                    "{{\"ok\":true,\"subs_active\":{},\"max_seq\":{},\"now\":{}}}",
                    active,
                    max_seq,
                    now_unix()
                ),
            );
        }

        (Method::Get, "/api/events") => {
            let after: u64 = query_get(query, "after")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let max: usize = query_get(query, "max")
                .and_then(|s| s.parse().ok())
                .unwrap_or(500)
                .min(2000);
            let (events, max_seq) = {
                let store = app.store.lock().unwrap();
                (store.events_after(after, max), store.max_seq())
            };
            let mut out = String::from("{\"events\":[");
            for (i, e) in events.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&format!(
                    "{{\"seq\":{},\"received_at\":{},\"kind\":{},\"channel_id\":{},\"video_id\":{},\"ts\":{},\"title\":{}}}",
                    e.seq,
                    e.received_at,
                    json_string(&e.kind),
                    json_string(&e.channel_id),
                    json_string(&e.video_id),
                    json_string(&e.ts),
                    json_string(&e.title)
                ));
            }
            out.push_str(&format!("],\"max_seq\":{}}}", max_seq));
            json_response(req, 200, out);
        }

        (Method::Post, "/api/ack") => {
            let body = read_body(&mut req);
            let through = extract_u64(&body, "through").unwrap_or(0);
            app.store.lock().unwrap().set_ack(through);
            json_response(req, 200, "{\"ok\":true}".to_string());
        }

        (Method::Post, "/api/channels") => {
            let body = read_body(&mut req);
            let channels = extract_string_array(&body);
            // Persist to the channels file (single source of truth) then reconcile.
            let content = channels.join("\n") + "\n";
            if let Err(e) = std::fs::write(&app.cfg.channels_file, content) {
                json_response(
                    req,
                    500,
                    format!("{{\"error\":{}}}", json_string(&e.to_string())),
                );
                return;
            }
            let (subscribed, unsubscribed, active) = crate::renew::reconcile(app);
            json_response(
                req,
                200,
                format!(
                    "{{\"subscribed\":{},\"unsubscribed\":{},\"active\":{}}}",
                    subscribed, unsubscribed, active
                ),
            );
        }

        _ => json_response(req, 404, "{\"error\":\"not found\"}".to_string()),
    }
}

fn read_body(req: &mut Request) -> String {
    let mut body = Vec::new();
    let _ = req.as_reader().take(MAX_BODY).read_to_end(&mut body);
    String::from_utf8_lossy(&body).into_owned()
}

fn extract_u64(text: &str, key: &str) -> Option<u64> {
    let marker = format!("\"{}\"", key);
    let pos = text.find(&marker)? + marker.len();
    let rest = text[pos..].trim_start().strip_prefix(':')?.trim_start();
    let num: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    num.parse().ok()
}

/// Extract every JSON string literal inside the first `[...]` array.
fn extract_string_array(text: &str) -> Vec<String> {
    let start = match text.find('[') {
        Some(p) => p,
        None => return Vec::new(),
    };
    let rest = &text[start + 1..];
    let end = rest.find(']').unwrap_or(rest.len());
    let inner = &rest[..end];

    let chars: Vec<char> = inner.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '"' {
            i += 1;
            let mut s = String::new();
            while i < chars.len() {
                match chars[i] {
                    '\\' => {
                        i += 1;
                        if i < chars.len() {
                            s.push(match chars[i] {
                                'n' => '\n',
                                't' => '\t',
                                'r' => '\r',
                                other => other,
                            });
                            i += 1;
                        }
                    }
                    '"' => {
                        i += 1;
                        break;
                    }
                    other => {
                        s.push(other);
                        i += 1;
                    }
                }
            }
            let s = s.trim().to_string();
            if !s.is_empty() {
                out.push(s);
            }
        } else {
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_channels_array() {
        let body = r#"{"channels":["UCaaaaaaaaaaaaaaaaaaaaaa","@handle"," UCbbbbbbbbbbbbbbbbbbbbbb "]}"#;
        let v = extract_string_array(body);
        assert_eq!(v, vec!["UCaaaaaaaaaaaaaaaaaaaaaa", "@handle", "UCbbbbbbbbbbbbbbbbbbbbbb"]);
    }

    #[test]
    fn parses_through_number() {
        assert_eq!(extract_u64(r#"{"through": 42}"#, "through"), Some(42));
        assert_eq!(extract_u64(r#"{"through":7,"x":1}"#, "through"), Some(7));
        assert_eq!(extract_u64(r#"{"x":1}"#, "through"), None);
    }
}
