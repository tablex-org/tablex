// `rust_i18n::i18n!` reads `locales/` while the crate compiles and does not tell cargo: without this,
// an edit of a locale file would not rebuild the crate.
fn main() {
    println!("cargo:rerun-if-changed=locales");
    #[cfg(windows)]
    windows_resources();
}

// The icon of tablex.exe (Explorer, taskbar, Alt-Tab) and its version info (the Details tab of its properties),
// from assets/AppIcon.ico and the version of Cargo.toml. embed-resource (already in the tree through gpui) links
// the compiled resource into the bin; the .rc is written here because it holds the version.
#[cfg(windows)]
fn windows_resources() {
    use std::path::PathBuf;

    let version = std::env::var("CARGO_PKG_VERSION").unwrap();
    let mut numbers: Vec<&str> = version.split(['.', '-', '+']).take(3).collect();
    numbers.resize(4, "0");
    let numbers = numbers.join(",");
    let manifest_dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let icon = manifest_dir.join("assets").join("AppIcon.ico");
    println!("cargo:rerun-if-changed={}", icon.display());
    let rc = format!(
        r#"1 ICON "{icon}"
1 VERSIONINFO
FILEVERSION {numbers}
PRODUCTVERSION {numbers}
FILEOS 0x40004
FILETYPE 1
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "ProductName", "tablex"
      VALUE "FileDescription", "tablex"
      VALUE "InternalName", "tablex"
      VALUE "OriginalFilename", "tablex.exe"
      VALUE "FileVersion", "{version}"
      VALUE "ProductVersion", "{version}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#,
        // rc.exe reads a backslash in a string as an escape.
        icon = icon.display().to_string().replace('\\', "/"),
    );
    let out = PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("tablex.rc");
    std::fs::write(&out, rc).unwrap();
    embed_resource::compile(&out, embed_resource::NONE)
        .manifest_required()
        .unwrap();
}
