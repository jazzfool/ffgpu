struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> VertexOutput {
    var positions = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -3.0),
        vec2<f32>(3.0, 1.0),
        vec2<f32>(-1.0, 1.0)
    );

    var uvs = array<vec2<f32>, 3>(
        vec2<f32>(0.0, 2.0),
        vec2<f32>(2.0, 0.0),
        vec2<f32>(0.0, 0.0)
    );

    var output: VertexOutput;
    output.position = vec4<f32>(positions[vertex_index], 0.0, 1.0);
    output.uv = uvs[vertex_index];
    return output;
}

@group(0) @binding(0) var plane_y: texture_2d<f32>;
@group(0) @binding(1) var plane_uv: texture_2d<f32>;

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let yuv2rgb = mat3x3<f32>($color_matrix);

    var y = textureLoad(plane_y, vec2<u32>(in.position.xy), 0).r;
    var uv = textureLoad(plane_uv, vec2<u32>(floor(in.position.xy / 2)), 0).rg;

    var yuv = vec3<f32>(0.0);
    yuv.x = (y - 0.0625) / 0.8588;
    yuv.y = (uv.x - 0.5) / 0.8784;
    yuv.z = (uv.y - 0.5) / 0.8784;

    var rgb = clamp(yuv * yuv2rgb, vec3<f32>(0), vec3<f32>(1));

    return pow(vec4<f32>(rgb, 1), vec4<f32>(2.2));
}
