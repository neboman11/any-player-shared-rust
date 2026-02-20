fn main() {
    // Build info generation removed to avoid vergen-lib version conflict.
    // LIBRESPOT_BUILD_ID defaults to "unknown" at runtime when not set.
    println!("cargo:rustc-env=LIBRESPOT_BUILD_ID=unknown");
    println!("cargo:rustc-env=VERGEN_BUILD_DATE=unknown");
    println!("cargo:rustc-env=VERGEN_GIT_SHA=unknown");
    println!("cargo:rustc-env=VERGEN_GIT_COMMIT_DATE=unknown");
}
