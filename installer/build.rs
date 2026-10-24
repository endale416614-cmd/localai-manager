use std::path::PathBuf;

fn main() {
    // The icon of LocalAI-Setup.exe, the same resource as the manager's (see ../build.rs, which says why msvc only).
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        let res: PathBuf = [&std::env::var("CARGO_MANIFEST_DIR").unwrap(), "..", "assets", "app.res"].iter().collect();
        println!("cargo:rustc-link-arg-bins={}", res.display());
    }
    println!("cargo:rerun-if-changed=../assets/app.res");
}
