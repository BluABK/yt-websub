//! Small dependency-free helpers: time, hex, OS randomness, percent-encoding
//! (for TSV-safe storage), JSON string escaping, and query parsing.

use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

const HEX: &[u8; 16] = b"0123456789abcdef";

pub fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

pub fn hex_decode(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if s.len() % 2 != 0 {
        return None;
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len() / 2);
    let mut i = 0;
    while i < b.len() {
        out.push((hex_val(b[i])? << 4) | hex_val(b[i + 1])?);
        i += 2;
    }
    Some(out)
}

/// Hex string of `n` random bytes drawn from the OS RNG.
pub fn rand_hex(n: usize) -> String {
    let mut buf = vec![0u8; n];
    getrandom::getrandom(&mut buf).expect("OS RNG failed");
    hex_encode(&buf)
}

/// Percent-encode only structural/control bytes so a string is safe to store as
/// one TSV field. Multi-byte UTF-8 passes through unchanged (its bytes are all
/// >= 0x80 and never collide with the escaped set), so titles stay readable.
pub fn pct_encode(s: &str) -> String {
    let mut out: Vec<u8> = Vec::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b < 0x20 || b == b'%' || b == 0x7f {
            out.push(b'%');
            out.push(b"0123456789ABCDEF"[(b >> 4) as usize]);
            out.push(b"0123456789ABCDEF"[(b & 0x0f) as usize]);
        } else {
            out.push(b);
        }
    }
    String::from_utf8(out).unwrap_or_default()
}

pub fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(hi), Some(lo)) = (hex_val(b[i + 1]), hex_val(b[i + 2])) {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Decode an application/x-www-form-urlencoded or query-string component.
pub fn url_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                if let (Some(hi), Some(lo)) = (hex_val(b[i + 1]), hex_val(b[i + 2])) {
                    out.push((hi << 4) | lo);
                    i += 3;
                    continue;
                }
                out.push(b[i]);
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// First value for `key` in a `&`-separated query string (keys/values decoded).
pub fn query_get(query: &str, key: &str) -> Option<String> {
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let mut it = pair.splitn(2, '=');
        let k = it.next().unwrap_or("");
        if url_decode(k) == key {
            return Some(url_decode(it.next().unwrap_or("")));
        }
    }
    None
}

/// Constant-time byte-equality, so comparing the bearer token doesn't leak its
/// length/contents through timing. (HMAC verification already uses a CT compare.)
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Render `s` as a quoted, escaped JSON string (including the surrounding quotes).
pub fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_roundtrip() {
        let b = [0u8, 1, 15, 16, 255, 128];
        assert_eq!(hex_decode(&hex_encode(&b)).unwrap(), b);
        assert_eq!(hex_encode(&[0xde, 0xad]), "dead");
        assert!(hex_decode("xyz").is_none());
        assert!(hex_decode("abc").is_none()); // odd length
    }

    #[test]
    fn pct_roundtrip_preserves_utf8_and_escapes_tabs() {
        let s = "Hello\tWorld\n100% café 🎥";
        let enc = pct_encode(s);
        assert!(!enc.contains('\t'));
        assert!(!enc.contains('\n'));
        assert!(enc.contains("%25")); // the percent sign
        assert!(enc.contains("café")); // utf-8 preserved
        assert!(enc.contains("🎥"));
        assert_eq!(pct_decode(&enc), s);
    }

    #[test]
    fn query_parsing() {
        let q = "hub.mode=subscribe&hub.topic=https%3A%2F%2Fx%2Fy%3Fchannel_id%3DUC1&hub.challenge=abc+def";
        assert_eq!(query_get(q, "hub.mode").unwrap(), "subscribe");
        assert_eq!(query_get(q, "hub.topic").unwrap(), "https://x/y?channel_id=UC1");
        assert_eq!(query_get(q, "hub.challenge").unwrap(), "abc def");
        assert!(query_get(q, "missing").is_none());
    }

    #[test]
    fn json_escaping() {
        assert_eq!(json_string("a\"b\\c"), "\"a\\\"b\\\\c\"");
        assert_eq!(json_string("line\nbreak"), "\"line\\nbreak\"");
    }

    #[test]
    fn ct_eq_matches_semantics() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"abcd"));
        assert!(ct_eq(b"", b""));
    }
}
