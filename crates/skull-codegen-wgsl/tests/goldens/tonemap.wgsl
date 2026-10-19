struct FullscreenVertexOut {
  @builtin(position) position: vec4<f32>,
  @location(0) uv: vec2<f32>,
}

struct FullscreenU {
  uv0: vec2<f32>,
  uv1: vec2<f32>,
}

@group(0) @binding(0) var<uniform> u_FullscreenU: FullscreenU;

struct TonemapU_ddcba979 {
  exposure: f32,
  bloom_intensity: f32,
  bloom_uv: vec2<f32>,
  bloom_uv_lo: vec2<f32>,
  bloom_uv_hi: vec2<f32>,
}

@group(0) @binding(1) var<uniform> u_TonemapU_ddcba979: TonemapU_ddcba979;

@group(0) @binding(2) var tex_scene_tex: texture_2d<f32>;
@group(0) @binding(3) var samp_scene_tex: sampler;
@group(0) @binding(4) var tex_bloom_tex: texture_2d<f32>;
@group(0) @binding(5) var samp_bloom_tex: sampler;

fn aces_film_ddcba979_Vec3(x: vec3<f32>) -> vec3<f32> {
  var a: f32 = 0.0;
  var b: f32 = 0.0;
  var c: f32 = 0.0;
  var d: f32 = 0.0;
  var e: f32 = 0.0;
  a = 2.51;
  b = 0.03;
  c = 2.43;
  d = 0.59;
  e = 0.14;
  return clamp(((x * ((a * x) + b)) / ((x * ((c * x) + d)) + e)), vec3<f32>(0.0), vec3<f32>(1.0));
}

fn tonemap_frag_ddcba979_TonemapTex_ddcba979_TonemapU_ddcba979_Vec2(uv: vec2<f32>) -> vec4<f32> {
  var scene_pixel: vec4<f32> = vec4<f32>(0.0);
  var bloom_pixel: vec4<f32> = vec4<f32>(0.0);
  var combined: vec3<f32> = vec3<f32>(0.0);
  scene_pixel = textureSample(tex_scene_tex, samp_scene_tex, uv);
  bloom_pixel = textureSample(tex_bloom_tex, samp_bloom_tex, clamp((uv * u_TonemapU_ddcba979.bloom_uv), u_TonemapU_ddcba979.bloom_uv_lo, u_TonemapU_ddcba979.bloom_uv_hi));
  combined = (vec3<f32>(scene_pixel.x, scene_pixel.y, scene_pixel.z) + (vec3<f32>(bloom_pixel.x, bloom_pixel.y, bloom_pixel.z) * u_TonemapU_ddcba979.bloom_intensity));
  combined = (combined * u_TonemapU_ddcba979.exposure);
  combined = aces_film_ddcba979_Vec3(combined);
  return vec4<f32>(combined, scene_pixel.w);
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
  return tonemap_frag_ddcba979_TonemapTex_ddcba979_TonemapU_ddcba979_Vec2(input.uv);
}
