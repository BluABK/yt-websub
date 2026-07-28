//! yt-websub: a minimal, headless YouTube WebSub (PubSubHubbub) notification
//! server. One HTTPS listener serves both the public hub callback and the
//! token-authenticated control/poll API; a background thread keeps subscriptions
//! alive. See README.md and deploy/ for operation.

mod api;
mod app;
mod atom;
mod callback;
mod config;
mod http;
mod hub;
mod renew;
mod resolve;
mod sig;
mod store;
mod subs;
mod util;

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use app::App;

fn main() {
    let cfg = match config::Config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config error: {}", e);
            std::process::exit(2);
        }
    };

    let store = match store::Store::open(Path::new(&cfg.storage_dir)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("storage error ({}): {}", cfg.storage_dir, e);
            std::process::exit(2);
        }
    };
    let registry = subs::Registry::load(&Path::new(&cfg.storage_dir).join("subs.tsv"));

    let cert = match std::fs::read(&cfg.tls_cert) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("cannot read TLS cert {}: {}", cfg.tls_cert, e);
            std::process::exit(2);
        }
    };
    let key = match std::fs::read(&cfg.tls_key) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("cannot read TLS key {}: {}", cfg.tls_key, e);
            std::process::exit(2);
        }
    };

    let app = Arc::new(App {
        cfg: cfg.clone(),
        store: Mutex::new(store),
        subs: Mutex::new(registry),
        resolve_cache: Mutex::new(load_cache(&cfg.storage_dir)),
        reconcile_lock: Mutex::new(()),
        started_at: std::time::Instant::now(),
    });

    {
        let app = app.clone();
        std::thread::spawn(move || renew::run(app));
    }

    let ssl = tiny_http::SslConfig {
        certificate: cert,
        private_key: key,
    };
    let server = match tiny_http::Server::https(cfg.listen.as_str(), ssl) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            eprintln!("cannot listen on {}: {}", cfg.listen, e);
            std::process::exit(2);
        }
    };
    eprintln!(
        "[yt-websub] listening HTTPS on {} (callback base {})",
        cfg.listen, cfg.callback_base
    );

    let threads = cfg.accept_threads.max(1);
    let mut handles = Vec::new();
    for _ in 1..threads {
        let server = server.clone();
        let app = app.clone();
        handles.push(std::thread::spawn(move || worker(&server, &app)));
    }
    worker(&server, &app);
    for h in handles {
        let _ = h.join();
    }
}

fn worker(server: &tiny_http::Server, app: &App) {
    loop {
        match server.recv() {
            Ok(req) => http::handle(app, req),
            Err(e) => {
                // A failed accept on a bound listener is effectively fatal. Exit
                // so systemd restarts us cleanly rather than silently shedding an
                // accept thread (or hanging in join() while the others block).
                eprintln!("[server] fatal recv error: {}; exiting for restart", e);
                std::process::exit(1);
            }
        }
    }
}

fn load_cache(dir: &str) -> HashMap<String, String> {
    let mut m = HashMap::new();
    if let Ok(content) = std::fs::read_to_string(Path::new(dir).join("resolve.cache")) {
        for line in content.lines() {
            if let Some((k, v)) = line.split_once('\t') {
                m.insert(k.to_string(), v.to_string());
            }
        }
    }
    m
}
