//! `wgpu 30` + `egui-wgpu 0.37` 4× MSAA 3D build plate, mesh, and sliced toolpath renderer.

use std::ops::Range;
use std::sync::Arc;

use glam::Mat4;
use layercraft_viewport::{
    LayerMoveStep, LineVertex, SceneVertex, ToolpathGeometry, build_nozzle_marker_mesh,
};
use wgpu::util::DeviceExt as _;

const MSAA_SAMPLES: u32 = 4;

/// Immutable GPU-uploadable scene geometry produced when mesh/plate/toolpaths change.
#[derive(Debug, Default)]
pub struct GpuSceneSnapshot {
    pub revision: u64,
    pub bed_plate_verts: Vec<[f32; 10]>,
    pub bed_lines: Vec<[f32; 7]>,
    pub mesh_verts: Vec<[f32; 10]>,
    pub modifier_verts: Vec<[f32; 10]>,
    pub ghost_verts: Vec<[f32; 10]>,
    pub toolpath_verts: Vec<[f32; 10]>,
    pub layer_ranges: Vec<Range<u32>>,
    pub travel_lines: Vec<[f32; 7]>,
    pub travel_layer_ranges: Vec<Range<u32>>,
    pub layer_steps: Vec<Vec<LayerMoveStep>>,
}

impl GpuSceneSnapshot {
    /// Pack CPU vertex arrays into `bytemuck`-friendly float arrays without `unsafe`.
    pub fn from_geometry(
        revision: u64,
        bed_plate: &[SceneVertex],
        bed: &[LineVertex],
        mesh: &[SceneVertex],
        modifiers: &[SceneVertex],
        ghost: &[SceneVertex],
        toolpaths: &ToolpathGeometry,
    ) -> Self {
        Self {
            revision,
            bed_plate_verts: bed_plate
                .iter()
                .copied()
                .map(SceneVertex::to_packed)
                .collect(),
            bed_lines: bed.iter().copied().map(LineVertex::to_packed).collect(),
            mesh_verts: mesh.iter().copied().map(SceneVertex::to_packed).collect(),
            modifier_verts: modifiers
                .iter()
                .copied()
                .map(SceneVertex::to_packed)
                .collect(),
            ghost_verts: ghost.iter().copied().map(SceneVertex::to_packed).collect(),
            toolpath_verts: toolpaths
                .vertices
                .iter()
                .copied()
                .map(SceneVertex::to_packed)
                .collect(),
            layer_ranges: toolpaths.layer_ranges.clone(),
            travel_lines: toolpaths
                .travel_lines
                .iter()
                .copied()
                .map(LineVertex::to_packed)
                .collect(),
            travel_layer_ranges: toolpaths.travel_layer_ranges.clone(),
            layer_steps: toolpaths.layer_steps.clone(),
        }
    }
}

/// Per-frame paint callback submitted to `egui::Painter` for hardware 3D rendering.
pub struct ViewportCallback {
    pub scene: Arc<GpuSceneSnapshot>,
    pub view_proj: Mat4,
    pub eye_pos: [f32; 3],
    pub dark_mode: bool,
    pub viewport_size_px: [u32; 2],
    pub draw_mesh: bool,
    pub draw_ghost: bool,
    pub toolpath_range: Range<u32>,
    pub travel_range: Range<u32>,
    pub nozzle_pos: Option<[f32; 3]>,
}

struct OffscreenTarget {
    width: u32,
    height: u32,
    _msaa_color_tex: wgpu::Texture,
    msaa_color_view: wgpu::TextureView,
    _resolve_color_tex: wgpu::Texture,
    resolve_color_view: wgpu::TextureView,
    _depth_tex: wgpu::Texture,
    depth_view: wgpu::TextureView,
    blit_bind_group: wgpu::BindGroup,
}

struct UploadedBuffers {
    revision: u64,
    bed_plate_buf: Option<(wgpu::Buffer, u32)>,
    bed_buf: Option<(wgpu::Buffer, u32)>,
    mesh_buf: Option<(wgpu::Buffer, u32)>,
    modifier_buf: Option<(wgpu::Buffer, u32)>,
    ghost_buf: Option<(wgpu::Buffer, u32)>,
    toolpath_buf: Option<(wgpu::Buffer, u32)>,
    travel_buf: Option<(wgpu::Buffer, u32)>,
}

/// Persistent GPU pipelines and cached scene vertex buffers stored in `egui_wgpu` resources.
pub struct ViewportGpuRenderer {
    target_format: wgpu::TextureFormat,
    bg_pipeline: wgpu::RenderPipeline,
    lit_pipeline: wgpu::RenderPipeline,
    translucent_pipeline: wgpu::RenderPipeline,
    line_pipeline: wgpu::RenderPipeline,
    blit_pipeline: wgpu::RenderPipeline,
    blit_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    cam_buf: wgpu::Buffer,
    cam_bind_group: wgpu::BindGroup,
    offscreen: Option<OffscreenTarget>,
    uploaded: Option<UploadedBuffers>,
}

/// Install the LayerCraft 3D renderer into `egui_wgpu::RenderState` once at startup.
pub fn install_renderer(rs: &egui_wgpu::RenderState) {
    let renderer = ViewportGpuRenderer::new(&rs.device, rs.target_format);
    rs.renderer.write().callback_resources.insert(renderer);
}

fn make_lit_pipeline(
    device: &wgpu::Device,
    layout: &wgpu::PipelineLayout,
    shader: &wgpu::ShaderModule,
    target_format: wgpu::TextureFormat,
    depth_write: bool,
    label: &str,
) -> wgpu::RenderPipeline {
    let lit_attrs = wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x3, 2 => Float32x4];
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: Some("vs_lit"),
            compilation_options: Default::default(),
            buffers: &[Some(wgpu::VertexBufferLayout {
                array_stride: 40,
                step_mode: wgpu::VertexStepMode::Vertex,
                attributes: &lit_attrs,
            })],
        },
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            cull_mode: None,
            ..Default::default()
        },
        depth_stencil: Some(wgpu::DepthStencilState {
            format: wgpu::TextureFormat::Depth32Float,
            depth_write_enabled: Some(depth_write),
            depth_compare: Some(wgpu::CompareFunction::Less),
            stencil: wgpu::StencilState::default(),
            bias: wgpu::DepthBiasState::default(),
        }),
        multisample: wgpu::MultisampleState {
            count: MSAA_SAMPLES,
            ..Default::default()
        },
        fragment: Some(wgpu::FragmentState {
            module: shader,
            entry_point: Some("fs_lit"),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: target_format,
                blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        multiview_mask: None,
        cache: None,
    })
}

fn pack_scene_uniform(view_proj: Mat4, eye_pos: [f32; 3], dark_mode: bool) -> [f32; 32] {
    let vp = view_proj.to_cols_array();
    let light_flag = if dark_mode { 0.0_f32 } else { 1.0_f32 };
    let (bg_top, bg_bottom, bg_glow) = if dark_mode {
        (
            [0.068_f32, 0.078, 0.098, 1.0],
            [0.108_f32, 0.122, 0.150, 1.0],
            [0.145_f32, 0.185, 0.220, 0.32],
        )
    } else {
        (
            [0.835_f32, 0.865, 0.905, 1.0],
            [0.935_f32, 0.948, 0.968, 1.0],
            [1.000_f32, 1.000, 1.000, 0.48],
        )
    };
    [
        vp[0],
        vp[1],
        vp[2],
        vp[3],
        vp[4],
        vp[5],
        vp[6],
        vp[7],
        vp[8],
        vp[9],
        vp[10],
        vp[11],
        vp[12],
        vp[13],
        vp[14],
        vp[15],
        eye_pos[0],
        eye_pos[1],
        eye_pos[2],
        light_flag,
        bg_top[0],
        bg_top[1],
        bg_top[2],
        bg_top[3],
        bg_bottom[0],
        bg_bottom[1],
        bg_bottom[2],
        bg_bottom[3],
        bg_glow[0],
        bg_glow[1],
        bg_glow[2],
        bg_glow[3],
    ]
}

impl ViewportGpuRenderer {
    /// Compile WGSL shaders and construct the 4× MSAA studio backdrop, lit, translucent, line, and blit pipelines.
    pub fn new(device: &wgpu::Device, target_format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("layercraft_viewport_shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
        });

        let initial_uniform = pack_scene_uniform(Mat4::IDENTITY, [0.0, 0.0, 100.0], true);
        let cam_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("layercraft_scene_uniform"),
            contents: bytemuck::cast_slice(&initial_uniform),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });

        let cam_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("layercraft_scene_uniform_layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });

        let cam_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("layercraft_scene_uniform_bg"),
            layout: &cam_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: cam_buf.as_entire_binding(),
            }],
        });

        let scene_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("layercraft_scene_pipeline_layout"),
            bind_group_layouts: &[Some(&cam_layout)],
            immediate_size: 0,
        });

        let bg_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("layercraft_bg_pipeline"),
            layout: Some(&scene_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_bg"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: wgpu::TextureFormat::Depth32Float,
                depth_write_enabled: Some(false),
                depth_compare: Some(wgpu::CompareFunction::Always),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState {
                count: MSAA_SAMPLES,
                ..Default::default()
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_bg"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });

        let lit_pipeline = make_lit_pipeline(
            device,
            &scene_layout,
            &shader,
            target_format,
            true,
            "layercraft_lit_pipeline",
        );
        let translucent_pipeline = make_lit_pipeline(
            device,
            &scene_layout,
            &shader,
            target_format,
            false,
            "layercraft_translucent_pipeline",
        );

        let line_attrs = wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x4];
        let line_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("layercraft_line_pipeline"),
            layout: Some(&scene_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_line"),
                compilation_options: Default::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: 28,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &line_attrs,
                })],
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::LineList,
                ..Default::default()
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: wgpu::TextureFormat::Depth32Float,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::LessEqual),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState {
                count: MSAA_SAMPLES,
                ..Default::default()
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_line"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });

        let blit_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("layercraft_blit_layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let blit_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("layercraft_blit_pipeline_layout"),
            bind_group_layouts: &[Some(&blit_layout)],
            immediate_size: 0,
        });

        let blit_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("layercraft_blit_pipeline"),
            layout: Some(&blit_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_blit"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_blit"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("layercraft_blit_sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        Self {
            target_format,
            bg_pipeline,
            lit_pipeline,
            translucent_pipeline,
            line_pipeline,
            blit_pipeline,
            blit_layout,
            sampler,
            cam_buf,
            cam_bind_group,
            offscreen: None,
            uploaded: None,
        }
    }

    fn ensure_offscreen(&mut self, device: &wgpu::Device, width: u32, height: u32) {
        let w = width.max(1);
        let h = height.max(1);
        if let Some(cur) = &self.offscreen
            && cur.width == w
            && cur.height == h
        {
            return;
        }

        let size = wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        };

        let msaa_color_tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("layercraft_offscreen_msaa_color"),
            size,
            mip_level_count: 1,
            sample_count: MSAA_SAMPLES,
            dimension: wgpu::TextureDimension::D2,
            format: self.target_format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let msaa_color_view = msaa_color_tex.create_view(&wgpu::TextureViewDescriptor::default());

        let resolve_color_tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("layercraft_offscreen_resolve_color"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: self.target_format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let resolve_color_view =
            resolve_color_tex.create_view(&wgpu::TextureViewDescriptor::default());

        let depth_tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("layercraft_offscreen_msaa_depth"),
            size,
            mip_level_count: 1,
            sample_count: MSAA_SAMPLES,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Depth32Float,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let depth_view = depth_tex.create_view(&wgpu::TextureViewDescriptor::default());

        let blit_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("layercraft_blit_bg"),
            layout: &self.blit_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&resolve_color_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });

        self.offscreen = Some(OffscreenTarget {
            width: w,
            height: h,
            _msaa_color_tex: msaa_color_tex,
            msaa_color_view,
            _resolve_color_tex: resolve_color_tex,
            resolve_color_view,
            _depth_tex: depth_tex,
            depth_view,
            blit_bind_group,
        });
    }

    fn ensure_scene(&mut self, device: &wgpu::Device, scene: &GpuSceneSnapshot) {
        if self
            .uploaded
            .as_ref()
            .is_some_and(|u| u.revision == scene.revision)
        {
            return;
        }
        let bed_plate_buf = upload_slice(device, "layercraft_bed_plate", &scene.bed_plate_verts);
        let bed_buf = upload_slice(device, "layercraft_bed_lines", &scene.bed_lines);
        let mesh_buf = upload_slice(device, "layercraft_mesh_verts", &scene.mesh_verts);
        let modifier_buf = upload_slice(device, "layercraft_modifier_verts", &scene.modifier_verts);
        let ghost_buf = upload_slice(device, "layercraft_ghost_verts", &scene.ghost_verts);
        let toolpath_buf = upload_slice(device, "layercraft_toolpath_verts", &scene.toolpath_verts);
        let travel_buf = upload_slice(device, "layercraft_travel_lines", &scene.travel_lines);
        self.uploaded = Some(UploadedBuffers {
            revision: scene.revision,
            bed_plate_buf,
            bed_buf,
            mesh_buf,
            modifier_buf,
            ghost_buf,
            toolpath_buf,
            travel_buf,
        });
    }
}

fn upload_slice<T: bytemuck::NoUninit>(
    device: &wgpu::Device,
    label: &str,
    data: &[T],
) -> Option<(wgpu::Buffer, u32)> {
    if data.is_empty() {
        return None;
    }
    let buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some(label),
        contents: bytemuck::cast_slice(data),
        usage: wgpu::BufferUsages::VERTEX,
    });
    let count = u32::try_from(data.len()).unwrap_or(u32::MAX);
    Some((buf, count))
}

impl egui_wgpu::CallbackTrait for ViewportCallback {
    fn prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        _screen: &egui_wgpu::ScreenDescriptor,
        encoder: &mut wgpu::CommandEncoder,
        resources: &mut egui_wgpu::CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        let Some(renderer) = resources.get_mut::<ViewportGpuRenderer>() else {
            return Vec::new();
        };

        let w = self.viewport_size_px[0].max(1);
        let h = self.viewport_size_px[1].max(1);
        renderer.ensure_offscreen(device, w, h);
        renderer.ensure_scene(device, &self.scene);
        let uniform = pack_scene_uniform(self.view_proj, self.eye_pos, self.dark_mode);
        queue.write_buffer(&renderer.cam_buf, 0, bytemuck::cast_slice(&uniform));

        let Some(offscreen) = &renderer.offscreen else {
            return Vec::new();
        };
        let Some(uploaded) = &renderer.uploaded else {
            return Vec::new();
        };
        let nozzle_buf = self.nozzle_pos.and_then(|tip| {
            let verts: Vec<[f32; 10]> = build_nozzle_marker_mesh(tip, self.dark_mode)
                .into_iter()
                .map(SceneVertex::to_packed)
                .collect();
            upload_slice(device, "layercraft_nozzle_marker", &verts)
        });

        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("layercraft_offscreen_3d"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &offscreen.msaa_color_view,
                depth_slice: None,
                resolve_target: Some(&offscreen.resolve_color_view),
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Discard,
                },
            })],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: &offscreen.depth_view,
                depth_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Clear(1.0),
                    store: wgpu::StoreOp::Discard,
                }),
                stencil_ops: None,
            }),
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });

        // 1. Studio gradient backdrop
        pass.set_pipeline(&renderer.bg_pipeline);
        pass.set_bind_group(0, &renderer.cam_bind_group, &[]);
        pass.draw(0..3, 0..1);

        // 2. Physical matte build plate
        if let Some((buf, count)) = &uploaded.bed_plate_buf {
            pass.set_pipeline(&renderer.lit_pipeline);
            pass.set_bind_group(0, &renderer.cam_bind_group, &[]);
            pass.set_vertex_buffer(0, buf.slice(..));
            pass.draw(0..*count, 0..1);
        }

        // 3. Bed grid lines & origin triad
        if let Some((buf, count)) = &uploaded.bed_buf {
            pass.set_pipeline(&renderer.line_pipeline);
            pass.set_bind_group(0, &renderer.cam_bind_group, &[]);
            pass.set_vertex_buffer(0, buf.slice(..));
            pass.draw(0..*count, 0..1);
        }

        // 4. Opaque model mesh (Prepare) or sliced toolpath beads (Preview)
        if self.draw_mesh
            && let Some((buf, count)) = &uploaded.mesh_buf
        {
            pass.set_pipeline(&renderer.lit_pipeline);
            pass.set_bind_group(0, &renderer.cam_bind_group, &[]);
            pass.set_vertex_buffer(0, buf.slice(..));
            pass.draw(0..*count, 0..1);
        }

        if !self.toolpath_range.is_empty()
            && let Some((buf, count)) = &uploaded.toolpath_buf
        {
            let start = self.toolpath_range.start.min(*count);
            let end = self.toolpath_range.end.min(*count);
            if end > start {
                pass.set_pipeline(&renderer.lit_pipeline);
                pass.set_bind_group(0, &renderer.cam_bind_group, &[]);
                pass.set_vertex_buffer(0, buf.slice(..));
                pass.draw(start..end, 0..1);
            }
        }

        if !self.travel_range.is_empty()
            && let Some((buf, count)) = &uploaded.travel_buf
        {
            let start = self.travel_range.start.min(*count);
            let end = self.travel_range.end.min(*count);
            if end > start {
                pass.set_pipeline(&renderer.line_pipeline);
                pass.set_bind_group(0, &renderer.cam_bind_group, &[]);
                pass.set_vertex_buffer(0, buf.slice(..));
                pass.draw(start..end, 0..1);
            }
        }

        if let Some((buf, count)) = &nozzle_buf {
            pass.set_pipeline(&renderer.lit_pipeline);
            pass.set_bind_group(0, &renderer.cam_bind_group, &[]);
            pass.set_vertex_buffer(0, buf.slice(..));
            pass.draw(0..*count, 0..1);
        }

        // 5. Translucent modifiers & ghost mesh (depth-tested, no depth write)
        if self.draw_mesh
            && let Some((buf, count)) = &uploaded.modifier_buf
        {
            pass.set_pipeline(&renderer.translucent_pipeline);
            pass.set_bind_group(0, &renderer.cam_bind_group, &[]);
            pass.set_vertex_buffer(0, buf.slice(..));
            pass.draw(0..*count, 0..1);
        }

        if self.draw_ghost
            && let Some((buf, count)) = &uploaded.ghost_buf
        {
            pass.set_pipeline(&renderer.translucent_pipeline);
            pass.set_bind_group(0, &renderer.cam_bind_group, &[]);
            pass.set_vertex_buffer(0, buf.slice(..));
            pass.draw(0..*count, 0..1);
        }

        Vec::new()
    }

    fn paint(
        &self,
        info: egui::PaintCallbackInfo,
        pass: &mut wgpu::RenderPass<'static>,
        resources: &egui_wgpu::CallbackResources,
    ) {
        let Some(renderer) = resources.get::<ViewportGpuRenderer>() else {
            return;
        };
        let Some(offscreen) = &renderer.offscreen else {
            return;
        };
        let vp = info.viewport_in_pixels();
        if vp.width_px <= 0 || vp.height_px <= 0 {
            return;
        }
        pass.set_viewport(
            vp.left_px as f32,
            vp.top_px as f32,
            vp.width_px as f32,
            vp.height_px as f32,
            0.0,
            1.0,
        );
        pass.set_pipeline(&renderer.blit_pipeline);
        pass.set_bind_group(0, &offscreen.blit_bind_group, &[]);
        pass.draw(0..3, 0..1);
    }
}
