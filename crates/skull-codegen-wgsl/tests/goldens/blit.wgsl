struct FullscreenVertexIn {
  @location(0) position: vec2<f32>,
  @location(1) uv: vec2<f32>,
}

struct FullscreenVertexOut {
  @builtin(position) position: vec4<f32>,
  @location(0) uv: vec2<f32>,
}

@group(0) @binding(0) var tex_input_tex: texture_2d<f32>;
@group(0) @binding(1) var samp_input_tex: sampler;

fn blit_frag_InputTex_Vec2_ret_Vec4(uv: vec2<f32>) -> vec4<f32> {
  return textureSample(tex_input_tex, samp_input_tex, uv);
}

@vertex
fn fullscreen_vertex(input: FullscreenVertexIn) -> FullscreenVertexOut {
  var output: FullscreenVertexOut;
  output.position = vec4<f32>(input.position, 0.0, 1.0);
  output.uv = input.uv;
  return output;
}

@fragment
fn fragment_main(input: FullscreenVertexOut) -> @location(0) vec4<f32> {
  return blit_frag_InputTex_Vec2_ret_Vec4(input.uv);
}
