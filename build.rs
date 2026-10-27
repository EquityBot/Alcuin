// Windows reserves 1 MiB for a program's main thread, which the unoptimised
// CLI and MCP entries exceed on browser paths. Reserve the 8 MiB Linux gives.
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg-bins=/STACK:8388608");
    }
}
