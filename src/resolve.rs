//! Resolve a `channels.txt` entry (a `UC...` id, an `@handle`, or a channel URL)
//! to a canonical `UCxxxx` channel id. UC ids and `/channel/UC..` URLs are
//! extracted directly; handles are resolved by fetching the channel page and
//! scraping the canonical id. Results are cached by the caller.

use std::collections::HashMap;
use std::io::Read;
use std::time::Duration;

const UA: &str =
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36";

/// Cap on the resolver's outbound response body, so a hostile/oversized page
/// can't drive memory toward the process MemoryMax ceiling.
const MAX_RESPONSE_BYTES: u64 = 4 * 1024 * 1024;

/// The only hosts the resolver is ever allowed to fetch. Everything else — a
/// channels.txt entry pointing at http://internal, a link-local/metadata address,
/// or any non-YouTube host — is refused. This is the primary SSRF control.
const ALLOWED_HOSTS: [&str; 3] = ["www.youtube.com", "youtube.com", "m.youtube.com"];

/// True only for `https://<allowed-youtube-host>[:port][/...]` URLs. Rejects any
/// non-https scheme, embedded `userinfo@` credentials, and any other host
/// (including `www.youtube.com.evil.com` and `...@evil.com` tricks).
fn is_allowed_url(url: &str) -> bool {
    let rest = match url.strip_prefix("https://") {
        Some(r) => r,
        None => return false,
    };
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    if authority.contains('@') {
        return false; // reject userinfo (e.g. youtube.com@evil.com)
    }
    let host = authority.split(':').next().unwrap_or(authority);
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    ALLOWED_HOSTS.contains(&host.as_str())
}

pub fn is_channel_id(s: &str) -> bool {
    s.len() == 24
        && s.starts_with("UC")
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Find a 24-char `UC...` id following any known marker in arbitrary text/HTML.
/// Markers are tried in priority order: the page's own canonical/og:url link and
/// `externalId` (authoritative for the channel the page belongs to) before the
/// generic `/channel/UC`, which could otherwise first match a *linked* channel.
pub fn extract_uc(text: &str) -> Option<String> {
    for marker in [
        "rel=\"canonical\" href=\"https://www.youtube.com/channel/UC",
        "property=\"og:url\" content=\"https://www.youtube.com/channel/UC",
        "\"externalId\":\"UC",
        "\"channelId\":\"UC",
        "/channel/UC",
        "channel_id=UC",
    ] {
        if let Some(pos) = text.find(marker) {
            let start = pos + marker.len() - 2; // include "UC"
            let cand: String = text[start..].chars().take(24).collect();
            if is_channel_id(&cand) {
                return Some(cand);
            }
        }
    }
    None
}

fn normalize_url(s: &str) -> String {
    let s = s.trim();
    if s.starts_with("http://") || s.starts_with("https://") {
        return s.to_string();
    }
    if let Some(rest) = s.strip_prefix('@') {
        return format!("https://www.youtube.com/@{}", rest);
    }
    if s.starts_with("youtube.com") || s.starts_with("www.youtube.com") {
        return format!("https://{}", s);
    }
    format!("https://www.youtube.com/@{}", s)
}

/// Resolve `input` to a `UCxxxx` id, consulting/updating `cache`.
pub fn resolve(input: &str, cache: &mut HashMap<String, String>) -> Option<String> {
    let s = input.trim();
    if s.is_empty() {
        return None;
    }
    if is_channel_id(s) {
        return Some(s.to_string());
    }
    if let Some(uc) = extract_uc(s) {
        return Some(uc);
    }
    if let Some(uc) = cache.get(s) {
        // Only trust a cached value that is still a well-formed channel id.
        if is_channel_id(uc) {
            return Some(uc.clone());
        }
    }
    let url = normalize_url(s);
    // SSRF guard: only ever fetch https youtube.com URLs. A channels.txt entry
    // (settable by an authenticated API client) that is a full http://internal or
    // link-local/metadata URL must never be fetched from inside the trust boundary.
    if !is_allowed_url(&url) {
        eprintln!("[resolve] refusing to fetch non-YouTube URL for entry {:?}", s);
        return None;
    }
    let agent = ureq::builder()
        .timeout(Duration::from_secs(20))
        .redirects(0) // never follow a redirect off the validated host
        .build();
    let resp = agent.get(&url).set("User-Agent", UA).call().ok()?;
    // Cap the body regardless of ureq's default limit.
    let mut buf = Vec::new();
    resp.into_reader()
        .take(MAX_RESPONSE_BYTES)
        .read_to_end(&mut buf)
        .ok()?;
    let body = String::from_utf8_lossy(&buf).into_owned();
    let uc = extract_uc(&body)?;
    cache.insert(s.to_string(), uc.clone());
    Some(uc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_uc_ids() {
        assert!(is_channel_id("UCabcdefghijklmnopqrstuv"));
        assert!(!is_channel_id("UCtooshort"));
        assert!(!is_channel_id("notachannelid000000000000"));
    }

    #[test]
    fn extracts_from_markers() {
        assert_eq!(
            extract_uc("foo /channel/UCabcdefghijklmnopqrstuv/live bar"),
            Some("UCabcdefghijklmnopqrstuv".to_string())
        );
        assert_eq!(
            extract_uc("\"channelId\":\"UCabcdefghijklmnopqrstuv\""),
            Some("UCabcdefghijklmnopqrstuv".to_string())
        );
        assert_eq!(extract_uc("nothing here"), None);
    }

    #[test]
    fn ssrf_url_allowlist() {
        // Allowed: https youtube hosts.
        assert!(is_allowed_url("https://www.youtube.com/@handle"));
        assert!(is_allowed_url(
            "https://youtube.com/channel/UCabcdefghijklmnopqrstuv"
        ));
        assert!(is_allowed_url("https://m.youtube.com/@x"));
        assert!(is_allowed_url("https://www.youtube.com:443/@x"));

        // Rejected: non-https scheme.
        assert!(!is_allowed_url("http://www.youtube.com/@x"));
        // Rejected: internal / link-local / metadata targets.
        assert!(!is_allowed_url("https://169.254.169.254/latest/meta-data/"));
        assert!(!is_allowed_url("https://127.0.0.1:6379/"));
        assert!(!is_allowed_url("https://localhost/"));
        // Rejected: suffix-domain and userinfo tricks.
        assert!(!is_allowed_url("https://www.youtube.com.evil.com/"));
        assert!(!is_allowed_url("https://www.youtube.com@evil.com/"));
        assert!(!is_allowed_url("https://evil.com/"));
    }
}
