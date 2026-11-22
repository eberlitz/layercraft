//! LayerCraft native desktop application (`eframe` + `wgpu`).

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use clap::Parser;
use layercraft_mesh::SampleShape;
use layercraft_ui_egui::{LayerCraftApp, ThemeMode};
use layercraft_viewport_gpu::{ViewportGpuRenderer, install_renderer};

#[derive(Parser)]
#[command(
    name = "layercraft",
    version,
    about = "LayerCraft pure-Rust 3D slicer desktop app"
)]
struct Args {
    /// Optional STL or 3MF file to open at startup.
    file: Option<PathBuf>,
    /// Optional built-in sample primitive (`cube20`, `cylinder`, `hollow_tube`, `stepped_block`, `thin_rib`, `bridge_arch`, `dome`).
    #[arg(long)]
    sample: Option<String>,
    /// Optional initial color theme (`dark`, `light`, `system`).
    #[arg(long)]
    theme: Option<String>,
    /// Automatically slice on startup and switch to Preview view.
    #[arg(long)]
    preview: bool,
    /// Optional PNG output path: render headlessly offscreen via wgpu and exit.
    #[arg(long)]
    screenshot: Option<PathBuf>,
}

fn parse_sample(name: &str) -> SampleShape {
    match name.to_ascii_lowercase().as_str() {
        "cylinder" => SampleShape::Cylinder,
        "hollow_tube" | "tube" => SampleShape::HollowTube,
        "stepped_block" | "stepped" => SampleShape::SteppedBlock,
        "thin_rib" | "rib" => SampleShape::ThinRib,
        "bridge_arch" | "bridge" => SampleShape::BridgeArch,
        "dome" | "hemisphere" => SampleShape::Dome,
        _ => SampleShape::Cube20,
    }
}

fn parse_theme(name: &str) -> ThemeMode {
    match name.to_ascii_lowercase().as_str() {
        "light" => ThemeMode::Light,
        "system" => ThemeMode::System,
        _ => ThemeMode::Dark,
    }
}

fn build_app(
    initial_bytes: Option<(String, Vec<u8>)>,
    sample: Option<SampleShape>,
    theme: Option<ThemeMode>,
    preview: bool,
) -> LayerCraftApp {
    let mut app = if initial_bytes.is_some() {
        LayerCraftApp::new(None)
    } else {
        LayerCraftApp::new(Some(sample.unwrap_or(SampleShape::Cube20)))
    };
    if let Some((name, bytes)) = initial_bytes {
        app.load_mesh_bytes(&name, bytes);
    }
    if let Some(mode) = theme {
        app.set_theme_mode(mode);
    }
    if preview {
        app.run_slice();
    }
    app
}

fn render_headless_screenshot(
    app: &mut LayerCraftApp,
    path: &Path,
    width: u32,
    height: u32,
) -> Result<(), String> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        compatible_surface: None,
        force_fallback_adapter: false,
        ..Default::default()
    }))
    .or_else(|_| {
        pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            compatible_surface: None,
            force_fallback_adapter: true,
            ..Default::default()
        }))
    })
    .map_err(|e| format!("failed requesting wgpu adapter: {e}"))?;

    let (device, queue) =
        pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
            .map_err(|e| format!("failed creating wgpu device: {e}"))?;

    let format = wgpu::TextureFormat::Rgba8Unorm;
    let mut egui_renderer =
        egui_wgpu::Renderer::new(&device, format, egui_wgpu::RendererOptions::default());
    egui_renderer
        .callback_resources
        .insert(ViewportGpuRenderer::new(&device, format));

    let size = wgpu::Extent3d {
        width,
        height,
        depth_or_array_layers: 1,
    };
    let color_tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("layercraft_headless_color"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let color_view = color_tex.create_view(&wgpu::TextureViewDescriptor::default());

    let unpadded_row = width.saturating_mul(4);
    let padded_row = wgpu::util::align_to(unpadded_row, wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
    let staging = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("layercraft_headless_staging"),
        size: u64::from(padded_row) * u64::from(height),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    let ctx = egui::Context::default();
    ctx.set_pixels_per_point(1.0);

    for pass in 0..4u32 {
        let raw_input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(width as f32, height as f32),
            )),
            time: Some(f64::from(pass) * 0.5),
            ..Default::default()
        };
        let mut full_output = ctx.run_ui(raw_input, |ui| app.render_ui(ui));

        for (id, deltas) in full_output.textures_delta.set.drain() {
            for delta in deltas {
                egui_renderer.update_texture(&device, &queue, id, &delta);
            }
        }

        if pass == 3 {
            let paint_jobs = ctx.tessellate(full_output.shapes, full_output.pixels_per_point);
            let screen_desc = egui_wgpu::ScreenDescriptor {
                size_in_pixels: [width, height],
                pixels_per_point: full_output.pixels_per_point,
            };
            let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("layercraft_headless_encoder"),
            });
            let user_cmd_bufs = egui_renderer.update_buffers(
                &device,
                &queue,
                &mut encoder,
                &paint_jobs,
                &screen_desc,
            );
            {
                let rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("layercraft_headless_pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &color_view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                egui_renderer.render(&mut rpass.forget_lifetime(), &paint_jobs, &screen_desc);
            }
            encoder.copy_texture_to_buffer(
                color_tex.as_image_copy(),
                wgpu::TexelCopyBufferInfo {
                    buffer: &staging,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(padded_row),
                        rows_per_image: None,
                    },
                },
                size,
            );
            queue.submit(
                user_cmd_bufs
                    .into_iter()
                    .chain(std::iter::once(encoder.finish())),
            );
        }

        for id in full_output.textures_delta.free.drain() {
            egui_renderer.free_texture(&id);
        }
    }

    let map_status: Arc<Mutex<Option<Result<(), wgpu::BufferAsyncError>>>> =
        Arc::new(Mutex::new(None));
    let map_status_cb = Arc::clone(&map_status);
    let slice = staging.slice(..);
    slice.map_async(wgpu::MapMode::Read, move |res| {
        if let Ok(mut guard) = map_status_cb.lock() {
            *guard = Some(res);
        }
    });
    device
        .poll(wgpu::PollType::wait_indefinitely())
        .map_err(|e| format!("wgpu device poll failed: {e:?}"))?;

    let status = map_status
        .lock()
        .ok()
        .and_then(|mut g| g.take())
        .ok_or_else(|| "wgpu map_async did not complete".to_owned())?;
    status.map_err(|e| format!("wgpu staging map failed: {e}"))?;

    let mut rgb = Vec::with_capacity((width as usize) * (height as usize) * 3);
    {
        let mapped = slice
            .get_mapped_range()
            .map_err(|e| format!("wgpu mapped range failed: {e:?}"))?;
        for row in mapped.chunks(padded_row as usize) {
            if let Some(valid) = row.get(..unpadded_row as usize) {
                for &[r, g, b, _] in valid.as_chunks::<4>().0 {
                    rgb.push(r);
                    rgb.push(g);
                    rgb.push(b);
                }
            }
        }
    }
    staging.unmap();

    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    let file = std::fs::File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut encoder = png::Encoder::new(file, width, height);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
    writer.write_image_data(&rgb).map_err(|e| e.to_string())
}

fn main() -> ExitCode {
    let args = Args::parse();
    let sample = args.sample.as_deref().map(parse_sample);
    let theme = args.theme.as_deref().map(parse_theme);
    let preview = args.preview;

    let initial_bytes = match args.file {
        Some(path) => match std::fs::read(&path) {
            Ok(bytes) => Some((path.to_string_lossy().into_owned(), bytes)),
            Err(e) => {
                eprintln!("failed to read {}: {e}", path.display());
                return ExitCode::FAILURE;
            }
        },
        None => None,
    };

    if let Some(screenshot_path) = args.screenshot {
        let mut app = build_app(initial_bytes, sample, theme, preview);
        if let Err(e) = render_headless_screenshot(&mut app, &screenshot_path, 1360, 840) {
            eprintln!("headless screenshot failed: {e}");
            return ExitCode::FAILURE;
        }
        return ExitCode::SUCCESS;
    }

    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Wgpu,
        viewport: egui::ViewportBuilder::default()
            .with_title("LayerCraft")
            .with_inner_size([1360.0, 840.0]),
        ..Default::default()
    };

    let res = eframe::run_native(
        "LayerCraft",
        options,
        Box::new(move |cc| {
            if let Some(rs) = cc.wgpu_render_state.as_ref() {
                install_renderer(rs);
            }
            let app = build_app(initial_bytes, sample, theme, preview);
            Ok(Box::new(app))
        }),
    );

    if let Err(e) = res {
        eprintln!("desktop app failed: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
