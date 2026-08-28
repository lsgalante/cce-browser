//! Generates the WPE WebKit FFI bindings, but **only under the `wpe` feature**.
//!
//! Without it this is a no-op, so a default (Servo) build needs no WPE headers
//! and behaves exactly as it did before the port started. See WPE-PORT.md.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/wpe/wrapper.h");
    if std::env::var("CARGO_FEATURE_WPE").is_err() {
        return;
    }

    // Both modules are needed: wpe-webkit-2.0 is the engine, wpe-platform-2.0
    // is the embedding layer (WPEDisplay / WPEView / WPEToplevel) the port
    // subclasses. pkg-config emits the link flags for us.
    let mut clang_args = Vec::new();
    for module in ["wpe-webkit-2.0", "wpe-platform-2.0"] {
        let lib = pkg_config::Config::new()
            .probe(module)
            .unwrap_or_else(|e| panic!("`{module}` not found — pacman -S wpewebkit ({e})"));
        for path in &lib.include_paths {
            clang_args.push(format!("-I{}", path.display()));
        }
    }

    let bindings = bindgen::Builder::default()
        .header("src/wpe/wrapper.h")
        .clang_args(&clang_args)
        // The engine, the embedding layer, and just enough GObject to
        // register subclasses and turn a main loop.
        .allowlist_item("(wpe|WPE|webkit|WebKit)_?.*")
        .allowlist_item("g_(object|type|signal|bytes|timeout|free|error)_.*")
        // The main loop AND the context: `pump` drains the context directly.
        .allowlist_item("g_main_(loop|context)_.*")
        .allowlist_item("G(Object|Type|Value|Bytes|Error|MainLoop|ParamSpec|Closure).*")
        // The main-context poll protocol: GPollFD is what `query` fills in.
        .allowlist_item("G(MainContext|PollFD|Source).*")
        .allowlist_item("g_(type|object)_.*")
        // GObject's generated enums are plain C enums; keep them as consts so
        // vfunc tables and property flags stay comparable without casts.
        .default_enum_style(bindgen::EnumVariation::ModuleConsts)
        .derive_default(true)
        .layout_tests(false)
        .generate()
        .expect("bindgen failed over the WPE headers");

    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out.join("wpe_bindings.rs"))
        .expect("could not write wpe_bindings.rs");
}
