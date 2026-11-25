//! Puts the application manifest into the executable.

/// The process heap (Direct3D's, the driver's, the title's) as a segment
/// heap: see `crates/victory-gpu/src/heap.rs`.
const MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <asmv3:application xmlns:asmv3="urn:schemas-microsoft-com:asm.v3">
    <asmv3:windowsSettings>
      <heapType xmlns="http://schemas.microsoft.com/SMI/2020/WindowsSettings">SegmentHeap</heapType>
    </asmv3:windowsSettings>
  </asmv3:application>
</assembly>
"#;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let windows = std::env::var("CARGO_CFG_TARGET_OS").is_ok_and(|v| v == "windows");
    let msvc = std::env::var("CARGO_CFG_TARGET_ENV").is_ok_and(|v| v == "msvc");
    if !(windows && msvc) {
        return;
    }
    let Ok(dir) = std::env::var("OUT_DIR") else {
        return;
    };
    let path = std::path::Path::new(&dir).join("victory.manifest");
    if let Err(e) = std::fs::write(&path, MANIFEST) {
        println!("cargo:warning=no application manifest: {e}");
        return;
    }
    println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
    println!("cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}", path.display());
}
