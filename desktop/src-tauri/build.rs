fn main() {
    // Optional local-only extras module: when src/ext_ops.rs is present, expose
    // the `has_ext_ops` cfg so main.rs compiles it in. A public checkout without
    // that file simply builds without it — no build flag to remember.
    println!("cargo::rustc-check-cfg=cfg(has_ext_ops)");
    if std::path::Path::new("src/ext_ops.rs").exists() {
        println!("cargo::rustc-cfg=has_ext_ops");
    }
    // Rebuild if the file appears or disappears.
    println!("cargo::rerun-if-changed=src/ext_ops.rs");
    tauri_build::build()
}
