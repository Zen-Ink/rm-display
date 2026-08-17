use std::env;

fn main() {
    println!("cargo:rerun-if-env-changed=RMPP_QUILL_LIB_DIR");
    if env::var_os("CARGO_FEATURE_QUILL").is_none() {
        return;
    }

    let target = env::var("TARGET").unwrap_or_default();
    if target != "aarch64-unknown-linux-gnu" {
        println!("cargo:warning=quill feature enabled for {target}; real Quill backend is not linked on this host");
        return;
    }

    let directory = env::var("RMPP_QUILL_LIB_DIR")
        .expect("RMPP_QUILL_LIB_DIR must contain the aarch64 libquill.so");
    println!("cargo:rustc-link-search=native={directory}");
    println!("cargo:rustc-link-lib=dylib=quill");
    println!("cargo:rustc-link-arg=-Wl,--allow-shlib-undefined");
    // DT_RPATH is inherited while resolving libquill's dependencies, unlike
    // DT_RUNPATH. This lets a colocated libqsgepaper.so be found as well.
    println!("cargo:rustc-link-arg=-Wl,--disable-new-dtags");
    println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN");
}
