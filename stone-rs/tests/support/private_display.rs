/// Window tests show real windows and drive i3, so they run only on the private Xvfb + i3
/// from `scripts/private-display`, never on the developer's desktop.
pub fn available() -> bool {
    if std::env::var_os("STONE_PRIVATE_DISPLAY").is_some() {
        return true;
    }
    println!("skipping: needs the private display; run `cargo nextest run -p stone-rs`");
    false
}
