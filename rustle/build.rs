fn main() {
    // Target triple of this build: `rustle update` fetches the matching
    // release asset.
    println!(
        "cargo:rustc-env=RUSTLE_TARGET={}",
        std::env::var("TARGET").expect("TARGET")
    );
    println!("cargo:rerun-if-changed=build.rs");
}
