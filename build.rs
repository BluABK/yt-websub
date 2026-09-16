//! Build script. Its only job is housekeeping: keeping `target/` from growing
//! without bound — see `build_support/target_gc.rs`. It generates nothing and
//! sets nothing the crate reads.

#[path = "build_support/target_gc.rs"]
mod target_gc;

fn main() {
    // Re-run on the changes that rebuild the crate anyway, so the cleanup gets
    // its turn on every real rebuild — and on TARGET_GC, so `dry` and `force`
    // take effect without an edit. (With no directives at all Cargo would
    // re-run on any package file, but then the variable would be ignored.)
    for path in ["build.rs", "build_support", "src", "tests", "vendor", "Cargo.toml", "Cargo.lock"] {
        println!("cargo:rerun-if-changed={path}");
    }
    println!("cargo:rerun-if-env-changed=TARGET_GC");
    // `false`: this script's output never changes, so a build does not
    // necessarily recompile this crate's own binaries, and the cleanup must
    // leave their earlier builds alone.
    target_gc::run_from_build_script(false);
}
