//! Headless CLI for LayerCraft slicing, script execution, G-code evaluation, and oracle checks.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use layercraft_config::PrintConfig;
use layercraft_engine::{Session, command_registry, execute};
use layercraft_gcode::{compare_layer_iou, emit_gcode, evaluate_gcode};
use layercraft_io::read_mesh_file;
use layercraft_slice::slice_single_mesh;
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
    },
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

fn run(cli: Cli) -> Result<(), String> {
    match cli.cmd {
        Cmd::Slice {
            input,
            output,
            config,
            stats,
        } => {
            let cfg: PrintConfig = match config {
                Some(path) => {
                    let txt = std::fs::read_to_string(&path)
                        .map_err(|e| format!("{}: {e}", path.display()))?;
                    serde_json::from_str(&txt).map_err(|e| format!("{}: {e}", path.display()))?
                }
                None => PrintConfig::default(),
            };
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
