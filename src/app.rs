//! Shared application state, behind `Arc`, with fine-grained mutexes held only
//! briefly (network I/O always happens outside the locks).
//!
//! LOCK POISONING: the code uses `.lock().unwrap()` throughout, which is safe
//! only because the release/dev profiles set `panic = "abort"` (see Cargo.toml):
//! the first panic aborts the whole process (systemd then restarts it) before any
//! thread can observe a poisoned mutex. If that profile setting is ever removed,
//! a single panic while a lock is held would poison it and cascade into a
//! half-dead server — so `panic = "abort"` is a load-bearing invariant here, not
//! just a size optimization.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::config::Config;
use crate::store::Store;
use crate::subs::Registry;

pub struct App {
    pub cfg: Config,
    pub store: Mutex<Store>,
    pub subs: Mutex<Registry>,
    pub resolve_cache: Mutex<HashMap<String, String>>,
    /// Serializes whole `reconcile` runs (API-driven vs. the renewal thread) so
    /// they can't both subscribe the same new channel concurrently.
    pub reconcile_lock: Mutex<()>,
}
