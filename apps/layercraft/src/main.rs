//! LayerCraft native desktop application (`eframe` + `wgpu`).

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use layercraft_mesh::SampleShape;
use layercraft_ui_egui::LayerCraftApp;
use layercraft_viewport_gpu::install_renderer;

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

fn main() -> ExitCode {
    let args = Args::parse();
    let sample = args.sample.as_deref().map(parse_sample);
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

    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Wgpu,
        viewport: egui::ViewportBuilder::default()
            .with_title("LayerCraft")
            .with_inner_size([1280.0, 820.0]),
        ..Default::default()
    };

    let res = eframe::run_native(
        "LayerCraft",
        options,
        Box::new(move |cc| {
            if let Some(rs) = cc.wgpu_render_state.as_ref() {
                install_renderer(rs);
            }
            let mut app = if initial_bytes.is_some() {
                LayerCraftApp::new(None)
            } else {
                LayerCraftApp::new(Some(sample.unwrap_or(SampleShape::Cube20)))
            };
            if let Some((name, bytes)) = initial_bytes {
                app.load_mesh_bytes(&name, bytes);
            }
            Ok(Box::new(app))
        }),
    );

    if let Err(e) = res {
        eprintln!("desktop app failed: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
