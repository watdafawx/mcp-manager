//! Embeds the icon and version info into `mcpm.exe` (Windows targets only).

fn main() {
    println!("cargo:rerun-if-changed=../../assets/mcpm.ico");
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let mut res = winresource::WindowsResource::new();
    res.set_icon("../../assets/mcpm.ico");
    res.set("ProductName", "MCP Manager");
    res.set("FileDescription", "MCP Manager");
    res.set("OriginalFilename", "mcpm.exe");
    if let Err(e) = res.compile() {
        // A missing resource compiler must not break `cargo build`: the exe just has no icon.
        println!("cargo:warning=could not embed the icon: {e}");
    }
}
