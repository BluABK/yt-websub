//! Garbage collection for this build's Cargo profile directory
//! (`target/<profile>/`), run from the build script so the build itself keeps
//! its cache from growing without bound. Call [`run_from_build_script`] from
//! `build.rs`.
//!
//! Cargo never removes anything from `target/`. Three kinds of leftovers pile
//! up, and each is removed here only under a rule that holds **while a build is
//! running** — the build script executes in the middle of one, after Cargo has
//! decided which cached units it will reuse, so deleting one of those would
//! break the build in progress:
//!
//! 1. **Incremental caches** (`incremental/<crate>-<hash>/`). Every flavour of
//!    this crate (build, test, clippy, each example) keeps its own, forever.
//!    One that was last written more than `incremental_slack` before the newest
//!    is removed. The rule is relative to the newest cache, not to the clock,
//!    so returning to the project after a month deletes nothing that the
//!    flavours in regular use still hold. Safe mid-build: only this package is
//!    compiled incrementally, and all of its compilations wait for this build
//!    script; rustc recreates a missing cache directory.
//!
//! 2. **Dependency builds for versions no longer in `Cargo.lock`** — the
//!    compiled leftovers of every version bump. A unit's package version is
//!    read from the compiler's dep-info (`.d`), whose source paths run through
//!    `registry/src/<index>/<package>-<version>/`; a build-script run, which has
//!    no dep-info of its own, takes the version of the build script it runs
//!    (found through the fingerprint's dependency hashes). Safe mid-build: the
//!    lock file is what this build resolved against, so a version absent from
//!    it cannot be part of the build. Units whose version can't be determined
//!    (path and git dependencies, or anything unexpected) are kept. Feature or
//!    profile variants of a version that IS locked are kept too — telling a
//!    stale variant from one the running build is about to reuse would need
//!    Cargo's own unit graph, which a build script can't see.
//!
//! 3. **Outputs of superseded builds of this package** — the test binary and
//!    executable of every earlier configuration (hundreds of MB each, with
//!    their debug info). For each kind of target (`bin-…`, `test-bin-…`,
//!    `example-…`) and build profile, everything but the most recently built
//!    is removed. **Opt-in** (`local_always_rebuilt`): it is only safe when
//!    every target of this package is recompiled on every run of the build
//!    script — true when the script's output changes each time (a build
//!    counter, say), since every target depends on it. Otherwise the build in
//!    progress may be about to reuse one of those outputs as it stands. Only
//!    the output FILES go; the fingerprint folders stay, because Cargo may be
//!    about to write into the one belonging to the unit it is rebuilding.
//!
//! The files of a unit are found by the 16-hex-digit hash Cargo puts in both
//! the fingerprint folder name (`.fingerprint/<package>-<hash>/`) and every
//! output name (`deps/lib<crate>-<hash>.rlib`, `build/<package>-<hash>/`).
//!
//! Shared between `build.rs` (which runs it) and the crate's test build (which
//! tests it); nothing here is part of the shipped program. It needs no crates
//! and builds on edition 2021, so the same file serves every project that
//! carries a copy — keep the copies identical.
//!
//! `TARGET_GC` steers it at build time: `off` (or `0`) never runs it, `dry`
//! lists what would go as build warnings and deletes nothing, `force` runs it
//! regardless of when it last ran. Otherwise it runs at most once per
//! [`RUN_EVERY`] per profile, and reports only when it freed something.

// Build-time code: plain std::fs, outside the app's I/O-monitor facade.
#![allow(clippy::disallowed_methods)]

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// What one pass decided to delete.
#[derive(Debug, Default)]
pub struct Plan {
    /// Whole directories: stale incremental caches, and the fingerprint and
    /// build-script folders of dependency versions no longer locked.
    pub dirs: Vec<PathBuf>,
    /// Single files: compiled outputs in `deps/` and `examples/`.
    pub files: Vec<PathBuf>,
    pub incremental: usize,
    pub stale_versions: usize,
    pub superseded_local: usize,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.dirs.is_empty() && self.files.is_empty()
    }

    /// `2 incremental caches, 5 stale dependency builds, 1 old app build`.
    pub fn summary(&self) -> String {
        format!(
            "{} incremental caches, {} stale dependency builds, {} old builds of this package",
            self.incremental, self.stale_versions, self.superseded_local
        )
    }

    /// Bytes the plan would free (sizes on disk now).
    pub fn bytes(&self) -> u64 {
        self.dirs.iter().chain(&self.files).map(|p| size_of(p)).sum()
    }

    /// Delete everything in the plan. Returns the bytes freed and how many
    /// entries could not be removed (a running program's executable, say) —
    /// those are skipped, never retried in a loop, and never fatal.
    pub fn execute(&self) -> (u64, usize) {
        let mut freed = 0;
        let mut failed = 0;
        for f in &self.files {
            let size = size_of(f);
            match fs::remove_file(f) {
                Ok(()) => freed += size,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => failed += 1,
            }
        }
        for d in &self.dirs {
            let size = size_of(d);
            match fs::remove_dir_all(d) {
                Ok(()) => freed += size,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => {
                    // Partly removed is still partly freed.
                    freed += size.saturating_sub(size_of(d));
                    failed += 1;
                }
            }
        }
        (freed, failed)
    }
}

/// Minimum time between two passes over the same profile folder.
pub const RUN_EVERY: Duration = Duration::from_secs(6 * 3600);

/// How far behind the newest incremental cache another may fall.
pub const INCREMENTAL_SLACK: Duration = Duration::from_secs(7 * 86400);

/// Run a pass from inside `build.rs`, reading everything it needs from the
/// environment Cargo gives a build script.
///
/// `local_always_rebuilt` enables rule 3 (see the module doc): pass `true`
/// only when this build script's output changes on every run.
///
/// Prints nothing but `cargo:warning` lines, and no `rerun-if` directives: a
/// build script that prints none is re-run whenever a file of the package
/// changes, and printing one here would silently narrow that for callers that
/// rely on it.
#[cfg_attr(test, allow(dead_code))]
pub fn run_from_build_script(local_always_rebuilt: bool) {
    let mode = std::env::var("TARGET_GC").unwrap_or_default().to_ascii_lowercase();
    if mode == "off" || mode == "0" {
        return;
    }
    let dry = mode == "dry";
    let force = mode == "force";

    // OUT_DIR is `<profile>/build/<package>-<hash>/out`.
    let out = PathBuf::from(std::env::var_os("OUT_DIR").unwrap_or_default());
    let Some(profile_dir) = out.ancestors().nth(3) else { return };
    // Refuse anything that doesn't look like a Cargo profile folder, so an
    // unusual OUT_DIR can never aim the deleter somewhere else.
    if !profile_dir.join(".fingerprint").is_dir() || !profile_dir.join("deps").is_dir() {
        return;
    }
    let running_unit = out
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .and_then(|n| split_hash(n).map(|(_, h)| h.to_string()));

    let stamp = profile_dir.join(".target-gc-stamp");
    let recent = fs::metadata(&stamp)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age < RUN_EVERY);
    if recent && !force && !dry {
        return;
    }

    let pkg = std::env::var("CARGO_PKG_NAME").unwrap_or_default();
    if pkg.is_empty() {
        return;
    }
    // The lock sits beside the manifest, or at a workspace root above it.
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap_or_default());
    let lock = manifest.ancestors().find_map(|d| fs::read_to_string(d.join("Cargo.lock")).ok());

    let plan = plan(&Inputs {
        profile_dir,
        root_pkg: &pkg,
        lock: lock.as_deref(),
        running_unit: running_unit.as_deref(),
        incremental_slack: INCREMENTAL_SLACK,
        local_always_rebuilt,
    });

    if dry {
        println!(
            "cargo:warning=target GC (dry run): would free {} - {}",
            human_bytes(plan.bytes()),
            plan.summary()
        );
        for p in plan.dirs.iter().chain(&plan.files) {
            println!("cargo:warning=  {}", p.display());
        }
        return;
    }

    let (freed, failed) = if plan.is_empty() { (0, 0) } else { plan.execute() };
    let _ = fs::write(&stamp, "");
    if freed > 0 || failed > 0 {
        let skipped = if failed > 0 {
            format!("; {failed} entries in use were skipped")
        } else {
            String::new()
        };
        println!(
            "cargo:warning=target GC freed {} ({}){skipped}",
            human_bytes(freed),
            plan.summary()
        );
    }
}

/// `1.2 GB` / `340 MB`.
pub fn human_bytes(b: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = MB * 1024.0;
    if b as f64 >= GB {
        format!("{:.1} GB", b as f64 / GB)
    } else {
        format!("{:.0} MB", b as f64 / MB)
    }
}

/// What to collect, and what must never be touched.
pub struct Inputs<'a> {
    /// `target/<profile>` (or `target/<triple>/<profile>`).
    pub profile_dir: &'a Path,
    /// This package's name, as it appears in fingerprint folder names.
    pub root_pkg: &'a str,
    /// `Cargo.lock` text. `None`, or text with no packages in it, disables the
    /// stale-version rule rather than treating every dependency as stale.
    pub lock: Option<&'a str>,
    /// The hash of the build-script run executing right now (its `OUT_DIR`
    /// lives in `build/<package>-<hash>/`). Never touched, whatever the rules say.
    pub running_unit: Option<&'a str>,
    /// How far behind the newest incremental cache another may fall before it
    /// is removed.
    pub incremental_slack: Duration,
    /// Whether every target of this package is recompiled on every build —
    /// the precondition for rule 3 (see the module doc).
    pub local_always_rebuilt: bool,
}

/// Decide what to delete. Reads the profile directory; changes nothing.
pub fn plan(inp: &Inputs) -> Plan {
    let mut plan = Plan::default();
    plan_incremental(inp, &mut plan);

    let units = read_units(&inp.profile_dir.join(".fingerprint"));
    if units.is_empty() {
        return plan;
    }
    let outputs = index_outputs(inp.profile_dir);

    // Stale dependency versions.
    let locked = inp.lock.map(parse_lock).unwrap_or_default();
    if !locked.is_empty() {
        let versions = unit_versions(inp, &units, &outputs);
        for u in units.values() {
            if u.pkg == inp.root_pkg || Some(u.hash.as_str()) == inp.running_unit {
                continue;
            }
            let Some(ver) = versions.get(&u.hash) else { continue };
            if locked.get(&u.pkg).is_some_and(|vs| vs.contains(ver)) {
                continue;
            }
            plan.stale_versions += 1;
            plan.dirs.push(u.dir.clone());
            if let Some(o) = outputs.get(&u.hash) {
                plan.files.extend(o.files.iter().cloned());
                plan.dirs.extend(o.dirs.iter().cloned());
            }
        }
    }

    if !inp.local_always_rebuilt {
        plan.files.sort();
        plan.files.dedup();
        plan.dirs.sort();
        plan.dirs.dedup();
        return plan;
    }

    // Superseded builds of this package: the newest of each target kind, per
    // build profile (check vs real build), survives.
    let key = |u: &Unit, stem: &str| (stem.to_string(), u.profiles.get(stem).copied());
    let mut newest: HashMap<(String, Option<u64>), (SystemTime, &str)> = HashMap::new();
    for u in units.values().filter(|u| u.pkg == inp.root_pkg) {
        for (stem, built) in &u.targets {
            if local_target_name(stem).is_none() {
                continue;
            }
            let e = newest.entry(key(u, stem)).or_insert((*built, u.hash.as_str()));
            if *built > e.0 {
                *e = (*built, u.hash.as_str());
            }
        }
    }
    for u in units.values().filter(|u| u.pkg == inp.root_pkg) {
        for (stem, _) in &u.targets {
            let Some(name) = local_target_name(stem) else { continue };
            if newest.get(&key(u, stem)).is_some_and(|(_, h)| *h == u.hash) {
                continue;
            }
            let Some(o) = outputs.get(&u.hash) else { continue };
            // One fingerprint folder can hold several targets (a bin and the
            // examples built alongside it): only this target's files go.
            let alt = name.replace('-', "_");
            let before = plan.files.len();
            plan.files.extend(
                o.files
                    .iter()
                    .filter(|f| {
                        f.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                            n.starts_with(&format!("{name}-")) || n.starts_with(&format!("{alt}-"))
                        })
                    })
                    .cloned(),
            );
            if plan.files.len() > before {
                plan.superseded_local += 1;
            }
        }
    }

    plan.files.sort();
    plan.files.dedup();
    plan.dirs.sort();
    plan.dirs.dedup();
    plan
}

fn plan_incremental(inp: &Inputs, plan: &mut Plan) {
    let Ok(rd) = fs::read_dir(inp.profile_dir.join("incremental")) else { return };
    let dirs: Vec<(PathBuf, SystemTime)> = rd
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| Some((e.path(), e.metadata().ok()?.modified().ok()?)))
        .collect();
    let Some(newest) = dirs.iter().map(|(_, t)| *t).max() else { return };
    for (dir, t) in dirs {
        if t + inp.incremental_slack < newest {
            plan.incremental += 1;
            plan.dirs.push(dir);
        }
    }
}

/// One `.fingerprint/<package>-<hash>/` folder.
struct Unit {
    dir: PathBuf,
    pkg: String,
    hash: String,
    /// Each target recorded in the folder (`bin-streamarchiver`, …) with the
    /// time it was last built — its JSON's write time.
    targets: Vec<(String, SystemTime)>,
    /// Per target, Cargo's hash of the profile it was built with. A check
    /// (clippy) and a real build of the same target differ here, so a later
    /// clippy run doesn't make the real test binary look superseded.
    profiles: HashMap<String, u64>,
    /// Fingerprints of the units this one depends on, as Cargo's hash files
    /// spell them (16 hex digits, little-endian).
    deps: Vec<String>,
}

/// Every unit, keyed by its folder hash.
fn read_units(fingerprint_dir: &Path) -> HashMap<String, Unit> {
    let mut units = HashMap::new();
    let Ok(rd) = fs::read_dir(fingerprint_dir) else { return units };
    for e in rd.flatten() {
        let Some(name) = e.file_name().to_str().map(str::to_string) else { continue };
        let Some((pkg, hash)) = split_hash(&name) else { continue };
        let mut unit = Unit {
            dir: e.path(),
            pkg: pkg.to_string(),
            hash: hash.to_string(),
            targets: Vec::new(),
            profiles: HashMap::new(),
            deps: Vec::new(),
        };
        let Ok(files) = fs::read_dir(e.path()) else { continue };
        for f in files.flatten() {
            let path = f.path();
            if path.extension().and_then(|x| x.to_str()) != Some("json") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else { continue };
            let Ok(built) = f.metadata().and_then(|m| m.modified()) else { continue };
            unit.targets.push((stem.to_string(), built));
            let Ok(text) = fs::read_to_string(&path) else { continue };
            let (profile, deps) = read_fingerprint_json(&text);
            if let Some(profile) = profile {
                unit.profiles.insert(stem.to_string(), profile);
            }
            unit.deps.extend(deps.into_iter().map(fingerprint_hex));
        }
        units.insert(unit.hash.clone(), unit);
    }
    units
}

/// The `"profile"` number and the dependency fingerprints from one of Cargo's
/// fingerprint JSON files: `{…,"profile":123,…,"deps":[[1,"name",false,456],…],…}`.
///
/// A small reader rather than a JSON crate, so a build script using this needs
/// no dependencies. Both fields have a fixed, flat shape — the entries of
/// `deps` are `[pkg_id, crate name, public, fingerprint]`, and a crate name
/// can contain neither `,` nor `]`. Anything unexpected reads as absent, which
/// only makes the collector keep more.
pub fn read_fingerprint_json(text: &str) -> (Option<u64>, Vec<u64>) {
    let profile = number_after(text, "\"profile\":");
    let mut deps = Vec::new();
    if let Some(i) = text.find("\"deps\":[") {
        let mut rest = &text[i + "\"deps\":[".len()..];
        while let Some(entry) = rest.trim_start().strip_prefix('[') {
            let Some(end) = entry.find(']') else { break };
            let fields: Vec<&str> = entry[..end].split(',').collect();
            if let (4, Some(Ok(fp))) =
                (fields.len(), fields.last().map(|f| f.trim().parse::<u64>()))
            {
                deps.push(fp);
            }
            match entry[end + 1..].trim_start().strip_prefix(',') {
                Some(next) => rest = next,
                None => break,
            }
        }
    }
    (profile, deps)
}

fn number_after(text: &str, key: &str) -> Option<u64> {
    let tail = text[text.find(key)? + key.len()..].trim_start();
    let end = tail.find(|c: char| !c.is_ascii_digit()).unwrap_or(tail.len());
    tail[..end].parse().ok()
}

/// The hex a unit's own hash file holds for fingerprint `fp`.
pub fn fingerprint_hex(fp: u64) -> String {
    fp.to_le_bytes().iter().map(|b| format!("{b:02x}")).collect()
}

/// Files and folders outside `.fingerprint` that belong to one unit.
#[derive(Default)]
struct Outputs {
    files: Vec<PathBuf>,
    dirs: Vec<PathBuf>,
}

fn index_outputs(profile_dir: &Path) -> HashMap<String, Outputs> {
    let mut out: HashMap<String, Outputs> = HashMap::new();
    for sub in ["deps", "examples"] {
        let Ok(rd) = fs::read_dir(profile_dir.join(sub)) else { continue };
        for e in rd.flatten() {
            if !e.file_type().is_ok_and(|t| t.is_file()) {
                continue;
            }
            let Some(name) = e.file_name().to_str().map(str::to_string) else { continue };
            if let Some(h) = output_hash(&name) {
                out.entry(h.to_string()).or_default().files.push(e.path());
            }
        }
    }
    if let Ok(rd) = fs::read_dir(profile_dir.join("build")) {
        for e in rd.flatten() {
            if !e.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let Some(name) = e.file_name().to_str().map(str::to_string) else { continue };
            if let Some((_, h)) = split_hash(&name) {
                out.entry(h.to_string()).or_default().dirs.push(e.path());
            }
        }
    }
    out
}

/// `(package, hash)` from `<package>-<16 hex>`.
pub fn split_hash(name: &str) -> Option<(&str, &str)> {
    let (pkg, hash) = name.rsplit_once('-')?;
    (!pkg.is_empty() && is_hash(hash)).then_some((pkg, hash))
}

/// The unit hash in an output file name: `libserde-3f047ae6714a941f.rlib`,
/// `streamarchiver-96609692d25e96c6.exe`, `foo-0123456789abcdef.dll.lib`.
/// Names without one (Cargo's uplifted copies) are never matched.
pub fn output_hash(file_name: &str) -> Option<&str> {
    let stem = file_name.split('.').next()?;
    split_hash(stem).map(|(_, h)| h)
}

fn is_hash(s: &str) -> bool {
    s.len() == 16 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// For a target-kind JSON stem of this package, the target's name — `None`
/// for anything that isn't a binary, test or example (build scripts above
/// all: the one running now lives in one of those folders).
pub fn local_target_name(stem: &str) -> Option<&str> {
    let s = stem.strip_prefix("test-").unwrap_or(stem);
    ["bin-", "example-", "bench-"].iter().find_map(|p| s.strip_prefix(p)).filter(|n| !n.is_empty())
}

/// The package version of every unit it can be determined for.
fn unit_versions(
    inp: &Inputs,
    units: &HashMap<String, Unit>,
    outputs: &HashMap<String, Outputs>,
) -> HashMap<String, String> {
    let mut versions = HashMap::new();
    for u in units.values() {
        if u.pkg == inp.root_pkg {
            continue;
        }
        let Some(o) = outputs.get(&u.hash) else { continue };
        // Compiled units: deps/<crate>-<hash>.d; build scripts: build/<pkg>-<hash>/*.d
        let mut dep_infos: Vec<PathBuf> =
            o.files.iter().filter(|f| f.extension().is_some_and(|x| x == "d")).cloned().collect();
        for d in &o.dirs {
            if let Ok(rd) = fs::read_dir(d) {
                dep_infos.extend(
                    rd.flatten()
                        .map(|e| e.path())
                        .filter(|p| p.extension().is_some_and(|x| x == "d")),
                );
            }
        }
        for di in dep_infos {
            if let Some(v) = read_head(&di).and_then(|t| registry_version(&t, &u.pkg)) {
                versions.insert(u.hash.clone(), v);
                break;
            }
        }
    }
    // Build-script runs have no dep-info: borrow the version of the build
    // script of the same package that they depend on.
    let by_hex: HashMap<String, &str> = units
        .values()
        .flat_map(|u| {
            u.targets.iter().filter_map(move |(stem, _)| {
                let hex = fs::read_to_string(u.dir.join(stem)).ok()?;
                Some((hex.trim().to_string(), u.hash.as_str()))
            })
        })
        .collect();
    let mut borrowed = Vec::new();
    for u in units.values() {
        if u.pkg == inp.root_pkg || versions.contains_key(&u.hash) {
            continue;
        }
        let found = u.deps.iter().find_map(|d| {
            let h = by_hex.get(d)?;
            let dep = units.get(*h)?;
            (dep.pkg == u.pkg).then(|| versions.get(*h).cloned()).flatten()
        });
        if let Some(v) = found {
            borrowed.push((u.hash.clone(), v));
        }
    }
    versions.extend(borrowed);
    versions
}

/// The first 64 KB of a file — dep-info lists the package's own sources
/// first, so the registry path is always near the top.
fn read_head(path: &Path) -> Option<String> {
    let mut buf = Vec::new();
    fs::File::open(path).ok()?.take(64 * 1024).read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// `pkg`'s version from a dep-info text, by finding a source path of the form
/// `…/registry/src/<index>/<pkg>-<version>/…`. Only a path naming this very
/// package counts, so a file pulled in from another crate can't mislead it.
pub fn registry_version(dep_info: &str, pkg: &str) -> Option<String> {
    let norm = dep_info.replace('\\', "/");
    let needle = format!("/{pkg}-");
    let mut rest = norm.as_str();
    while let Some(i) = rest.find("/registry/src/") {
        let tail = &rest[i + "/registry/src/".len()..];
        rest = tail;
        // Skip the index folder name.
        let Some(slash) = tail.find('/') else { break };
        let after_index = &tail[slash..];
        let Some(v) = after_index.strip_prefix(&needle) else { continue };
        let ver = &v[..v.find('/')?];
        let first = ver.split('.').next()?;
        if ver.split('.').count() >= 3
            && first.chars().all(|c| c.is_ascii_digit())
            && !first.is_empty()
        {
            return Some(ver.to_string());
        }
    }
    None
}

/// Package name → every version `Cargo.lock` pins for it.
pub fn parse_lock(text: &str) -> HashMap<String, HashSet<String>> {
    let mut out: HashMap<String, HashSet<String>> = HashMap::new();
    let mut name: Option<String> = None;
    for line in text.lines().map(str::trim) {
        if line == "[[package]]" {
            name = None;
        } else if let Some(v) = line.strip_prefix("name = ") {
            name = Some(v.trim_matches('"').to_string());
        } else if let Some(v) = line.strip_prefix("version = ") {
            let Some(n) = name.take() else { continue };
            out.entry(n).or_default().insert(v.trim_matches('"').to_string());
        }
    }
    out
}

/// Bytes under a path (a file's length, or a folder's recursive total).
fn size_of(path: &Path) -> u64 {
    let Ok(meta) = fs::symlink_metadata(path) else { return 0 };
    if !meta.is_dir() {
        return meta.len();
    }
    let mut total = 0;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            match e.file_type() {
                Ok(t) if t.is_dir() => stack.push(e.path()),
                Ok(_) => total += e.metadata().map(|m| m.len()).unwrap_or(0),
                Err(_) => {}
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake profile directory in the system temp folder, removed on drop.
    struct Profile(PathBuf);

    impl Profile {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!("sa-target-gc-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&p);
            fs::create_dir_all(&p).unwrap();
            Profile(p)
        }

        fn write(&self, rel: &str, body: &str) -> PathBuf {
            let p = self.0.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, body).unwrap();
            p
        }

        /// A fingerprint unit: its JSON (with `deps` fingerprints) and hash file.
        fn unit(&self, dir: &str, stem: &str, own_fp: u64, deps: &[u64]) {
            self.unit_in(dir, stem, own_fp, deps, 7);
        }

        fn unit_in(&self, dir: &str, stem: &str, own_fp: u64, deps: &[u64], profile: u64) {
            let deps: Vec<String> = deps.iter().map(|d| format!(r#"[1,"x",false,{d}]"#)).collect();
            self.write(
                &format!(".fingerprint/{dir}/{stem}.json"),
                &format!(
                    r#"{{"rustc":1,"profile":{profile},"deps":[{}],"local":[]}}"#,
                    deps.join(",")
                ),
            );
            self.write(&format!(".fingerprint/{dir}/{stem}"), &fingerprint_hex(own_fp));
        }

        fn inputs<'a>(&'a self, lock: &'a str) -> Inputs<'a> {
            Inputs {
                profile_dir: &self.0,
                root_pkg: "app",
                lock: Some(lock),
                running_unit: None,
                incremental_slack: Duration::from_secs(7 * 86400),
                local_always_rebuilt: true,
            }
        }
    }

    impl Drop for Profile {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn set_mtime(path: &Path, t: SystemTime) {
        fs::File::options().write(true).open(path).unwrap().set_modified(t).unwrap();
    }

    fn set_dir_mtime(path: &Path, t: SystemTime) {
        let mut opts = fs::OpenOptions::new();
        #[cfg(windows)]
        {
            // A directory handle needs backup semantics, and setting its times
            // needs FILE_WRITE_ATTRIBUTES — not the GENERIC_WRITE `.write(true)`
            // would ask for, which a directory refuses.
            use std::os::windows::fs::OpenOptionsExt;
            const FILE_WRITE_ATTRIBUTES: u32 = 0x100;
            const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
            opts.access_mode(FILE_WRITE_ATTRIBUTES).custom_flags(FILE_FLAG_BACKUP_SEMANTICS);
        }
        #[cfg(not(windows))]
        opts.read(true);
        opts.open(path).unwrap().set_modified(t).unwrap();
    }

    const LOCK: &str = r#"
version = 4

[[package]]
name = "serde"
version = "1.0.228"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "aws-lc-rs"
version = "1.13.0"

[[package]]
name = "app"
version = "1.0.0"
"#;

    fn dep_info(pkg_ver: &str, out: &str) -> String {
        format!(
            "{out}: C:\\Users\\u\\.cargo\\registry\\src\\index.crates.io-1949cf8c6b5b557f\\{pkg_ver}\\src\\lib.rs\n"
        )
    }

    #[test]
    fn helpers_read_names_versions_and_hashes() {
        assert_eq!(
            split_hash("aws-lc-rs-1bb7ea4896abddb3"),
            Some(("aws-lc-rs", "1bb7ea4896abddb3"))
        );
        assert_eq!(split_hash("serde"), None);
        assert_eq!(split_hash("serde-XYZ"), None);
        assert_eq!(output_hash("libserde-3f047ae6714a941f.rlib"), Some("3f047ae6714a941f"));
        assert_eq!(output_hash("foo-0123456789abcdef.dll.lib"), Some("0123456789abcdef"));
        assert_eq!(output_hash("streamarchiver.exe"), None, "uplifted copies are never matched");
        assert_eq!(local_target_name("test-bin-app"), Some("app"));
        assert_eq!(local_target_name("example-vp_probe"), Some("vp_probe"));
        assert_eq!(local_target_name("build-script-build-script-build"), None);
        assert_eq!(local_target_name("run-build-script-build-script-build"), None);
        // Little-endian, as Cargo writes its hash files (verified against a
        // real target directory: 3625170821281366791 -> 074396aca7314f32).
        assert_eq!(fingerprint_hex(3625170821281366791), "074396aca7314f32");

        let di = dep_info("aws-lc-rs-1.13.0", "x.d");
        assert_eq!(registry_version(&di, "aws-lc-rs").as_deref(), Some("1.13.0"));
        assert_eq!(registry_version(&di, "aws-lc"), None, "a longer name is not this package");
        assert_eq!(registry_version(&di, "serde"), None);
        assert_eq!(registry_version("x.d: src/lib.rs", "serde"), None, "path deps have no version");

        let lock = parse_lock(LOCK);
        assert!(lock["serde"].contains("1.0.228"));
        assert!(lock["aws-lc-rs"].contains("1.13.0"));
        assert_eq!(lock.len(), 3);
    }

    #[test]
    fn stale_versions_go_locked_ones_and_unknowns_stay() {
        let p = Profile::new("versions");
        // serde 1.0.200: an old version, with its build script and run.
        p.unit("serde-00000000000000a1", "lib-serde", 11, &[]);
        p.write("deps/libserde-00000000000000a1.rlib", "old rlib");
        p.write("deps/serde-00000000000000a1.d", &dep_info("serde-1.0.200", "a"));
        p.unit("serde-00000000000000b1", "build-script-build-script-build", 12, &[]);
        p.write(
            "build/serde-00000000000000b1/build_script_build-00000000000000b1.d",
            &dep_info("serde-1.0.200", "b"),
        );
        p.unit("serde-00000000000000c1", "run-build-script-build-script-build", 13, &[12]);
        p.write("build/serde-00000000000000c1/out/generated.rs", "old out");
        // serde 1.0.228: the locked version, twice (two feature variants).
        p.unit("serde-00000000000000a2", "lib-serde", 21, &[]);
        p.write("deps/libserde-00000000000000a2.rlib", "new rlib");
        p.write("deps/serde-00000000000000a2.d", &dep_info("serde-1.0.228", "c"));
        p.unit("serde-00000000000000a3", "lib-serde", 22, &[]);
        p.write("deps/serde-00000000000000a3.d", &dep_info("serde-1.0.228", "d"));
        // A path dependency: no registry path, no version, kept.
        p.unit("helper-00000000000000d1", "lib-helper", 31, &[]);
        p.write("deps/helper-00000000000000d1.d", "e: ../helper/src/lib.rs\n");
        // A package the lock no longer has at all.
        p.unit("gone-00000000000000e1", "lib-gone", 41, &[]);
        p.write("deps/gone-00000000000000e1.d", &dep_info("gone-0.1.0", "f"));

        let plan = plan(&p.inputs(LOCK));
        let rel = |v: &[PathBuf]| -> Vec<String> {
            v.iter()
                .map(|x| x.strip_prefix(&p.0).unwrap().to_string_lossy().replace('\\', "/"))
                .collect()
        };
        assert_eq!(plan.stale_versions, 4, "{:?} {:?}", rel(&plan.dirs), rel(&plan.files));
        assert_eq!(
            rel(&plan.dirs),
            vec![
                ".fingerprint/gone-00000000000000e1",
                ".fingerprint/serde-00000000000000a1",
                ".fingerprint/serde-00000000000000b1",
                ".fingerprint/serde-00000000000000c1",
                "build/serde-00000000000000b1",
                "build/serde-00000000000000c1",
            ]
        );
        assert_eq!(
            rel(&plan.files),
            vec![
                "deps/gone-00000000000000e1.d",
                "deps/libserde-00000000000000a1.rlib",
                "deps/serde-00000000000000a1.d",
            ]
        );

        // No lock (or an empty one): nothing is judged stale by version.
        let mut inp = p.inputs("");
        assert_eq!(plan_for(&inp).stale_versions, 0);
        inp.lock = None;
        assert_eq!(plan_for(&inp).stale_versions, 0);

        // The build-script run executing right now is untouchable.
        let mut inp = p.inputs(LOCK);
        inp.running_unit = Some("00000000000000c1");
        let kept = plan_for(&inp);
        assert!(kept.dirs.iter().all(|d| !d.to_string_lossy().contains("00000000000000c1")));

        let expected = plan.bytes();
        assert!(expected > 0);
        let (freed, failed) = plan.execute();
        assert_eq!(freed, expected, "every planned byte is freed");
        assert_eq!(failed, 0);
        assert!(!p.0.join("deps/libserde-00000000000000a1.rlib").exists());
        assert!(p.0.join("deps/libserde-00000000000000a2.rlib").exists(), "locked version kept");
        assert!(p.0.join(".fingerprint/helper-00000000000000d1").exists(), "unknown version kept");
    }

    fn plan_for(inp: &Inputs) -> Plan {
        plan(inp)
    }

    #[test]
    fn only_the_newest_build_of_each_local_target_keeps_its_outputs() {
        let p = Profile::new("local");
        let now = SystemTime::now();
        let old = now - Duration::from_secs(3 * 86400);
        // Two test binaries: the older one's files go.
        p.unit("app-0000000000000001", "test-bin-app", 1, &[]);
        p.write("deps/app-0000000000000001.exe", "old test exe");
        p.write("deps/app-0000000000000001.pdb", "old test pdb");
        p.unit("app-0000000000000002", "test-bin-app", 2, &[]);
        p.write("deps/app-0000000000000002.exe", "new test exe");
        set_mtime(&p.0.join(".fingerprint/app-0000000000000001/test-bin-app.json"), old);
        // A folder holding an old bin AND an example that is still the newest
        // of its kind: the example's files stay.
        p.unit("app-0000000000000003", "bin-app", 3, &[]);
        p.unit("app-0000000000000003", "example-probe", 4, &[]);
        p.write("deps/app-0000000000000003.exe", "old bin");
        p.write("examples/probe-0000000000000003.exe", "only probe build");
        set_mtime(&p.0.join(".fingerprint/app-0000000000000003/bin-app.json"), old);
        p.unit("app-0000000000000004", "bin-app", 5, &[]);
        p.write("deps/app-0000000000000004.exe", "new bin");
        // Build scripts of this package are never touched.
        p.unit("app-0000000000000005", "build-script-build-script-build", 6, &[]);
        p.write("build/app-0000000000000005/build_script_build-0000000000000005.exe", "bs");
        p.unit("app-0000000000000006", "build-script-build-script-build", 7, &[]);
        set_mtime(
            &p.0.join(".fingerprint/app-0000000000000005/build-script-build-script-build.json"),
            old,
        );
        // A clippy run of the tests, newer than the real test build: a
        // different profile, so it supersedes nothing of the real one.
        p.unit_in("app-0000000000000007", "test-bin-app", 8, &[], 99);
        p.write("deps/app-0000000000000007.d", "check-mode dep-info");

        let plan = plan(&p.inputs(LOCK));
        let mut names: Vec<String> = plan
            .files
            .iter()
            .map(|f| f.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                "app-0000000000000001.exe",
                "app-0000000000000001.pdb",
                "app-0000000000000003.exe"
            ]
        );
        assert_eq!(plan.superseded_local, 2);
        assert!(
            plan.dirs.is_empty(),
            "no local fingerprint or build folder is removed: {:?}",
            plan.dirs
        );

        // A package whose targets are not rebuilt on every build keeps them all.
        let mut inp = p.inputs(LOCK);
        inp.local_always_rebuilt = false;
        let kept = super::plan(&inp);
        assert!(kept.files.is_empty(), "{:?}", kept.files);
        assert_eq!(kept.superseded_local, 0);
    }

    #[test]
    fn fingerprint_json_reader_takes_profile_and_dep_fingerprints() {
        // The shape Cargo writes (fields trimmed for length).
        let text = r#"{"rustc":1562763049001146449,"features":"[\"default\"]","target":13257006825011013318,"profile":8617065825857819494,"path":4942398508502643691,"deps":[[962701078083361349,"build_script_build",false,7530688809888881362],[1156329338539430517,"eframe",false,3625170821281366791]],"local":[{"CheckDepInfo":{"dep_info":"debug\\.fingerprint\\x-0123456789abcdef\\dep-lib-x","checksum":false}}],"rustflags":[],"config":0,"compile_kind":0}"#;
        let (profile, deps) = read_fingerprint_json(text);
        assert_eq!(profile, Some(8617065825857819494));
        assert_eq!(deps, vec![7530688809888881362, 3625170821281366791]);

        assert_eq!(read_fingerprint_json(r#"{"profile":5,"deps":[]}"#), (Some(5), vec![]));
        assert_eq!(read_fingerprint_json("not json"), (None, vec![]));
        // A malformed entry is skipped, the rest still read.
        assert_eq!(
            read_fingerprint_json(r#"{"deps":[[1,"a",false],[2,"b",true,9]]}"#),
            (None, vec![9])
        );
    }

    #[test]
    fn incremental_caches_are_judged_against_the_newest_not_the_clock() {
        let p = Profile::new("incremental");
        let long_ago = SystemTime::now() - Duration::from_secs(60 * 86400);
        for (name, days_before_newest) in
            [("app-aaa", 0u64), ("app-bbb", 3), ("app-ccc", 10), ("probe-ddd", 30)]
        {
            let d = p.0.join("incremental").join(name);
            fs::create_dir_all(d.join("s-session")).unwrap();
            set_dir_mtime(&d, long_ago - Duration::from_secs(days_before_newest * 86400));
        }
        // Everything is two months old — a project picked up again after a
        // break. Only the ones far behind the newest go.
        let plan = plan(&p.inputs(LOCK));
        let mut names: Vec<String> = plan
            .dirs
            .iter()
            .map(|d| d.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, vec!["app-ccc", "probe-ddd"]);
        assert_eq!(plan.incremental, 2);
    }

    #[test]
    fn a_missing_or_empty_profile_plans_nothing() {
        let p = Profile::new("empty");
        assert!(plan(&p.inputs(LOCK)).is_empty());
        let inp = Inputs {
            profile_dir: &p.0.join("does-not-exist"),
            root_pkg: "app",
            lock: Some(LOCK),
            running_unit: None,
            incremental_slack: Duration::from_secs(1),
            local_always_rebuilt: true,
        };
        assert!(plan(&inp).is_empty());
    }
}
