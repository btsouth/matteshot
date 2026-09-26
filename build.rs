use std::path::PathBuf;

fn main() {
    // Windows shows these in Task Manager and the file properties dialog, and
    // Defender's static classifier reads them too. 0.13.1 shipped with a blank
    // CompanyName and a lowercase product name.
    //
    // The script is written here rather than checked in so the version fields
    // always come from Cargo.toml. It avoids #include on purpose: plain
    // numbers compile the same under rc.exe on Windows and llvm-rc when
    // cross-checking from Linux, with no SDK headers on the include path.
    let manifest_dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    let version = std::env::var("CARGO_PKG_VERSION").unwrap();
    let numeric = format!(
        "{},{},{},0",
        std::env::var("CARGO_PKG_VERSION_MAJOR").unwrap(),
        std::env::var("CARGO_PKG_VERSION_MINOR").unwrap(),
        std::env::var("CARGO_PKG_VERSION_PATCH").unwrap(),
    );

    // Resource id 1 is the app icon: the tray and every window load it by
    // that id (regenerate the file with `matteshot --icon assets`).
    let icon = manifest_dir.join("assets").join("matteshot.ico");
    let icon_line = if icon.exists() {
        format!("1 ICON \"{}\"\n", icon.display().to_string().replace('\\', "\\\\"))
    } else {
        String::new()
    };

    let script = format!(
        r#"{icon_line}
1 VERSIONINFO
FILEVERSION {numeric}
PRODUCTVERSION {numeric}
FILEFLAGSMASK 0x3f
FILEFLAGS 0x0
FILEOS 0x40004
FILETYPE 0x1
FILESUBTYPE 0x0
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904b0"
    BEGIN
      VALUE "CompanyName", "Southbound Software"
      VALUE "FileDescription", "Matteshot"
      VALUE "FileVersion", "{version}"
      VALUE "LegalCopyright", "Copyright (C) 2026 Southbound Software. MIT OR Apache-2.0."
      VALUE "OriginalFilename", "matteshot.exe"
      VALUE "ProductName", "Matteshot"
      VALUE "ProductVersion", "{version}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#
    );
    let rc = out_dir.join("matteshot.rc");
    std::fs::write(&rc, script).expect("write resource script");

    // Only the Windows target links a resource. Anything else (a host-side
    // tool run, a stray `cargo check` without --target) has nothing to embed.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile(&rc, embed_resource::NONE)
            .manifest_optional()
            .expect("embed version resource");
    }
    println!("cargo:rerun-if-changed=assets/matteshot.ico");
    println!("cargo:rerun-if-changed=build.rs");
}
