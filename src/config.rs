//! Hand-parsed `KEY=VALUE` configuration (no serde). Loaded from a file
//! (`$YTWEBSUB_CONFIG`, default `/etc/yt-websub.env`) with process-env overlay.

use std::collections::HashMap;
use std::env;
use std::fs;

#[derive(Clone)]
pub struct Config {
    pub callback_base: String, // e.g. https://hooks.example.com
    pub listen: String,        // e.g. 0.0.0.0:443
    pub tls_cert: String,      // PEM fullchain path
    pub tls_key: String,       // PEM private key path
    pub bearer_token: String,  // /api auth
    pub channels_file: String, // operator-managed desired channel list
    pub storage_dir: String,   // events.log, subs.tsv, etc.
    pub lease_seconds: u64,
    pub hub_url: String,
    pub accept_threads: usize,
}

/// Parse `KEY=VALUE` lines: ignores blanks and `#` comments, trims whitespace,
/// and strips one layer of matching surrounding quotes.
pub fn parse_env_file(content: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (k, v) = match line.split_once('=') {
            Some(x) => x,
            None => continue,
        };
        let k = k.trim().to_string();
        let mut v = v.trim();
        if v.len() >= 2
            && ((v.starts_with('"') && v.ends_with('"'))
                || (v.starts_with('\'') && v.ends_with('\'')))
        {
            v = &v[1..v.len() - 1];
        }
        out.push((k, v.to_string()));
    }
    out
}

impl Config {
    pub fn load() -> Result<Config, String> {
        let mut map: HashMap<String, String> = HashMap::new();
        let path = env::var("YTWEBSUB_CONFIG").unwrap_or_else(|_| "/etc/yt-websub.env".to_string());
        if let Ok(content) = fs::read_to_string(&path) {
            for (k, v) in parse_env_file(&content) {
                map.insert(k, v);
            }
        }
        // Process-env wins over the file.
        for (k, v) in env::vars() {
            if k.starts_with("YTWEBSUB_") {
                map.insert(k, v);
            }
        }
        Config::from_map(&map)
    }

    pub fn from_map(map: &HashMap<String, String>) -> Result<Config, String> {
        let get = |k: &str| map.get(k).cloned();
        let req = |k: &str| -> Result<String, String> {
            map.get(k)
                .cloned()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| format!("missing required config: {}", k))
        };

        let callback_base = req("YTWEBSUB_CALLBACK_BASE")?
            .trim_end_matches('/')
            .to_string();
        let bearer_token = req("YTWEBSUB_BEARER_TOKEN")?;
        // Fail closed on the shipped placeholder or an obviously weak token: the
        // /api surface is internet-exposed, and the example value is public in this
        // repo. Refuse to start rather than silently run with no real auth.
        if bearer_token.starts_with("CHANGE_ME") || bearer_token.len() < 32 {
            return Err(
                "YTWEBSUB_BEARER_TOKEN looks unset or too weak; set a random token of at \
                 least 32 characters (e.g. `openssl rand -hex 32`)"
                    .to_string(),
            );
        }
        let tls_cert = req("YTWEBSUB_TLS_CERT")?;
        let tls_key = req("YTWEBSUB_TLS_KEY")?;
        let storage_dir =
            get("YTWEBSUB_STORAGE_DIR").unwrap_or_else(|| "/var/lib/yt-websub".to_string());
        let channels_file = get("YTWEBSUB_CHANNELS_FILE")
            .unwrap_or_else(|| format!("{}/channels.txt", storage_dir));
        let listen = get("YTWEBSUB_LISTEN").unwrap_or_else(|| "0.0.0.0:443".to_string());
        let lease_seconds = get("YTWEBSUB_LEASE_SECONDS")
            .and_then(|s| s.parse().ok())
            .unwrap_or(432_000);
        let hub_url = get("YTWEBSUB_HUB_URL")
            .unwrap_or_else(|| "https://pubsubhubbub.appspot.com/subscribe".to_string());
        let accept_threads = get("YTWEBSUB_ACCEPT_THREADS")
            .and_then(|s| s.parse().ok())
            .filter(|&n: &usize| n >= 1)
            .unwrap_or(4);

        Ok(Config {
            callback_base,
            listen,
            tls_cert,
            tls_key,
            bearer_token,
            channels_file,
            storage_dir,
            lease_seconds,
            hub_url,
            accept_threads,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_strips_quotes_and_comments() {
        let content = "\
# a comment
YTWEBSUB_CALLBACK_BASE = https://hooks.example.com/
YTWEBSUB_BEARER_TOKEN=\"secret token\"

YTWEBSUB_LEASE_SECONDS = 600
not_a_kv_line
";
        let pairs = parse_env_file(content);
        let map: HashMap<String, String> = pairs.into_iter().collect();
        assert_eq!(map.get("YTWEBSUB_CALLBACK_BASE").unwrap(), "https://hooks.example.com/");
        assert_eq!(map.get("YTWEBSUB_BEARER_TOKEN").unwrap(), "secret token");
        assert_eq!(map.get("YTWEBSUB_LEASE_SECONDS").unwrap(), "600");
        assert!(!map.contains_key("not_a_kv_line"));
    }

    #[test]
    fn from_map_applies_defaults_and_requires_keys() {
        let mut m = HashMap::new();
        m.insert("YTWEBSUB_CALLBACK_BASE".into(), "https://h.example.com/".into());
        // 64-char token: must pass the placeholder/length check in from_map.
        m.insert(
            "YTWEBSUB_BEARER_TOKEN".into(),
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
        );
        m.insert("YTWEBSUB_TLS_CERT".into(), "/c.pem".into());
        m.insert("YTWEBSUB_TLS_KEY".into(), "/k.pem".into());
        let cfg = Config::from_map(&m).unwrap();
        assert_eq!(cfg.callback_base, "https://h.example.com"); // trailing slash stripped
        assert_eq!(cfg.listen, "0.0.0.0:443");
        assert_eq!(cfg.lease_seconds, 432_000);
        assert_eq!(cfg.accept_threads, 4);
        assert_eq!(cfg.channels_file, "/var/lib/yt-websub/channels.txt");

        m.remove("YTWEBSUB_TLS_CERT");
        assert!(Config::from_map(&m).is_err());
    }

    #[test]
    fn rejects_placeholder_and_weak_bearer_token() {
        let mut m = HashMap::new();
        m.insert("YTWEBSUB_CALLBACK_BASE".into(), "https://h.example.com".into());
        m.insert("YTWEBSUB_TLS_CERT".into(), "/c.pem".into());
        m.insert("YTWEBSUB_TLS_KEY".into(), "/k.pem".into());

        // The shipped placeholder must be refused.
        m.insert("YTWEBSUB_BEARER_TOKEN".into(), "CHANGE_ME_long_random_token".into());
        assert!(Config::from_map(&m).is_err());

        // Too short must be refused.
        m.insert("YTWEBSUB_BEARER_TOKEN".into(), "deadbeef".into());
        assert!(Config::from_map(&m).is_err());

        // A proper 48-char random-looking token is accepted.
        m.insert(
            "YTWEBSUB_BEARER_TOKEN".into(),
            "0123456789abcdef0123456789abcdef0123456789abcdef".into(),
        );
        assert!(Config::from_map(&m).is_ok());
    }
}
