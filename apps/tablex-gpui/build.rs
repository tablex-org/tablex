// `rust_i18n::i18n!` reads `locales/` while the crate compiles and does not tell cargo: without this,
// an edit of a locale file would not rebuild the crate.
fn main() {
    println!("cargo:rerun-if-changed=locales");
}
