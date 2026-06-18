//! Outbound subscribe/unsubscribe to the WebSub hub over HTTPS (ureq + rustls).
//! Uses async verification: the hub replies 202 and then calls back our GET
//! verification endpoint, which flips the subscription to `active`.

use std::time::Duration;

use crate::config::Config;
use crate::subs::Sub;

/// Send a `subscribe` or `unsubscribe` request. Returns the HTTP status code,
/// or an error string on transport failure.
pub fn send(cfg: &Config, sub: &Sub, mode: &str) -> Result<u16, String> {
    let callback = format!("{}/yt/cb/{}", cfg.callback_base, sub.token);
    let lease = cfg.lease_seconds.to_string();
    let agent = ureq::builder().timeout(Duration::from_secs(20)).build();
    let resp = agent.post(&cfg.hub_url).send_form(&[
        ("hub.callback", callback.as_str()),
        ("hub.topic", sub.topic.as_str()),
        ("hub.verify", "async"),
        ("hub.mode", mode),
        ("hub.secret", sub.secret.as_str()),
        ("hub.lease_seconds", lease.as_str()),
        ("hub.verify_token", sub.token.as_str()),
    ]);
    match resp {
        Ok(r) => Ok(r.status()),
        Err(ureq::Error::Status(code, _)) => Ok(code),
        Err(e) => Err(e.to_string()),
    }
}

pub fn is_ok(code: u16) -> bool {
    (200..300).contains(&code)
}
