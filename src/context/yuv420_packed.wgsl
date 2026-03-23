struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@group(0) @binding(0) var plane_y: texture_2d<f32>;
@group(0) @binding(1) var plane_uv: texture_2d<f32>;

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let yuv2rgb = mat3x3<f32>($color_matrix);

    var y = textureLoad(plane_y, vec2<u32>(in.position.xy), 0).r * $scale;
    var uv = textureLoad(plane_uv, vec2<u32>(floor(in.position.xy / 2)), 0).rg * $scale;

    var yuv = vec3<f32>(0.0);
    yuv.x = (y - 0.0625) / 0.8588;
    yuv.y = (uv.x - 0.5) / 0.8784;
    yuv.z = (uv.y - 0.5) / 0.8784;

    var rgb = clamp(yuv * yuv2rgb, vec3<f32>(0), vec3<f32>(1));

    return vec4<f32>(rgb, 1);
}
