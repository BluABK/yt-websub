//! Resolve a `channels.txt` entry (a `UC...` id, an `@handle`, or a channel URL)
//! to a canonical `UCxxxx` channel id. UC ids and `/channel/UC..` URLs are
//! extracted directly; handles are resolved by fetching the channel page and
//! scraping the canonical id. Results are cached by the caller.

use std::collections::HashMap;
use std::time::Duration;

const UA: &str =
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36";

pub fn is_channel_id(s: &str) -> bool {
    s.len() == 24
        && s.starts_with("UC")
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Find a 24-char `UC...` id following any known marker in arbitrary text/HTML.
pub fn extract_uc(text: &str) -> Option<String> {
    for marker in [
        "/channel/UC",
        "\"channelId\":\"UC",
        "\"externalId\":\"UC",
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
        return Some(uc.clone());
    }
    let url = normalize_url(s);
    let agent = ureq::builder().timeout(Duration::from_secs(20)).build();
    let body = agent
        .get(&url)
        .set("User-Agent", UA)
        .call()
        .ok()?
        .into_string()
        .ok()?;
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
}
