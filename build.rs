fn main() {
    let mut res = winres::WindowsResource::new();
    // Embed the app icon (regenerate with `matteshot --icon assets`).
    if std::path::Path::new("assets/matteshot.ico").exists() {
        res.set_icon("assets/matteshot.ico");
    }
    // Windows shows these in Task Manager and the file properties dialog, and
    // Defender's static classifier reads them too. 0.13.1 shipped with a blank
    // CompanyName and a lowercase product name.
    res.set("CompanyName", "SouthForge AI")
        .set("ProductName", "Matteshot")
        .set("FileDescription", "Matteshot")
        .set("LegalCopyright", "Copyright (C) 2026 SouthForge AI")
        .set("OriginalFilename", "matteshot.exe");
    res.compile().expect("embed version resource");
    println!("cargo:rerun-if-changed=assets/matteshot.ico");
}
