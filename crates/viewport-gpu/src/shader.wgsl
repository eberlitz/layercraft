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

struct BeadVertIn {
    @location(0) uv_t_cap: vec4<f32>,
    @location(1) norm_ao: vec3<f32>,
    @location(2) p01: vec4<f32>,
    @location(3) off01: vec4<f32>,
    @location(4) bead_meta: vec4<f32>,
};

@vertex
fn vs_bead(in: BeadVertIn) -> LitOut {
    let pa = in.p01.xy;
    let pb = in.p01.zw;
    let off_a = in.off01.xy;
    let off_b = in.off01.zw;
    let z_mid = in.bead_meta.x;
    let hz = in.bead_meta.y;
    let flags = u32(max(in.bead_meta.z, 0.0) + 0.5);
    let packed_rgb = u32(max(in.bead_meta.w, 0.0) + 0.5);
    let base_rgb = vec3<f32>(
        f32((packed_rgb >> 16u) & 255u) / 255.0,
        f32((packed_rgb >> 8u) & 255u) / 255.0,
        f32(packed_rgb & 255u) / 255.0,
    );

    let d = pb - pa;
    let len = max(length(d), 1e-5);
    let dir = d / len;
    let left = vec2<f32>(-dir.y, dir.x);

    let cap_push = in.uv_t_cap.w;
    var world_pos: vec3<f32>;
    var normal: vec3<f32>;
    let ao = in.norm_ao.z;

    if abs(cap_push) > 0.5 {
        if cap_push < 0.0 {
            let has_cap = (flags & 1u) != 0u;
            let push = select(0.0, min(0.35 * length(off_a), 0.25), has_cap);
            world_pos = vec3<f32>(pa - dir * push, z_mid);
            normal = vec3<f32>(-dir.x, -dir.y, 0.0);
        } else {
            let has_cap = (flags & 2u) != 0u;
            let push = select(0.0, min(0.35 * length(off_b), 0.25), has_cap);
            world_pos = vec3<f32>(pb + dir * push, z_mid);
            normal = vec3<f32>(dir.x, dir.y, 0.0);
        }
    } else {
        let u = in.uv_t_cap.x;
        let v = in.uv_t_cap.y;
        let t = in.uv_t_cap.z;
        let nu = in.norm_ao.x;
        let nz = in.norm_ao.y;

        let center_xy = mix(pa, pb, t);
        let off_vec = mix(off_a, off_b, t);
        world_pos = vec3<f32>(center_xy + off_vec * u, z_mid + hz * v);

        let use_smooth = select((flags & 4u) != 0u, (flags & 8u) != 0u, t > 0.5);
        var norm_xy = left;
        if use_smooth && length(off_vec) > 1e-5 {
            norm_xy = normalize(off_vec);
        }
        normal = normalize(vec3<f32>(norm_xy * nu, nz));
    }

    var out: LitOut;
    out.clip_pos = scene.view_proj * vec4<f32>(world_pos, 1.0);
    out.normal = normal;
    out.color = vec4<f32>(clamp(base_rgb * ao, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
    out.world_pos = world_pos;
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
    let diff_key = max(dot(n, key_dir), 0.0) * 0.52;
    let diff_fill = max(dot(n, fill_dir), 0.0) * 0.20;
    let diff_rim = max(dot(n, rim_dir), 0.0) * 0.12;
    let hemi = (0.5 + 0.5 * n.z) * select(0.16, 0.22, light_mode);
    let ambient = select(0.20, 0.30, light_mode);
    let lit = clamp(ambient + diff_key + diff_fill + diff_rim + hemi, 0.15, 1.04);
    let h_key = normalize(key_dir + v);
    let spec = pow(max(dot(n, h_key), 0.0), 36.0) * select(0.24, 0.16, light_mode);
    let fresnel = pow(1.0 - max(dot(n, v), 0.0), 3.0) * select(0.12, 0.07, light_mode);
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
