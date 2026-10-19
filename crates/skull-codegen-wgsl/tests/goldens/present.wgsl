struct FullscreenVertexOut {
  @builtin(position) position: vec4<f32>,
  @location(0) uv: vec2<f32>,
}

struct FullscreenU {
  uv0: vec2<f32>,
  uv1: vec2<f32>,
}

@group(0) @binding(0) var<uniform> u_FullscreenU: FullscreenU;

struct PresentU_ddcba979 {
  encode_srgb: u32,
  src_origin: vec2<f32>,
  src_extent: vec2<f32>,
  tex_size: vec2<f32>,
  active_size: vec2<f32>,
  dst_scale: vec2<f32>,
}

@group(0) @binding(1) var<uniform> u_PresentU_ddcba979: PresentU_ddcba979;

@group(0) @binding(2) var tex_src_tex: texture_2d<f32>;
@group(0) @binding(3) var samp_src_tex: sampler;

fn linear_channel_to_srgb_ddcba979_float(value: f32) -> f32 {
  var cell_1: f32 = 0.0;
  if (value <= 0.0031308) {
    cell_1 = (value * 12.92);
  } else {
    cell_1 = ((1.055 * pow(value, (1.0 / 2.4))) - 0.055);
  }
  return cell_1;
}

fn linear_to_srgb_ddcba979_Vec3(value: vec3<f32>) -> vec3<f32> {
  return vec3<f32>(linear_channel_to_srgb_ddcba979_float(value.x), linear_channel_to_srgb_ddcba979_float(value.y), linear_channel_to_srgb_ddcba979_float(value.z));
}

fn present_frag_ddcba979_SrcTex_ddcba979_PresentU_ddcba979_Vec2(uv: vec2<f32>) -> vec4<f32> {
  var dst: vec2<f32> = vec2<f32>(0.0);
  var texel: vec2<f32> = vec2<f32>(0.0);
  var scale: vec2<f32> = vec2<f32>(0.0);
  var region: vec2<f32> = vec2<f32>(0.0);
  var center_dist: vec2<f32> = vec2<f32>(0.0);
  var warped: vec2<f32> = vec2<f32>(0.0);
  var lo: vec2<f32> = vec2<f32>(0.0);
  var hi: vec2<f32> = vec2<f32>(0.0);
  var pixel: vec4<f32> = vec4<f32>(0.0);
  var cell_12: vec3<f32> = vec3<f32>(0.0);
  dst = ((uv * u_PresentU_ddcba979.tex_size) / u_PresentU_ddcba979.active_size);
  texel = (u_PresentU_ddcba979.src_origin + (dst * u_PresentU_ddcba979.src_extent));
  scale = max(u_PresentU_ddcba979.dst_scale, vec2<f32>(1.0, 1.0));
  region = (vec2<f32>(0.5, 0.5) - (vec2<f32>(0.5, 0.5) / scale));
  center_dist = (fract(texel) - 0.5);
  warped = (((center_dist - clamp(center_dist, (vec2<f32>(0.0, 0.0) - region), region)) * scale) + 0.5);
  lo = (u_PresentU_ddcba979.src_origin + vec2<f32>(0.5, 0.5));
  hi = ((u_PresentU_ddcba979.src_origin + u_PresentU_ddcba979.src_extent) - vec2<f32>(0.5, 0.5));
  pixel = textureSample(tex_src_tex, samp_src_tex, (clamp((floor(texel) + warped), lo, hi) / u_PresentU_ddcba979.tex_size));
  if (u_PresentU_ddcba979.encode_srgb != 0u) {
    cell_12 = linear_to_srgb_ddcba979_Vec3(vec3<f32>(pixel.x, pixel.y, pixel.z));
  } else {
    cell_12 = vec3<f32>(pixel.x, pixel.y, pixel.z);
  }
  return vec4<f32>(cell_12, pixel.w);
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
  return present_frag_ddcba979_SrcTex_ddcba979_PresentU_ddcba979_Vec2(input.uv);
}
