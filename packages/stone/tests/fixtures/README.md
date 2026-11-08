These fixtures are produced by this project; none is third-party content.

- `tone.ogg` is a 60 ms mono Vorbis tone generated from a 660 Hz sine at
  22050 Hz. It is the one encoded-Vorbis fixture for Stone's decoder and
  stream tests, Stone's asset-report tests and the CLI package tests, which
  reference this copy. There is no Vorbis encoder in the build, so it stays
  checked in instead of being generated per test.
- `pixel.ttf` is produced by `stone-rs/src/test_fonts.rs::pixel_font`.
  The Rock watcher-reload test needs a second font with different advances
  from the embedded DroidSans. It shares the Rust tests' generated ASCII
  font; it contains no third-party font data. Regenerate it by calling that
  function and writing its bytes to this path.
