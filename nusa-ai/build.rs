use std::path::PathBuf;

fn main() {
    // With the GNU toolchain WebView2Loader.dll must sit next to the exe.
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let Some(profile_dir) = out.ancestors().nth(3) else { return };
    let home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var("USERPROFILE").unwrap_or_default()).join(".cargo"));
    let Ok(regs) = std::fs::read_dir(home.join("registry").join("src")) else { return };
    for reg in regs.flatten() {
        let Ok(crates) = std::fs::read_dir(reg.path()) else { continue };
        for c in crates.flatten() {
            if c.file_name().to_string_lossy().starts_with("webview2-com-sys") {
                let dll = c.path().join("x64").join("WebView2Loader.dll");
                if dll.exists() {
                    let _ = std::fs::copy(&dll, profile_dir.join("WebView2Loader.dll"));
                    return;
                }
            }
        }
    }
}
