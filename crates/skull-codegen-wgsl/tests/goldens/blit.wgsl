struct FullscreenVertexOut {
  @builtin(position) position: vec4<f32>,
  @location(0) uv: vec2<f32>,
}

struct FullscreenU {
  uv0: vec2<f32>,
  uv1: vec2<f32>,
}

@group(0) @binding(0) var<uniform> u_FullscreenU: FullscreenU;

@group(0) @binding(1) var tex_input_tex: texture_2d<f32>;
@group(0) @binding(2) var samp_input_tex: sampler;

fn blit_frag_ddcba979_InputTex_ddcba979_Vec2(uv: vec2<f32>) -> vec4<f32> {
  return textureSample(tex_input_tex, samp_input_tex, uv);
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
  return blit_frag_ddcba979_InputTex_ddcba979_Vec2(input.uv);
}
