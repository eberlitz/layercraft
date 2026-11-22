//! `wgpu 30` + `egui-wgpu 0.37` 3D build plate, mesh, and sliced toolpath renderer.

use std::ops::Range;
use std::sync::Arc;

use glam::Mat4;
use layercraft_viewport::{LineVertex, SceneVertex};
use wgpu::util::DeviceExt as _;

/// Immutable GPU-uploadable scene geometry produced when mesh/plate/toolpaths change.
#[derive(Debug, Default)]
pub struct GpuSceneSnapshot {
    pub revision: u64,
    pub bed_lines: Vec<[f32; 7]>,
    pub mesh_verts: Vec<[f32; 10]>,
    pub ghost_verts: Vec<[f32; 10]>,
    pub toolpath_verts: Vec<[f32; 10]>,
    pub layer_ranges: Vec<Range<u32>>,
}

impl GpuSceneSnapshot {
    /// Pack CPU vertex arrays into `bytemuck`-friendly float arrays without `unsafe`.
    pub fn from_geometry(
        revision: u64,
        bed: &[LineVertex],
        mesh: &[SceneVertex],
        ghost: &[SceneVertex],
        toolpaths: &[SceneVertex],
        layer_ranges: Vec<Range<u32>>,
    ) -> Self {
        Self {
            revision,
            bed_lines: bed.iter().copied().map(LineVertex::to_packed).collect(),
            mesh_verts: mesh.iter().copied().map(SceneVertex::to_packed).collect(),
            ghost_verts: ghost.iter().copied().map(SceneVertex::to_packed).collect(),
            toolpath_verts: toolpaths
                .iter()
                .copied()
                .map(SceneVertex::to_packed)
                .collect(),
            layer_ranges,
        }
    }
}

/// Per-frame paint callback submitted to `egui::Painter` for hardware 3D rendering.
pub struct ViewportCallback {
    pub scene: Arc<GpuSceneSnapshot>,
    pub view_proj: Mat4,
    pub viewport_size_px: [u32; 2],
    pub draw_mesh: bool,
    pub draw_ghost: bool,
    pub toolpath_range: Range<u32>,
}

struct OffscreenTarget {
    width: u32,
    height: u32,
    _color_tex: wgpu::Texture,
    color_view: wgpu::TextureView,
    _depth_tex: wgpu::Texture,
    depth_view: wgpu::TextureView,
    blit_bind_group: wgpu::BindGroup,
}

struct UploadedBuffers {
    revision: u64,
    bed_buf: Option<(wgpu::Buffer, u32)>,
    mesh_buf: Option<(wgpu::Buffer, u32)>,
    ghost_buf: Option<(wgpu::Buffer, u32)>,
    toolpath_buf: Option<(wgpu::Buffer, u32)>,
}

/// Persistent GPU pipelines and cached scene vertex buffers stored in `egui_wgpu` resources.
pub struct ViewportGpuRenderer {
    target_format: wgpu::TextureFormat,
    lit_pipeline: wgpu::RenderPipeline,
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

impl ViewportGpuRenderer {
    /// Compile WGSL shaders and construct the lit, line, and blit pipelines.
    pub fn new(device: &wgpu::Device, target_format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("layercraft_viewport_shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
        });

        let cam_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("layercraft_cam_uniform"),
            contents: bytemuck::cast_slice(&Mat4::IDENTITY.to_cols_array()),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });

        let cam_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("layercraft_cam_layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });

        let cam_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("layercraft_cam_bg"),
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

        let lit_attrs = wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x3, 2 => Float32x4];
        let lit_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("layercraft_lit_pipeline"),
            layout: Some(&scene_layout),
            vertex: wgpu::VertexState {
                module: &shader,
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
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::Less),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
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
        });

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
            multisample: wgpu::MultisampleState::default(),
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
            lit_pipeline,
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

        let color_tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("layercraft_offscreen_color"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: self.target_format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let color_view = color_tex.create_view(&wgpu::TextureViewDescriptor::default());

        let depth_tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("layercraft_offscreen_depth"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
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
                    resource: wgpu::BindingResource::TextureView(&color_view),
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
            _color_tex: color_tex,
            color_view,
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
        let bed_buf = upload_slice(device, "layercraft_bed_lines", &scene.bed_lines);
        let mesh_buf = upload_slice(device, "layercraft_mesh_verts", &scene.mesh_verts);
        let ghost_buf = upload_slice(device, "layercraft_ghost_verts", &scene.ghost_verts);
        let toolpath_buf = upload_slice(device, "layercraft_toolpath_verts", &scene.toolpath_verts);
        self.uploaded = Some(UploadedBuffers {
            revision: scene.revision,
            bed_buf,
            mesh_buf,
            ghost_buf,
            toolpath_buf,
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
        queue.write_buffer(
            &renderer.cam_buf,
            0,
            bytemuck::cast_slice(&self.view_proj.to_cols_array()),
        );

        let Some(offscreen) = &renderer.offscreen else {
            return Vec::new();
        };
        let Some(uploaded) = &renderer.uploaded else {
            return Vec::new();
        };

        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("layercraft_offscreen_3d"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &offscreen.color_view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: 0.085,
                        g: 0.095,
                        b: 0.118,
                        a: 1.0,
                    }),
                    store: wgpu::StoreOp::Store,
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

        if let Some((buf, count)) = &uploaded.bed_buf {
            pass.set_pipeline(&renderer.line_pipeline);
            pass.set_bind_group(0, &renderer.cam_bind_group, &[]);
            pass.set_vertex_buffer(0, buf.slice(..));
            pass.draw(0..*count, 0..1);
        }

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

        if self.draw_ghost
            && let Some((buf, count)) = &uploaded.ghost_buf
        {
            pass.set_pipeline(&renderer.lit_pipeline);
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
