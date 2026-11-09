# Stone engine contracts

Standing rules for the Stone runtime in `packages/stone` and `stone-rs`. The
code is the reference for mechanisms; this document keeps the contracts and
the rationale that the code cannot show. A new owner of any of these takes
its live callers in the same change and deletes the superseded state; do not
add a compatibility flag beside the mechanism it replaces.

## Render spaces and resolution

`resolve_frame_view` (`packages/stone/engine/systems/frame_view.rock`) owns
camera framing, scene resolution, presentation, UI scale and coordinate
spaces. Stone supports these game styles without independent snap, scale,
filter and present flags:

| Style | Scene motion | Scene raster | Sprite sampling | UI |
|---|---|---|---|---|
| Pixel perfect | target-pixel steps | fixed virtual target, integer present | nearest | virtual or design UI |
| Smooth pixel art | continuous | integral art density or drawable resolution | nearest | design UI at output resolution |
| Low-resolution baked art | continuous | integral multiples of authored art density | per material | design UI, text and overlays at output resolution |
| Resolution-independent art | continuous | drawable resolution | linear, or an SDF/vector pipeline | design UI at output resolution |

World framing is independent of every row. A camera that shows 40 world units
wide shows 40 world units on any display; resolution changes sampling density
and the vertical extent implied by the aspect ratio, never the meaning of a
world unit. UI is authored in design units against a reference height
(default 1080); ultrawide displays expose more design-space width instead of
stretching the layout.

### Authoring spaces

1. **World**: gameplay units; camera zoom is visible world width.
2. **UI design**: reference-height layout units, owned by `UiRect`.
3. **Output**: logical window units, top-left origin. `screen.*` places
   primitives exactly here.

Drawable pixels and scene texels are raster dimensions, not authoring spaces;
gameplay and layout never use them as positions. There is no separate
camera-space authoring API: world to output transforms are resolved from the
camera and the frame's surfaces.

### Camera

`Camera2D { center, visible_world_width }`. Zoom changes
`visible_world_width`, shake offsets the resolved center, and
`visible_world_height = visible_world_width * framing_height / framing_width`.
The framing aspect is the drawable aspect for `Native`, `OutputScale` and
`CrispArt`, and the fixed virtual target aspect for `PixelPerfect`. Scene
rounding never feeds back into framing. Framing is width-driven and
axis-aligned; camera rotation would invalidate the integral-texel guarantees.
One `FrameView` is active for rendering; a minimap or split view uses a second
pure resolver result bound to another DrawList, not fields on the camera.

### Scene raster policy

`SceneRaster` replaces combinable render-resolution, camera-snap, letterbox
and present-shift flags:

- `Native`: physical drawable resolution.
- `OutputScale(k)`: `round(drawable * k)`, `0 < k <= 1`; a fill-cost choice
  with framing unchanged, presented linearly.
- `CrispArt { asset_ppwu, max_scale }`: the scene size derives from the
  visible world size and the authored pixels per world unit, at the largest
  integral art scale up to `max_scale` that fits the drawable, so a
  nearest-sampled art texel covers an integral block of scene texels while the
  camera moves continuously. If scale 1 does not fit it falls to
  drawable-native size and waives the integral guarantee. Framing never
  changes to make a rung fit. At an integral rung, density wins over integer
  scene extent: the resolver uses `ceil(visible_world_size * density)` texels
  and crops the sub-texel surplus symmetrically from the presentation source
  rectangle; picking uses the same crop.
- `PixelPerfect(size)`: one fixed virtual target, world transform snapped to
  its texel grid, presented at the largest integer scale that fits and
  letterboxed (ultrawide pillarboxes rather than growing virtual width). A
  drawable smaller than the target presents at 1x and crops centrally. Pixels
  per world unit must be integral; otherwise the engine reports an invalid
  configuration rather than rounding the zoom. There is no smooth-present
  remainder: exact pixels and continuous subpixel scrolling conflict on one
  low-resolution raster. A game wanting continuous motion with nearest
  sprites uses `CrispArt`.

Presentation filtering is a property of the raster policy, not a game flag.

**Capacity and active rectangle.** Camera-driven size (`scene_size`) and
allocation (`scene_capacity`) are separate. A `CrispArt` scene allocates
drawable-sized capacity once and renders into the active top-left
subrectangle, so camera zoom causes no reallocation; only a drawable resize or
raster profile change rebuilds targets. This covers every scene target (color,
depth, id, outline, glow, shadow map, pyramid levels, game-injected
intermediates). Derived pyramid levels round with one rule,
`max(1, ceil(active / 2))` per axis, applied centrally. Consequences:

- Binding a canvas viewports and scissors to its active extent; the scissor
  is what confines `glClear`, which ignores the viewport. The pass executor
  scales the fullscreen quad's UV attribute, not a uniform, so passes with
  their own vertex stage follow too.
- Texture clamp-to-edge only knows the capacity edge. Any shader that offsets
  taps clamps to the first and last active texel centers, per sampled input;
  equal active fractions do not map center to center when two textures have
  different texel sizes.
- Fullscreen and signal-sampling shaders never derive normalized UVs from the
  backing texture size.

### Sampling

Sampling is draw state, not camera state. `TextureFilter` (`Nearest`,
`Linear`, `MipStable`) is declared where a texture is produced and inherited
by each draw, which may override it with `sampling:` on `draw_sprite`,
`Material`, `RenderProps`, `RectBatch` and each `Pass` input. The backend
binds immutable samplers per (filter, wrap), so one image is sampled two ways
without duplicate uploads. A bound sampler overrides the texture's wrap as
well as its filter, so a clamp-only sampler would un-tile REPEAT textures.
Linearly sampled atlases need gutters and RGB dilation under transparent
pixels; that is an asset requirement, not a camera workaround.

### UI scale and FrameView

```text
design_to_output = h / reference_height * user_scale
design_height    = h / design_to_output
design_width     = w / design_to_output
```

Device pixel ratio sets drawable density, not design-unit size.

All derived values live in one immutable `FrameView` returned by a pure
resolver. A zero-sized or minimized surface resolves to null (suspended):
rendering skips the frame and input keeps the last presented view.
`scene_output_rect` (top-left, Y-down logical output units),
`scene_output_viewport` (bottom-left drawable pixels for GL) and
`scene_source_rect` (scene texels) are computed together; callers never apply
device-pixel ratio or an FBO Y flip. `FrameView` is the only source for world
to output projection, output to scene-texel picking, render-pass dimensions,
the presentation rectangle and design to output conversion. Nothing
recomputes these from `screen_size`, render resolution or camera globals.
`output_to_scene_texel` returns null outside the presented scene rectangle,
so picking in a letterbox bar hits nothing.

Two frame roles carry the type. `PRESENTED_VIEW` is what the player last saw:
update-stage `mouse_world`, `screen_to_world`, picking and UI input use it.
`RENDER_VIEW` is resolved after update systems position the camera, just
before draw systems; world rendering and deferred world anchors use it, and
after a successful present it becomes the next `PRESENTED_VIEW`. Code that
mutates a camera and needs conversion against the new camera in the same
update calls the pure resolver and uses `screen_to_world_in` or
`world_to_screen_in`; it must not mutate a global half-resolved view.
Unqualified helpers mean `PRESENTED_VIEW` and draw code uses `RENDER_VIEW`.
An update system producing visual content queues a world anchor or projects
in draw; it must not bake a camera read into vertices during update.

`RenderConfig { scene: SceneRaster, ui: UiScale }` and `UiScale.user_scale`
are runtime-mutable. A change resolves at the next render boundary; a frame
never observes half of each configuration.

### Draw spaces and destinations

`DrawSpace` (`World`, `Target`) says what a list's coordinates mean;
`DrawDest` (`Scene`, `Composite`, `Output`) says which surface receives the
pixels. A DrawList is a coordinate convention, a destination and a flush
point; only user lists own a canvas.

| List | Space | Destination | Content |
|---|---|---|---|
| `world` | World | Scene | lit and occluded gameplay |
| `gizmo` | World | Scene | world debug geometry |
| `world_overlay` | World | Output | world-anchored text and effects rasterized at output resolution |
| `pixel_screen` | Target | Composite | virtual-pixel UI for a pixel-perfect game |
| `screen` | Target | Output | raw logical-output primitives |

A frame has two resolutions and three surfaces: the scene canvas (scene
resolution, HDR, pre-grade), `COMPOSITE_CANVAS` (scene resolution, the
finished LDR frame after bloom, tonemap and outline) and the window (drawable
resolution). `present_composite()` is the only resample in the frame.

- `pixel_screen` is Composite, not Scene, because a HUD drawn before post
  would be an HDR input: it would bloom, be tonemapped and be treated as
  geometry by the outline pass.
- `world_overlay` queues world coordinates and projects them at flush against
  the frozen `RENDER_VIEW`. Deferral is required: an Output-destination verb
  bakes its anchor into output coordinates on the CPU, so a caller in
  `@update` would read a view a frame stale. Fixed-UI-size content anchored
  in the world (health bars, nameplates) uses `ui_at_world`, which projects
  its anchor at the final view and lays out a normal `UiRect`.
- `UiRect` lowers once through `design_to_output` into the same output
  emission core as `screen.*`. A pixel-perfect game creates a
  Composite-context `UiRect` whose reference height is the virtual target
  height. Rects carry the context through every carve (spread `{ ..r }`,
  never mint a fresh rect), and `user_scale` does not apply to the composite
  context.

Every verb must honor its list's space. Parallax, if it ever appears, belongs
on a world DrawList's resolved camera binding, not a third authoring space.

Raster atlases are the primary font pipeline. Each font declares one raster
policy: output-density fonts rasterize their base em against the current
drawable density; fixed-raster fonts keep the requested em. Layout and hit
testing stay in logical units. SDF text is a separate, optional pipeline, not
another filter.

## Draw phases

`@late_draw` runs after the world pass has finished. The color pass that
writes entity ids into the pick buffer, the outline overlay and glow
accumulation all happen inside that pass, so a sprite drawn in `@late_draw`
can never win a hover or a click, its `outline:` is not composited that
frame and its glow misses the accumulation. Queued world batches are pooled
and survive the frame, so the geometry still appears one frame late; the
mistake looks like lag or an intermittent hover rather than a wrong pass.
`@late_draw` is for screen-space content over the finished frame. Anything
picked, outlined, depth-sorted or lit belongs in `@draw`.

The reverse trap leaves nothing to see. During draw systems the window is
bound, and `present_composite()` later blits the scene composite over it. A
queued batch is submitted into the composite; an immediate draw
(`draw_sampled_texture`, a raw sprite draw) issued from `@draw` goes straight
to the window and is overwritten, so the region reads as background. The
testbed sim `card_reaches_the_window` pins the correct path.

A custom-shader sprite mesh consumes the batched `Vertex` layout and owns the
y-sort depth remap: the vertex stage writes `position.z` from `pivot_y`
against `uYBounds`. Fork `sprite_batch_shader.rock` rather than writing a
stage from scratch; one that leaves z alone drops the geometry behind every
sprite. A Rock shader's `@uniform` block is packed from the struct the shader
declares, so a forgotten uniform is a compile error, not a black draw.

Outline width is in render-target texels, so one width is the same band on
every subject regardless of its size; size the outline to the subject.

## Scenes, arenas and handles

A `Scene<T>` is a typed, independently disposable lifetime for a root and
its owned resources. It is a weak handle, not a transform or editor
hierarchy. `Arena<T>` is typed entity storage; an `Entity<T>` is a weak
identity that cannot keep its row or Scene alive. Stale handles must not
revive when another Scene, Arena or row is created, and closing one Scene
must not affect another.

A close or discard request makes the Scene unavailable to new lookups
immediately. Physical retirement happens at the runner's frame safe point,
after update and draw consumers return, so already acquired rows remain
usable in the current synchronous work. Graceful close runs `on_close`
callbacks; discard skips them. `on_retire` performs resource retirement on
close, discard and failed construction, before owned arenas are destroyed. A
Scene-owned task poller uses this retirement path; its owner chooses when to
poll it, so a sleeping Scene's tasks do not advance.

`@scene_update` updates each live Scene of its declared root type; explicit
calls select a subset or a custom cadence. A Scene created during an update
joins at the next update boundary. Compatible body reload preserves live
Scene state.

Spatial indexes are derived query structures, not owners of gameplay
entities. They may retain copied geometry and weak payload handles and can be
discarded and rebuilt from their authoritative sources. Queries deduplicate
entries that span multiple cells; gameplay state and digests do not depend on
bucket layout.

Handle descriptors are values: copying one preserves the referent without
transferring ownership, and a uniquely owned resource has one teardown
authority. Handles use their owning abstraction's representation, not a
second hand-built encoding. An optional entity lookup admits absence;
required access raises when the entity is gone. Retirement cancels
owner-bound work, and cancellation is not completion. A borrow cannot be
retained across coroutine suspension: retain the handle and resolve it
explicitly on resumption. Multiple mutable aliases are permitted; that does
not permit invalidating storage while a live borrow still uses it. The
compiler's identity-storage checker (`crates/bone-core/src/identity_storage`)
enforces the storage side of this, subject to package-manifest exemptions
described in the [guide](guide.md).

## Audio

The pipeline is: source libraries and authoring tools, the `sound-core`
crate, compiled bank plus dependency manifest, the Stone realtime runtime,
typed game consumers.

- `sound-core` is dependency-light and tool-neutral: versioned authoring
  project and runtime-bank schemas, validation, deterministic compilation,
  dependency manifests. It must not depend on Stone, a game, a GUI framework
  or a catalog database.
- Stone owns device I/O, decoding, voices, buses, realtime parameter
  smoothing, scheduling and the Rock FFI. It consumes compiled banks and must
  not own source-library indexing, collections, tags, editor state or
  generator recipes.
- Games own gameplay meaning and typed mappings through bank and event
  handles resolved at bank load. They must not grow authoring UIs, cue
  serializers, parallel schedulers or per-play string lookup.
- Add sound-project schema fields only when a real consumer requires them,
  and extend the Rust project and runtime-bank types and Stone execution
  together; do not revive a parallel bank descriptor.
- Position is gameplay context, not authored cue data: world one-shots
  receive a source position at the call site. Music, ambience and persistent
  loops keep separate retained playback paths.

The frame/sample divide is the design problem. Rock runs on the game thread
at frame rate; audio runs at sample rate on a realtime callback thread, and
Rock can never run in the callback (LuaJIT GC, thread-local engine state).
Stepped frame-rate parameters zipper, so any parameter Rock automates is
slewed sample-side. Once we own DSP in the callback there is no allocation
and no lock there: parameters via atomics, events via an SPSC ring. Rock maps
gameplay values into typed parameter slots resolved when a bank loads and
Rust executes the compiled event, authored envelopes and smoothing; there is
no string-keyed per-frame parameter registry. Verify audio by running it:
building and grepping prove nothing. Each change needs an in-game ear check
and a headless sim that the path does not crash or drop out.

Runtime facts that are easy to get wrong: voice liveness is map membership,
not `Player::empty()`, because rodio's `stop()` only sets an atomic and an
`empty()` answer races with our own stop calls; voice ids are never reused.
The lowpass "off" position is `0.45 * rate`, deliberately not transparent,
to avoid a bypass branch and its discontinuity. A bus wrapper must be an
infinite source (`inner.next().unwrap_or(0.0)`) and a bare `MixerSource`
must never be a child: an empty `MixerSource` returns `None` and a parent
drops a child that returns `None`, so a bus would die permanently the first
instant it fell silent. Keep all bus levels at the device rate and channel
count, since every nesting level adds a per-sample resampling stage.

rodio is a playback library, and the voice model has consumed the live
parameters `Player` exposes: control is a 5 ms mutex-guarded step, the
`Player` chain is hardcoded, and there is no "play at sample T". Owning the
`Source` (our own `Source` impl before `append`, with atomic parameters)
bypasses rodio's control path and is the seam for per-voice DSP. Audio on
the GPU is rejected: the interesting DSP is serial per-sample recurrence,
realtime audio needs a rounding error of one CPU core, and dispatch plus
readback latency is milliseconds. The remaining engine ladder (offline DSP
vocabulary, a sample-accurate scheduler, and a Rock-to-DSP backend) and the
decision on when to stop building inside rodio are tracked on the issue
board.

## Text

Highly styled text is a rendering input (`packages/stone/engine/ui/text_art.rock`):
sharp at the surface that finally displays it, able to fan one glyph source
into shadows, emission, bloom and custom shaders, caching only the stable
work. Standing rules:

- Text is centered vertically by cap height, one rule for all text, so labels
  do not move with descenders. Migrating a legacy em-height size converts by
  the face's cap/em ratio.
- Cap demand must go through the cap/em ratio: `required_ppem =
  target_cap_px * U_em / U_cap * font_coverage_oversampling`, rounded upward
  to a coverage bucket. Comparing cap pixels directly with an em-sized bucket
  was a real defect. Demand above the bucket range errors instead of using an
  undersized atlas.
- Coverage oversampling defaults to `1.15`; `1.30` crosses a bucket boundary
  for a 12 px smooth font, and `1.00` and `1.15` select the same bucket.
- Every builder returns an independent `TextLayout`. A shared scratch layout
  let later consumers overwrite a layout still being observed during a menu
  transition; do not reintroduce reuse across callers.
- Stylized UI renders on its own output-density HDR surface with local bloom.
  It does not enter the world scene to reach scene bloom and does not pass
  through the post-bloom rgba8 UI surface. Bloom participation is explicit per
  layer; a white face is not emissive because its value approaches one. The
  compositor's two meanings are display-referred base color and emission.
- `TextArtEffectBounds` and `text_art_glyph_ensure` are the only padding
  owner; callers never maintain a gutter or fill-ratio correction.
- Wide blur is a downsample/blur/upsample pyramid, never repeated
  full-resolution separable passes, and a permanent atlas size only replaces
  one hidden ceiling with another.
- Time, mouse and spring state never enter a static cache key.

Rich text (`rich_flow.rock`): the span core is one line, one face and one
size. Newline, carriage return, tabs and bytes outside the supported ASCII
atlas reject loudly, which keeps total-width alignment exact without a
partial paragraph engine. Paragraph flow is a separate API on top of the
same renderer; it wraps across span boundaries, a style boundary must not
create a word boundary, and a fragment's logical advance bounds, not its ink
bounds, form the hover target. Layout geometry is separate from paint: color
and hover tint need no rewrap. SDF is a separate optional pipeline that does
not replace fixed pixel-font rasterization, and an SDF A/B must use the
actual selected smooth UI font, not only the pixel font. Rich text adds no
effect field (outline, shadow per span) until one route has complete
semantics, transport, bounds and ordering. Author-written markup, when it
arrives, is parsed at compile time into the same `[TextSpan]` the typed API
accepts; dynamic game data builds typed spans directly, never markup.
