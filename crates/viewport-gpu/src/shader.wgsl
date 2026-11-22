struct SceneUniform {
    view_proj: mat4x4<f32>,
    eye_pos: vec4<f32>,
    bg_top: vec4<f32>,
    bg_bottom: vec4<f32>,
    bg_glow: vec4<f32>,
};

@group(0) @binding(0)
var<uniform> scene: SceneUniform;

struct BgOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_bg(@builtin(vertex_index) idx: u32) -> BgOut {
    var out: BgOut;
    let x = f32((idx << 1u) & 2u);
    let y = f32(idx & 2u);
    out.pos = vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 1.0, 1.0);
    out.uv = vec2<f32>(x, y);
    return out;
}

@fragment
fn fs_bg(in: BgOut) -> @location(0) vec4<f32> {
    let base = mix(scene.bg_top.rgb, scene.bg_bottom.rgb, clamp(in.uv.y, 0.0, 1.0));
    let d = length((in.uv - vec2<f32>(0.5, 0.42)) * vec2<f32>(1.15, 1.35));
    let glow = exp(-d * d * 2.6) * scene.bg_glow.a;
    return vec4<f32>(mix(base, scene.bg_glow.rgb, glow), 1.0);
}

struct LitIn {
    @location(0) pos: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) color: vec4<f32>,
};

struct LitOut {
    @builtin(position) clip_pos: vec4<f32>,
    @location(0) normal: vec3<f32>,
    @location(1) color: vec4<f32>,
    @location(2) world_pos: vec3<f32>,
};

@vertex
fn vs_lit(in: LitIn) -> LitOut {
    var out: LitOut;
    out.clip_pos = scene.view_proj * vec4<f32>(in.pos, 1.0);
    out.normal = in.normal;
    out.color = in.color;
    out.world_pos = in.pos;
    return out;
}

@fragment
fn fs_lit(in: LitOut) -> @location(0) vec4<f32> {
    let light_mode = scene.eye_pos.w > 0.5;
    let v = normalize(scene.eye_pos.xyz - in.world_pos);
    var n = normalize(in.normal);
    if dot(n, v) < 0.0 {
        n = -n;
    }
    let key_dir = normalize(vec3<f32>(0.42, -0.38, 0.82));
    let fill_dir = normalize(vec3<f32>(-0.55, 0.58, 0.44));
    let rim_dir = normalize(vec3<f32>(0.0, 0.72, 0.68));
    let diff_key = max(dot(n, key_dir), 0.0) * 0.56;
    let diff_fill = max(dot(n, fill_dir), 0.0) * 0.24;
    let diff_rim = max(dot(n, rim_dir), 0.0) * 0.14;
    let hemi = (0.5 + 0.5 * n.z) * select(0.18, 0.26, light_mode);
    let ambient = select(0.24, 0.36, light_mode);
    let lit = clamp(ambient + diff_key + diff_fill + diff_rim + hemi, 0.18, 1.08);
    let h_key = normalize(key_dir + v);
    let spec = pow(max(dot(n, h_key), 0.0), 36.0) * select(0.22, 0.16, light_mode);
    let fresnel = pow(1.0 - max(dot(n, v), 0.0), 3.0) * select(0.14, 0.08, light_mode);
    let col_rgb = clamp(
        in.color.rgb * lit + vec3<f32>(spec) + in.color.rgb * fresnel,
        vec3<f32>(0.0),
        vec3<f32>(1.0),
    );
    return vec4<f32>(col_rgb, in.color.a);
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
    out.clip_pos = scene.view_proj * vec4<f32>(in.pos, 1.0);
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
