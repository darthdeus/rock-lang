pub fn available() -> bool {
    if std::env::var_os("STONE_PRIVATE_DISPLAY").is_some() {
        return true;
    }
    println!("skipping: needs the private display; run `cargo nextest run -p stone-rs`");
    false
}
