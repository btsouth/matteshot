fn main() {
    // Embed the app icon (regenerate with `matteshot --icon assets`).
    if std::path::Path::new("assets/matteshot.ico").exists() {
        let mut res = winres::WindowsResource::new();
        res.set_icon("assets/matteshot.ico");
        res.compile().expect("embed icon resource");
    }
    println!("cargo:rerun-if-changed=assets/matteshot.ico");
}
