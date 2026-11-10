// rust-embed requires its folder to exist at compile time. Create an empty one
// so `cargo test` / `cargo check` work without building the frontend first.
fn main() {
    let _ = std::fs::create_dir_all("web/build");
    println!("cargo:rerun-if-changed=web/build");
}
