//! Shared application state, behind `Arc`, with fine-grained mutexes held only
//! briefly (network I/O always happens outside the locks).

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
