use std::path::PathBuf;

fn main() {
    let config = slint_build::CompilerConfiguration::new().with_style("fluent-dark".into());
    slint_build::compile_with_config("ui/app.slint", config).unwrap();

    // The exe's icon (Explorer, shortcuts): assets/app.res is app.rc compiled with rc.exe. link.exe takes a
    // .res as it is, so building needs no resource compiler; assets/app.rc and icon.ico are its source.
    // Only link.exe reads a .res: a -gnu toolchain's ld cannot, so it builds without the icon.
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        let res: PathBuf = [&std::env::var("CARGO_MANIFEST_DIR").unwrap(), "assets", "app.res"].iter().collect();
        println!("cargo:rustc-link-arg-bins={}", res.display());
    }
    println!("cargo:rerun-if-changed=assets/app.res");
}
