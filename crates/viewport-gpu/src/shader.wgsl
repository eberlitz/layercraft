struct CameraUniform {
    view_proj: mat4x4<f32>,
};

@group(0) @binding(0)
var<uniform> cam: CameraUniform;

struct LitIn {
    @location(0) pos: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) color: vec4<f32>,
};

struct LitOut {
    @builtin(position) clip_pos: vec4<f32>,
    @location(0) normal: vec3<f32>,
    @location(1) color: vec4<f32>,
};

@vertex
fn vs_lit(in: LitIn) -> LitOut {
    var out: LitOut;
    out.clip_pos = cam.view_proj * vec4<f32>(in.pos, 1.0);
    out.normal = in.normal;
    out.color = in.color;
    return out;
}

@fragment
fn fs_lit(in: LitOut) -> @location(0) vec4<f32> {
    let n = normalize(in.normal);
    let key_dir = normalize(vec3<f32>(0.45, -0.35, 0.82));
    let fill_dir = normalize(vec3<f32>(-0.55, 0.60, 0.40));
    let diff1 = max(dot(n, key_dir), 0.0) * 0.55;
    let diff2 = max(dot(n, fill_dir), 0.0) * 0.20;
    let hemi = (0.5 + 0.5 * n.z) * 0.18;
    let lit = clamp(0.24 + diff1 + diff2 + hemi, 0.15, 1.05);
    return vec4<f32>(in.color.rgb * lit, in.color.a);
}

struct LineIn {
    @location(0) pos: vec3<f32>,
    @location(1) color: vec4<f32>,
};

struct LineOut {
    @builtin(position) clip_pos: vec4<f32>,
    @location(0) color: vec4<f32>,
};

@vertex
fn vs_line(in: LineIn) -> LineOut {
    var out: LineOut;
    out.clip_pos = cam.view_proj * vec4<f32>(in.pos, 1.0);
    out.color = in.color;
    return out;
}

@fragment
fn fs_line(in: LineOut) -> @location(0) vec4<f32> {
    return in.color;
}

@group(0) @binding(0)
var offscreen_tex: texture_2d<f32>;
@group(0) @binding(1)
var offscreen_samp: sampler;

struct BlitOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_blit(@builtin(vertex_index) idx: u32) -> BlitOut {
    var out: BlitOut;
    let x = f32((idx << 1u) & 2u);
    let y = f32(idx & 2u);
    out.pos = vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
    out.uv = vec2<f32>(x, y);
    return out;
}

@fragment
fn fs_blit(in: BlitOut) -> @location(0) vec4<f32> {
    return textureSample(offscreen_tex, offscreen_samp, in.uv);
}
