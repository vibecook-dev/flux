//! Tells the binary the target it was built for (`FLUX_TARGET`): the updater
//! (src/update) fetches the release package of the same target.
fn main() {
    let target = std::env::var("TARGET").unwrap_or_default();
    println!("cargo:rustc-env=FLUX_TARGET={target}");
    println!("cargo:rerun-if-changed=build.rs");
}
