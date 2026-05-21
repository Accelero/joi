//! Tauri build script: generates context, capability schemas, and embeds config/icons.
fn main() {
    // Re-embed the frontend when the built assets change, so `bun run build` + `cargo build`
    // picks up UI changes without a manual `touch` of main.rs.
    println!("cargo:rerun-if-changed=../dist");
    tauri_build::build();
}
