struct FullscreenVertexOut {
  @builtin(position) position: vec4<f32>,
  @location(0) uv: vec2<f32>,
}

struct FullscreenU {
  uv0: vec2<f32>,
  uv1: vec2<f32>,
}

@group(0) @binding(0) var<uniform> u_FullscreenU: FullscreenU;

struct PresentU {
  src_origin: vec2<f32>,
  src_extent: vec2<f32>,
  tex_size: vec2<f32>,
  active_size: vec2<f32>,
  dst_scale: vec2<f32>,
}

@group(0) @binding(1) var<uniform> u_PresentU: PresentU;

@group(0) @binding(2) var tex_src_tex: texture_2d<f32>;
@group(0) @binding(3) var samp_src_tex: sampler;

fn present_frag_SrcTex_PresentU_Vec2_ret_Vec4(uv: vec2<f32>) -> vec4<f32> {
  var dst: vec2<f32> = vec2<f32>(0.0);
  var texel: vec2<f32> = vec2<f32>(0.0);
  var scale: vec2<f32> = vec2<f32>(0.0);
  var region: vec2<f32> = vec2<f32>(0.0);
  var center_dist: vec2<f32> = vec2<f32>(0.0);
  var warped: vec2<f32> = vec2<f32>(0.0);
  var lo: vec2<f32> = vec2<f32>(0.0);
  var hi: vec2<f32> = vec2<f32>(0.0);
  dst = ((uv * u_PresentU.tex_size) / u_PresentU.active_size);
  texel = (u_PresentU.src_origin + (dst * u_PresentU.src_extent));
  scale = max(u_PresentU.dst_scale, vec2<f32>(1.0, 1.0));
  region = (vec2<f32>(0.5, 0.5) - (vec2<f32>(0.5, 0.5) / scale));
  center_dist = (fract(texel) - 0.5);
  warped = (((center_dist - clamp(center_dist, (vec2<f32>(0.0, 0.0) - region), region)) * scale) + 0.5);
  lo = (u_PresentU.src_origin + vec2<f32>(0.5, 0.5));
  hi = ((u_PresentU.src_origin + u_PresentU.src_extent) - vec2<f32>(0.5, 0.5));
  return textureSample(tex_src_tex, samp_src_tex, (clamp((floor(texel) + warped), lo, hi) / u_PresentU.tex_size));
}

@vertex
fn fullscreen_vertex(@builtin(vertex_index) vertex_index: u32) -> FullscreenVertexOut {
  var output: FullscreenVertexOut;
  let x = f32(vertex_index & 1u) * 2.0 - 1.0;
  let y = f32(vertex_index >> 1u) * 2.0 - 1.0;
  output.position = vec4<f32>(x, y, 0.0, 1.0);
  let corner = vec2<f32>(f32(vertex_index & 1u), f32(vertex_index >> 1u));
  output.uv = u_FullscreenU.uv0 + corner * (u_FullscreenU.uv1 - u_FullscreenU.uv0);
  return output;
}

@fragment
fn fragment_main(input: FullscreenVertexOut) -> @location(0) vec4<f32> {
  return present_frag_SrcTex_PresentU_Vec2_ret_Vec4(input.uv);
}
