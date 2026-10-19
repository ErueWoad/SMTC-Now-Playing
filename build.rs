use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=icon.ico");
    if Path::new("icon.ico").exists() {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("icon.ico");
        let _ = res.compile();
    }
}