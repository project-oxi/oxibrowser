//! Link configuration for the optional ANGLE-backed `webgl` feature.
//!
//! Libraries come from `third_party/angle/<rust-triple>/` (provisioned by
//! `scripts/setup-angle.sh`, gitignored). Link order matters: dependents first
//! (EGL, GLES), then the ANGLE core they resolve against, then the C++ runtime
//! and Apple frameworks the Metal backend needs.

use std::env;
use std::path::PathBuf;

fn main() {
    let webgl = env::var("CARGO_FEATURE_WEBGL").is_ok();
    if !webgl {
        return;
    }

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        panic!(
            "feature `webgl`: ANGLE Metal backend is currently provisioned for macOS only; \
             build without `--features webgl` on other platforms"
        );
    }

    let arch = match env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("aarch64") => "aarch64-apple-darwin",
        Ok("x86_64") => "x86_64-apple-darwin",
        other => panic!("feature `webgl`: unsupported target arch {other:?}"),
    };

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    // crates/oxibrowser-render -> crates -> workspace root
    let workspace_root = manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .expect("crate lives two levels below the workspace root");
    let angle_dir = workspace_root.join("third_party").join("angle").join(arch);

    if !angle_dir.join("libANGLE_all.a").is_file() {
        panic!(
            "feature `webgl`: ANGLE static archive missing at {}/libANGLE_all.a.\n\
             Run: scripts/setup-angle.sh   (pinned prebuilt, Metal draws unverified)\n\
             or:  scripts/build-angle.sh   (ANGLE chromium/8118 built from source, preferred)",
            angle_dir.display()
        );
    }

    println!("cargo:rustc-link-search=native={}", angle_dir.display());
    // Single fat archive built by scripts/build-angle.sh (dylib .o closure).
    println!("cargo:rustc-link-lib=static=ANGLE_all");
    println!("cargo:rustc-link-lib=dylib=c++");
    // The Metal backend carries Objective-C classes referenced only via the
    // ObjC runtime; without -ObjC the linker drops them from the archive.
    println!("cargo:rustc-link-arg=-ObjC");
    for fw in [
        "Metal",
        "IOSurface",
        "QuartzCore",
        "CoreFoundation",
        "Foundation",
        "CoreGraphics",
        "IOKit",
    ] {
        println!("cargo:rustc-link-lib=framework={fw}");
    }

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../third_party/angle");
}
