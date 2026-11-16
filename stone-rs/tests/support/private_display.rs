/// Window tests show real windows, change display modes and drive i3, so they
/// run only on the private Xvfb + i3 that `scripts/private-display` starts
/// (the nextest wrapper). A plain `cargo test` would otherwise use the
/// developer's desktop.
pub fn available() -> bool {
    if std::env::var_os("STONE_PRIVATE_DISPLAY").is_some() {
        return true;
    }
    println!("skipping: needs the private display; run `cargo nextest run -p stone-rs`");
    false
}
