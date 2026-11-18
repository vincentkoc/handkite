use std::{env, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=native/vision.m");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    let object = out.join("vision.o");
    let target = env::var("TARGET").expect("TARGET");
    let deployment = env::var("MACOSX_DEPLOYMENT_TARGET").unwrap_or_else(|_| "11.0".into());
    println!("cargo:rerun-if-env-changed=MACOSX_DEPLOYMENT_TARGET");
    let arch = if target.starts_with("aarch64") {
        "arm64"
    } else {
        "x86_64"
    };
    assert!(
        Command::new("xcrun")
            .args([
                "clang",
                "-arch",
                arch,
                &format!("-mmacosx-version-min={deployment}"),
                "-fobjc-arc",
                "-O2",
                "-c",
                "native/vision.m",
                "-o"
            ])
            .arg(&object)
            .status()
            .expect("clang")
            .success()
    );
    assert!(
        Command::new("xcrun")
            .args(["ar", "crs"])
            .arg(out.join("libhandkite_vision.a"))
            .arg(object)
            .status()
            .expect("ar")
            .success()
    );
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=handkite_vision");
    for framework in ["Vision", "Foundation", "CoreGraphics"] {
        println!("cargo:rustc-link-lib=framework={framework}");
    }
}
