//! Headless CLI for LayerCraft slicing, rendering, MCP server, G-code evaluation, and oracle checks.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use layercraft_config::{PrintConfig, ProfileKind, ProfileLibrary, ProfileSummary};
use layercraft_engine::{Session, command_registry, execute};
use layercraft_gcode::{compare_layer_iou, emit_gcode, evaluate_gcode};
use layercraft_io::read_mesh_file;
use layercraft_mcp::McpServer;
use layercraft_mesh::prepare_for_plate;
use layercraft_slice::slice_single_mesh;
use layercraft_viewport::{CameraPreset, ColorMode, RenderMode, RenderRequest, render_png};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const EMBEDDED_LICENSES: &str = include_str!("../../../THIRD-PARTY-LICENSES.txt");

#[derive(Parser)]
#[command(
    name = "layercraft-cli",
    version,
    about = "LayerCraft clean-room pure-Rust 3D slicer"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Clone, Copy, ValueEnum)]
enum CliRenderMode {
    Plate,
    Preview,
    LayerTopdown,
}

#[derive(Clone, Copy, ValueEnum)]
enum CliCamera {
    Isometric,
    Top,
    Front,
    Right,
}

#[derive(Subcommand)]
enum Cmd {
    /// Slice an STL or 3MF mesh to G-code.
    Slice {
        input: PathBuf,
        #[arg(short, long)]
        output: PathBuf,
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long)]
        stats: Option<PathBuf>,
        #[arg(long)]
        profile_dir: Vec<PathBuf>,
        #[arg(long)]
        profile_file: Vec<PathBuf>,
        #[arg(long)]
        machine: Option<String>,
        #[arg(long)]
        filament: Option<String>,
        #[arg(long)]
        process: Option<String>,
    },
    /// List available machine, filament, and process JSON profiles.
    Profiles {
        #[arg(long)]
        profile_dir: Vec<PathBuf>,
        #[arg(long)]
        profile_file: Vec<PathBuf>,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        all: bool,
    },
    /// Resolve a named profile with its full inheritance chain to JSON.
    ResolveProfile {
        #[arg(long)]
        kind: String,
        #[arg(long)]
        name: String,
        #[arg(long)]
        profile_dir: Vec<PathBuf>,
        #[arg(long)]
        profile_file: Vec<PathBuf>,
    },
    /// Render a build plate, 3D toolpath preview, or single top-down layer PNG.
    Render {
        input: PathBuf,
        #[arg(short, long)]
        output: PathBuf,
        #[arg(long, value_enum, default_value_t = CliRenderMode::Preview)]
        mode: CliRenderMode,
        #[arg(long, value_enum, default_value_t = CliCamera::Isometric)]
        camera: CliCamera,
        #[arg(long)]
        layer: Option<usize>,
        #[arg(long)]
        single_layer: bool,
        #[arg(long, default_value_t = 800)]
        width: u32,
        #[arg(long, default_value_t = 600)]
        height: u32,
    },
    /// Run the LayerCraft MCP JSON-RPC 2.0 server over stdio.
    Mcp,
    /// Print the JSON command registry with input/output schemas.
    Commands,
    /// Execute a JSON command script `[{"command": "...", "params": ...}, ...]`.
    Run { script: PathBuf },
    /// Evaluate physical invariants on a G-code file.
    Eval {
        gcode: PathBuf,
        #[arg(long, default_value_t = 1.75)]
        filament_diameter: f64,
        #[arg(long, default_value_t = 250.0)]
        bed_width: f64,
        #[arg(long, default_value_t = 250.0)]
        bed_depth: f64,
    },
    /// Compare physical metrics and per-layer 2D raster IoU between two G-code files.
    Compare {
        a: PathBuf,
        b: PathBuf,
        #[arg(long, default_value_t = 1.75)]
        filament_diameter: f64,
        #[arg(long, default_value_t = 250.0)]
        bed_width: f64,
        #[arg(long, default_value_t = 250.0)]
        bed_depth: f64,
        #[arg(long, default_value_t = 0.25)]
        pixel_mm: f64,
        #[arg(long, default_value_t = 0.21)]
        bead_radius_mm: f64,
    },
    /// Print bundled third-party permissive licenses.
    Licences,
}

#[derive(Deserialize)]
struct ScriptStep {
    command: String,
    #[serde(default)]
    params: Value,
}

#[derive(Serialize)]
struct CompareReport {
    a: layercraft_gcode::GcodeReport,
    b: layercraft_gcode::GcodeReport,
    raster: layercraft_gcode::RasterComparison,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    if let Err(e) = run(cli) {
        eprintln!("error: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn load_cli_profile_library(
    profile_dirs: &[PathBuf],
    profile_files: &[PathBuf],
) -> Result<(ProfileLibrary, Vec<ProfileSummary>), String> {
    let mut lib = ProfileLibrary::with_builtin_presets();
    for dir in profile_dirs {
        lib.load_dir(dir).map_err(|e| e.to_string())?;
    }
    let mut file_summaries = Vec::new();
    for file in profile_files {
        let mut loaded = lib.load_file(file).map_err(|e| e.to_string())?;
        file_summaries.append(&mut loaded);
    }
    Ok((lib, file_summaries))
}

fn auto_select_single(summaries: &[ProfileSummary], kind: ProfileKind) -> Option<String> {
    let mut instantiable: Vec<&str> = summaries
        .iter()
        .filter(|s| s.kind == kind && s.instantiation)
        .map(|s| s.name.as_str())
        .collect();
    if instantiable.len() == 1 {
        return instantiable.pop().map(str::to_owned);
    }
    let mut all_of_kind: Vec<&str> = summaries
        .iter()
        .filter(|s| s.kind == kind)
        .map(|s| s.name.as_str())
        .collect();
    if all_of_kind.len() == 1 {
        return all_of_kind.pop().map(str::to_owned);
    }
    None
}

fn run(cli: Cli) -> Result<(), String> {
    match cli.cmd {
        Cmd::Slice {
            input,
            output,
            config,
            stats,
            profile_dir,
            profile_file,
            machine,
            filament,
            process,
        } => {
            let mut cfg: PrintConfig = match config {
                Some(path) => {
                    let txt = std::fs::read_to_string(&path)
                        .map_err(|e| format!("{}: {e}", path.display()))?;
                    serde_json::from_str(&txt).map_err(|e| format!("{}: {e}", path.display()))?
                }
                None => PrintConfig::default(),
            };
            if !profile_dir.is_empty()
                || !profile_file.is_empty()
                || machine.is_some()
                || filament.is_some()
                || process.is_some()
            {
                let (lib, file_summaries) = load_cli_profile_library(&profile_dir, &profile_file)?;
                let m_sel =
                    machine.or_else(|| auto_select_single(&file_summaries, ProfileKind::Machine));
                let f_sel =
                    filament.or_else(|| auto_select_single(&file_summaries, ProfileKind::Filament));
                let p_sel =
                    process.or_else(|| auto_select_single(&file_summaries, ProfileKind::Process));
                let res = lib
                    .build_print_config(&cfg, m_sel.as_deref(), f_sel.as_deref(), p_sel.as_deref())
                    .map_err(|e| e.to_string())?;
                for w in &res.warnings {
                    eprintln!(
                        "profile warning [{} :: {}]: {}",
                        w.profile, w.key, w.message
                    );
                }
                cfg = res.config;
            }
            let mesh = read_mesh_file(&input).map_err(|e| format!("{}: {e}", input.display()))?;
            let sliced = slice_single_mesh(&mesh, &cfg).map_err(|e| e.to_string())?;
            let gcode_out = emit_gcode(&sliced, &cfg).map_err(|e| e.to_string())?;
            std::fs::write(&output, &gcode_out.gcode)
                .map_err(|e| format!("{}: {e}", output.display()))?;
            if let Some(stats_path) = stats {
                let json =
                    serde_json::to_string_pretty(&gcode_out.stats).map_err(|e| e.to_string())?;
                std::fs::write(&stats_path, json)
                    .map_err(|e| format!("{}: {e}", stats_path.display()))?;
            }
            println!(
                "sliced {} -> {} ({} layers, {:.2} mm3 filament)",
                input.display(),
                output.display(),
                gcode_out.stats.layer_count,
                gcode_out.stats.filament_volume_mm3
            );
            Ok(())
        }
        Cmd::Profiles {
            profile_dir,
            profile_file,
            kind,
            all,
        } => {
            let (lib, _) = load_cli_profile_library(&profile_dir, &profile_file)?;
            let kind_filter = match kind {
                Some(k) => Some(ProfileKind::parse_str(&k).map_err(|e| e.to_string())?),
                None => None,
            };
            let list = lib.list(kind_filter, all);
            let json = serde_json::to_string_pretty(&list).map_err(|e| e.to_string())?;
            println!("{json}");
            Ok(())
        }
        Cmd::ResolveProfile {
            kind,
            name,
            profile_dir,
            profile_file,
        } => {
            let (lib, _) = load_cli_profile_library(&profile_dir, &profile_file)?;
            let parsed_kind = ProfileKind::parse_str(&kind).map_err(|e| e.to_string())?;
            let resolved = lib.resolve(parsed_kind, &name).map_err(|e| e.to_string())?;
            let json = serde_json::to_string_pretty(&resolved).map_err(|e| e.to_string())?;
            println!("{json}");
            Ok(())
        }
        Cmd::Render {
            input,
            output,
            mode,
            camera,
            layer,
            single_layer,
            width,
            height,
        } => {
            let cfg = PrintConfig::default();
            let raw_mesh =
                read_mesh_file(&input).map_err(|e| format!("{}: {e}", input.display()))?;
            let prepared = prepare_for_plate(
                &raw_mesh,
                cfg.machine.bed_width_mm,
                cfg.machine.bed_depth_mm,
            )
            .map_err(|e| e.to_string())?;
            let render_mode = match mode {
                CliRenderMode::Plate => RenderMode::Plate,
                CliRenderMode::Preview => RenderMode::Preview,
                CliRenderMode::LayerTopdown => RenderMode::LayerTopDown,
            };
            let sliced = if render_mode == RenderMode::Plate {
                None
            } else {
                Some(slice_single_mesh(&prepared, &cfg).map_err(|e| e.to_string())?)
            };
            let camera_preset = match camera {
                CliCamera::Isometric => CameraPreset::Isometric,
                CliCamera::Top => CameraPreset::Top,
                CliCamera::Front => CameraPreset::Front,
                CliCamera::Right => CameraPreset::Right,
            };
            let png = render_png(
                Some(&prepared),
                sliced.as_ref(),
                &cfg,
                &RenderRequest {
                    width,
                    height,
                    mode: render_mode,
                    camera_preset,
                    max_layer: layer,
                    single_layer_only: single_layer,
                    color_mode: ColorMode::Feature,
                },
            )
            .map_err(|e| e.to_string())?;
            std::fs::write(&output, &png).map_err(|e| format!("{}: {e}", output.display()))?;
            println!(
                "rendered {} -> {} ({} bytes)",
                input.display(),
                output.display(),
                png.len()
            );
            Ok(())
        }
        Cmd::Mcp => McpServer::default().serve_stdio(),
        Cmd::Commands => {
            let reg = command_registry();
            let json = serde_json::to_string_pretty(&reg).map_err(|e| e.to_string())?;
            println!("{json}");
            Ok(())
        }
        Cmd::Run { script } => {
            let txt = std::fs::read_to_string(&script)
                .map_err(|e| format!("{}: {e}", script.display()))?;
            let steps: Vec<ScriptStep> = serde_json::from_str(&txt).map_err(|e| e.to_string())?;
            let mut session = Session::default();
            let mut results = Vec::with_capacity(steps.len());
            for step in steps {
                let res = execute(&mut session, &step.command, step.params)
                    .map_err(|e| format!("command `{}` failed: {e}", step.command))?;
                results.push(res);
            }
            let out = serde_json::to_string_pretty(&results).map_err(|e| e.to_string())?;
            println!("{out}");
            Ok(())
        }
        Cmd::Eval {
            gcode,
            filament_diameter,
            bed_width,
            bed_depth,
        } => {
            let txt =
                std::fs::read_to_string(&gcode).map_err(|e| format!("{}: {e}", gcode.display()))?;
            let (report, _) = evaluate_gcode(&txt, filament_diameter, bed_width, bed_depth)
                .map_err(|e| e.to_string())?;
            let json = serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?;
            println!("{json}");
            Ok(())
        }
        Cmd::Compare {
            a,
            b,
            filament_diameter,
            bed_width,
            bed_depth,
            pixel_mm,
            bead_radius_mm,
        } => {
            let txt_a = std::fs::read_to_string(&a).map_err(|e| format!("{}: {e}", a.display()))?;
            let txt_b = std::fs::read_to_string(&b).map_err(|e| format!("{}: {e}", b.display()))?;
            let (rep_a, segs_a) = evaluate_gcode(&txt_a, filament_diameter, bed_width, bed_depth)
                .map_err(|e| e.to_string())?;
            let (rep_b, segs_b) = evaluate_gcode(&txt_b, filament_diameter, bed_width, bed_depth)
                .map_err(|e| e.to_string())?;
            let raster = compare_layer_iou(&segs_a, &segs_b, pixel_mm, bead_radius_mm);
            let combined = CompareReport {
                a: rep_a,
                b: rep_b,
                raster,
            };
            let json = serde_json::to_string_pretty(&combined).map_err(|e| e.to_string())?;
            println!("{json}");
            Ok(())
        }
        Cmd::Licences => {
            print!("{EMBEDDED_LICENSES}");
            Ok(())
        }
    }
}
