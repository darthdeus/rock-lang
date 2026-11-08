# stone-rs

Pure Rust implementation of the Stone game engine library, providing a drop-in replacement for the C libstone.

## Status

**API Parity: 124/124 functions implemented** (excluding `stone_log` which is variadic C-only)

### Fully Implemented

- **Windowing**: Window creation, swap, resize, position, borders
- **Graphics**: OpenGL rendering, sprites, meshes, shaders, canvases, blend modes
- **Text**: Font loading (fontdue), text rendering with baseline alignment
- **Images**: Loading (PNG, JPEG), ImageData pixel manipulation
- **Input**: Keyboard (key down, key pressed by scancode), mouse position/buttons
- **Audio**: Sound effects and music via rodio (WAV, OGG Vorbis, MP3), reverb effect
- **Filesystem**: Read, write, exists, modtime, list directory, create directory
- **ImGui**: Full cimgui integration for debug UI
- **Stats**: Graphics statistics tracking (draw calls, memory, etc.)

### Not Implemented (1 function)

| Function | Reason |
|----------|--------|
| `stone_log` | Variadic function (`...`), difficult to expose via FFI |

### stone-rs Only Features

These features are only available when using the Rust backend (`--crabstone`):

| Feature | Function | Description |
|---------|----------|-------------|
| Reverb | `stone_set_reverb(enabled, duration_ms, amplitude)` | Apply reverb effect to sounds |

**Reverb parameters:**
- `enabled`: true to enable, false to disable
- `duration_ms`: delay time in milliseconds (typical: 20-100ms)
- `amplitude`: decay factor 0.0-1.0 (typical: 0.3-0.7)

## Dependencies

- **sdl2** - Windowing, input, events
- **glow** - OpenGL wrapper
- **image** - Image loading (replaces SDL2_image)
- **fontdue** - Font rendering (replaces FreeType)
- **rodio** - Audio playback (replaces SoLoud/SDL_mixer)
- **cimgui** - Dear ImGui C bindings (linked externally)

## Building

```bash
cargo build -p stone-rs
```

The library builds as a cdylib (`libstone.dylib` / `libstone.so`) that can be loaded via LuaJIT FFI.

## Usage

Use `--crabstone` flag with rock commands to use the Rust backend:

```bash
cargo run --bin rock -- run game --crabstone
cargo run --bin rock -- stone --crabstone  # Watch mode
```
