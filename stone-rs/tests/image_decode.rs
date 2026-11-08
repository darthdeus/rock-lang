// Background decode probes: the dimension probe agrees with a full decode,
// and malformed input is rejected without decoding.

use stone_rs::image_decode;

fn write_png(dir: &std::path::Path, name: &str, w: u32, h: u32) -> String {
    let mut buf = image::RgbaImage::new(w, h);
    for (x, y, p) in buf.enumerate_pixels_mut() {
        *p = image::Rgba([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8, 255]);
    }
    let path = dir.join(name);
    buf.save(&path).expect("write png");
    path.to_string_lossy().to_string()
}

fn tmpdir(tag: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(&format!("stone_image_decode_{tag}_"))
        .tempdir()
        .expect("mkdir")
}

#[test]
fn probe_dimensions_matches_full_decode() {
    let dir = tmpdir("probe");
    let path = write_png(dir.path(), "probe.png", 61, 37);
    let bytes = std::fs::read(&path).expect("read");

    assert_eq!(image_decode::probe_dimensions(&bytes), Some((61, 37)));

    let (_, w, h) = image_decode::decode_rgba_bytes(&bytes, &path).expect("decode");
    assert_eq!((w, h), (61, 37), "probe and full decode must agree");
}

#[test]
fn probe_dimensions_rejects_garbage() {
    assert_eq!(image_decode::probe_dimensions(b"not an image at all"), None);
}
