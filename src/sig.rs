//! `X-Hub-Signature` verification. Google's hub signs the notification body with
//! HMAC-SHA1 keyed by the `hub.secret` we registered, and sends
//! `X-Hub-Signature: sha1=<40-hex>`. The key is the raw bytes of the secret
//! string we supplied (an opaque value), not a decoding of it.

use hmac::{Hmac, Mac};
use sha1::Sha1;

use crate::util::hex_decode;

type HmacSha1 = Hmac<Sha1>;

/// Constant-time verify of a `sha1=<hex>` header against HMAC-SHA1(secret, body).
pub fn verify(secret: &[u8], body: &[u8], header: &str) -> bool {
    let hex = match header.trim().strip_prefix("sha1=") {
        Some(h) => h.trim(),
        None => return false,
    };
    let expected = match hex_decode(hex) {
        Some(b) => b,
        None => return false,
    };
    let mut mac = match HmacSha1::new_from_slice(secret) {
        Ok(m) => m,
        Err(_) => return false,
    };
    mac.update(body);
    mac.verify_slice(&expected).is_ok()
}

/// Compute the `sha1=<hex>` value the hub would send (used by tests).
#[cfg(test)]
pub fn sign(secret: &[u8], body: &[u8]) -> String {
    use crate::util::hex_encode;
    let mut mac = HmacSha1::new_from_slice(secret).unwrap();
    mac.update(body);
    format!("sha1={}", hex_encode(&mac.finalize().into_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vector() {
        // RFC 2202 HMAC-SHA1 test case 2: key="Jefe", data="what do ya want for nothing?"
        let sig = sign(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(sig, "sha1=effcdf6ae5eb2fa2d27416d5f184df9c259a7c79");
    }

    #[test]
    fn roundtrip_and_rejects_tampering() {
        let secret = b"my-per-sub-secret";
        let body = b"<feed>...</feed>";
        let header = sign(secret, body);
        assert!(verify(secret, body, &header));
        assert!(!verify(secret, b"<feed>tampered</feed>", &header));
        assert!(!verify(b"wrong-secret", body, &header));
        assert!(!verify(secret, body, "sha1=deadbeef"));
        assert!(!verify(secret, body, "no-prefix"));
    }
}
