//! Headless CLI for LayerCraft slicing, rendering, MCP server, G-code evaluation, and oracle checks.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand, ValueEnum};
use layercraft_config::{PrintConfig, ProfileKind, ProfileLibrary, ProfileSummary};
use layercraft_engine::{
    PlateRepairResult, PrinterAuth, PrinterProfile, PrinterProtocol, Session, UploadOptions,
    command_registry, decode_bgcode, encode_bgcode, execute, is_bgcode, probe_printer,
    slice_with_config, upload_to_printer,
};
use layercraft_gcode::{BgcodeOptions, compare_layer_iou, emit_gcode, evaluate_gcode};
use layercraft_io::{
    ThreeMfBuildItem, ThreeMfExportDocument, ThreeMfResourceMesh, read_mesh_file,
    write_3mf_document_bytes, write_stl_binary,
};
use layercraft_mcp::{McpConnectMode, McpServer};
use layercraft_mesh::{MeshRepairConfig, prepare_for_plate, repair_mesh};
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

#[derive(Clone, Copy, ValueEnum)]
enum CliPrinterProtocol {
    PrusaLink,
    PrusaConnectLocal,
    Octoprint,
    Moonraker,
}

impl From<CliPrinterProtocol> for PrinterProtocol {
    fn from(value: CliPrinterProtocol) -> Self {
        match value {
            CliPrinterProtocol::PrusaLink => Self::PrusaLink,
            CliPrinterProtocol::PrusaConnectLocal => Self::PrusaConnectLocal,
            CliPrinterProtocol::Octoprint => Self::OctoPrint,
            CliPrinterProtocol::Moonraker => Self::Moonraker,
        }
    }
}

#[derive(Args)]
struct PrinterConnArgs {
    /// Saved printer ID or name from `~/.config/layercraft/printers.json`.
    #[arg(long)]
    printer: Option<String>,
    /// Printer base URL (e.g. `http://192.168.1.50`).
    #[arg(long)]
    url: Option<String>,
    /// Printer API protocol.
    #[arg(long, value_enum)]
    protocol: Option<CliPrinterProtocol>,
    /// Username (for HTTP Digest authentication).
    #[arg(long)]
    username: Option<String>,
    /// Password (for HTTP Digest authentication).
    #[arg(long)]
    password: Option<String>,
    /// API key header value (`X-Api-Key`).
    #[arg(long)]
    api_key: Option<String>,
    /// Bearer auth token (Prusa Connect Local).
    #[arg(long)]
    token: Option<String>,
    /// Remote storage target (`usb`, `local`, `gcodes`).
    #[arg(long)]
    storage: Option<String>,
    /// Prusa Connect Local device ID (required for auto-print trigger).
    #[arg(long)]
    device_id: Option<String>,
    /// Allow self-signed TLS certificates on HTTPS endpoints.
    #[arg(long)]
    accept_invalid_certs: bool,
}

#[derive(Args)]
struct PrinterUploadArgs {
    input: PathBuf,
    #[command(flatten)]
    conn: PrinterConnArgs,
    #[arg(long)]
    remote_name: Option<String>,
    #[arg(long, value_name = "PLATE")]
    plate: Option<String>,
    #[arg(long)]
    config: Option<PathBuf>,
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
    /// Encode as compressed Prusa Binary G-code (`.bgcode`) before uploading.
    #[arg(long)]
    binary: bool,
    /// Trigger print start immediately after upload (default false).
    #[arg(long)]
    print: bool,
    /// Fail instead of overwriting an existing file on the printer.
    #[arg(long)]
    no_overwrite: bool,
}

#[derive(Subcommand)]
enum PrinterCmd {
    /// Query live firmware info, temperatures, state, and storage list from a printer.
    Test(PrinterConnArgs),
    /// Upload a `.gcode`/`.bgcode` or slice and upload an `.stl`/`.3mf` model directly to a printer.
    Upload(Box<PrinterUploadArgs>),
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
        /// Build plate to select for multi-plate .3mf files (1-based plate number or plate name).
        #[arg(long, value_name = "PLATE")]
        plate: Option<String>,
        /// Emit Prusa Binary G-code (`.bgcode`) instead of plain-text `.gcode`.
        #[arg(long)]
        binary: bool,
        /// Repair non-manifold/cracked/holey meshes automatically before slicing.
        #[arg(long)]
        repair: bool,
    },
    /// Repair a defective STL or 3MF mesh and save to `.stl` or `.3mf`.
    Repair {
        input: PathBuf,
        #[arg(short, long)]
        output: PathBuf,
        /// Print full JSON repair report instead of a summary line.
        #[arg(long)]
        json: bool,
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
        /// Build plate to select for multi-plate .3mf files (1-based plate number or plate name).
        #[arg(long, value_name = "PLATE")]
        plate: Option<String>,
    },
    /// Run the LayerCraft MCP JSON-RPC 2.0 server over stdio.
    Mcp {
        /// Require connecting to an existing running LayerCraft Studio instance.
        #[arg(long, conflicts_with = "standalone")]
        connect: bool,
        /// Always run an isolated headless session (do not connect to Studio).
        #[arg(long, conflicts_with = "connect")]
        standalone: bool,
        /// Custom Studio IPC socket path (overrides LAYERCRAFT_SOCKET; implies --connect).
        #[arg(long)]
        socket: Option<PathBuf>,
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
    /// Connect to and upload prints directly to a LAN/local printer.
    Printer {
        #[command(subcommand)]
        action: PrinterCmd,
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

#[allow(clippy::too_many_arguments)]
fn slice_cli_input(
    input: &std::path::Path,
    config: Option<PathBuf>,
    profile_dir: &[PathBuf],
    profile_file: &[PathBuf],
    machine: Option<String>,
    filament: Option<String>,
    process: Option<String>,
    plate: Option<&str>,
    repair_first: bool,
) -> Result<(layercraft_gcode::GcodeOutput, PrintConfig), String> {
    let is_3mf = input
        .extension()
        .and_then(|s| s.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("3mf"));
    let has_explicit_profiles = !profile_dir.is_empty()
        || !profile_file.is_empty()
        || machine.is_some()
        || filament.is_some()
        || process.is_some();

    let (sliced, effective_cfg) = if is_3mf {
        let mut session = Session::default();
        let in_str = input.to_string_lossy().into_owned();
        execute(
            &mut session,
            "project.load_file",
            serde_json::json!({ "path": in_str }),
        )
        .map_err(|e| format!("{}: {e}", input.display()))?;
        select_cli_plate(&mut session, plate)?;

        if let Some(cfg_path) = &config {
            let txt = std::fs::read_to_string(cfg_path)
                .map_err(|e| format!("{}: {e}", cfg_path.display()))?;
            let override_cfg: PrintConfig =
                serde_json::from_str(&txt).map_err(|e| format!("{}: {e}", cfg_path.display()))?;
            execute(
                &mut session,
                "config.set",
                serde_json::json!({ "config": override_cfg }),
            )
            .map_err(|e| e.to_string())?;
        }
        if has_explicit_profiles {
            let (lib, file_summaries) = load_cli_profile_library(profile_dir, profile_file)?;
            let m_sel =
                machine.or_else(|| auto_select_single(&file_summaries, ProfileKind::Machine));
            let f_sel =
                filament.or_else(|| auto_select_single(&file_summaries, ProfileKind::Filament));
            let p_sel =
                process.or_else(|| auto_select_single(&file_summaries, ProfileKind::Process));
            let res = lib
                .build_print_config(
                    &session.config,
                    m_sel.as_deref(),
                    f_sel.as_deref(),
                    p_sel.as_deref(),
                )
                .map_err(|e| e.to_string())?;
            for w in &res.warnings {
                eprintln!(
                    "profile warning [{} :: {}]: {}",
                    w.profile, w.key, w.message
                );
            }
            execute(
                &mut session,
                "config.set",
                serde_json::json!({ "config": res.config }),
            )
            .map_err(|e| e.to_string())?;
        }
        if repair_first {
            let rep_val = execute(&mut session, "plate.repair_mesh", serde_json::json!({}))
                .map_err(|e| format!("{}: {e}", input.display()))?;
            if let Ok(rep) = serde_json::from_value::<PlateRepairResult>(rep_val)
                && rep.changed
            {
                let welded: usize = rep.objects.iter().map(|o| o.report.welded_vertices).sum();
                let flipped: usize = rep.objects.iter().map(|o| o.report.flipped_triangles).sum();
                let holes: usize = rep.objects.iter().map(|o| o.report.holes_filled).sum();
                let rem: usize = rep
                    .objects
                    .iter()
                    .map(|o| o.report.removed_degenerate + o.report.removed_duplicate)
                    .sum();
                eprintln!(
                    "repaired {} ({} object(s)): welded {welded}, removed {rem}, flipped {flipped}, filled {holes} hole(s)",
                    input.display(),
                    rep.objects.len()
                );
            }
        }
        let sliced = slice_with_config(&session, &session.config).map_err(|e| e.to_string())?;
        (sliced, session.config)
    } else {
        let mut cfg: PrintConfig = match config {
            Some(path) => {
                let txt = std::fs::read_to_string(&path)
                    .map_err(|e| format!("{}: {e}", path.display()))?;
                serde_json::from_str(&txt).map_err(|e| format!("{}: {e}", path.display()))?
            }
            None => PrintConfig::default(),
        };
        if has_explicit_profiles {
            let (lib, file_summaries) = load_cli_profile_library(profile_dir, profile_file)?;
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
        let mut mesh = read_mesh_file(input).map_err(|e| format!("{}: {e}", input.display()))?;
        if repair_first {
            let repaired =
                repair_mesh(&mesh, &MeshRepairConfig::default()).map_err(|e| e.to_string())?;
            if repaired.report.changed {
                eprintln!(
                    "repaired {}: {} -> {} tris (welded {}, removed {}, flipped {}, filled {} hole(s))",
                    input.display(),
                    repaired.report.input_triangles,
                    repaired.report.output_triangles,
                    repaired.report.welded_vertices,
                    repaired.report.removed_degenerate + repaired.report.removed_duplicate,
                    repaired.report.flipped_triangles,
                    repaired.report.holes_filled
                );
                mesh = repaired.mesh;
            }
        }
        let sliced = slice_single_mesh(&mesh, &cfg).map_err(|e| e.to_string())?;
        (sliced, cfg)
    };
    let gcode_out = emit_gcode(&sliced, &effective_cfg).map_err(|e| e.to_string())?;
    Ok((gcode_out, effective_cfg))
}

fn resolve_cli_printer(conn: PrinterConnArgs) -> Result<PrinterProfile, String> {
    let mut sess = Session::default();
    let _ = sess.load_printers_from_disk();

    let mut base = if let Some(id_or_name) = conn.printer.as_deref() {
        sess.resolve_printer_profile(Some(id_or_name), None)
            .map_err(|e| e.to_string())?
    } else if conn.url.is_none() && sess.printers.len() == 1 {
        sess.resolve_printer_profile(None, None)
            .map_err(|e| e.to_string())?
    } else {
        PrinterProfile::default()
    };

    if let Some(url) = conn.url {
        base.base_url = url;
    }
    if base.base_url.trim().is_empty() {
        return Err("specify `--url <http://...>` or `--printer <saved_id>`".to_owned());
    }
    if let Some(proto) = conn.protocol {
        base.protocol = proto.into();
    }
    if let Some(tok) = conn.token {
        base.auth = PrinterAuth::Bearer { token: tok };
    } else if let Some(key) = conn.api_key {
        base.auth = PrinterAuth::ApiKey { api_key: key };
    } else if conn.username.is_some() || conn.password.is_some() {
        let prev_user = match &base.auth {
            PrinterAuth::Digest { username, .. } => username.clone(),
            _ => "maker".to_owned(),
        };
        let prev_pass = match &base.auth {
            PrinterAuth::Digest { password, .. } => password.clone(),
            _ => String::new(),
        };
        base.auth = PrinterAuth::Digest {
            username: conn.username.unwrap_or(prev_user),
            password: conn.password.unwrap_or(prev_pass),
        };
    }
    if let Some(st) = conn.storage {
        base.storage = Some(st);
    }
    if let Some(dev) = conn.device_id {
        base.device_id = Some(dev);
    }
    if conn.accept_invalid_certs {
        base.accept_invalid_certs = true;
    }
    Ok(base)
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
            plate,
            binary,
            repair,
        } => {
            let (gcode_out, effective_cfg) = slice_cli_input(
                &input,
                config,
                &profile_dir,
                &profile_file,
                machine,
                filament,
                process,
                plate.as_deref(),
                repair,
            )?;
            let out_ext = output
                .extension()
                .and_then(|s| s.to_str())
                .map(str::to_ascii_lowercase)
                .unwrap_or_default();
            let write_bgcode = binary
                || out_ext == "bgcode"
                || (effective_cfg.machine.binary_gcode && out_ext != "gcode");
            if write_bgcode {
                let bg = encode_bgcode(&gcode_out, &effective_cfg, &BgcodeOptions::default())
                    .map_err(|e| e.to_string())?;
                std::fs::write(&output, &bg).map_err(|e| format!("{}: {e}", output.display()))?;
                let pct = if gcode_out.gcode.is_empty() {
                    100.0
                } else {
                    (bg.len() as f64 / gcode_out.gcode.len() as f64) * 100.0
                };
                println!(
                    "sliced {} -> {} ({} layers, {:.2} mm3 filament, .bgcode {} bytes / {:.1}% of ASCII)",
                    input.display(),
                    output.display(),
                    gcode_out.stats.layer_count,
                    gcode_out.stats.filament_volume_mm3,
                    bg.len(),
                    pct
                );
            } else {
                std::fs::write(&output, &gcode_out.gcode)
                    .map_err(|e| format!("{}: {e}", output.display()))?;
                println!(
                    "sliced {} -> {} ({} layers, {:.2} mm3 filament)",
                    input.display(),
                    output.display(),
                    gcode_out.stats.layer_count,
                    gcode_out.stats.filament_volume_mm3
                );
            }
            if let Some(stats_path) = stats {
                let json =
                    serde_json::to_string_pretty(&gcode_out.stats).map_err(|e| e.to_string())?;
                std::fs::write(&stats_path, json)
                    .map_err(|e| format!("{}: {e}", stats_path.display()))?;
            }
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
            plate,
        } => {
            let is_3mf = input
                .extension()
                .and_then(|s| s.to_str())
                .is_some_and(|ext| ext.eq_ignore_ascii_case("3mf"));
            let render_mode = match mode {
                CliRenderMode::Plate => RenderMode::Plate,
                CliRenderMode::Preview => RenderMode::Preview,
                CliRenderMode::LayerTopdown => RenderMode::LayerTopDown,
            };
            let (prepared, cfg, sliced) = if is_3mf {
                let mut session = Session::default();
                let in_str = input.to_string_lossy().into_owned();
                execute(
                    &mut session,
                    "project.load_file",
                    serde_json::json!({ "path": in_str }),
                )
                .map_err(|e| format!("{}: {e}", input.display()))?;
                select_cli_plate(&mut session, plate.as_deref())?;
                let sliced = if render_mode == RenderMode::Plate {
                    None
                } else {
                    Some(slice_with_config(&session, &session.config).map_err(|e| e.to_string())?)
                };
                let prepared = session
                    .prepared_mesh
                    .as_deref()
                    .or(session.mesh.as_deref())
                    .cloned()
                    .ok_or_else(|| format!("{}: no mesh on selected plate", input.display()))?;
                (prepared, session.config, sliced)
            } else {
                if plate.is_some() {
                    return Err("--plate is only supported when rendering `.3mf` inputs".to_owned());
                }
                let cfg = PrintConfig::default();
                let raw_mesh =
                    read_mesh_file(&input).map_err(|e| format!("{}: {e}", input.display()))?;
                let prepared = prepare_for_plate(
                    &raw_mesh,
                    cfg.machine.bed_width_mm,
                    cfg.machine.bed_depth_mm,
                )
                .map_err(|e| e.to_string())?;
                let sliced = if render_mode == RenderMode::Plate {
                    None
                } else {
                    Some(slice_single_mesh(&prepared, &cfg).map_err(|e| e.to_string())?)
                };
                (prepared, cfg, sliced)
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
        Cmd::Repair {
            input,
            output,
            json,
        } => {
            let raw = read_mesh_file(&input).map_err(|e| format!("{}: {e}", input.display()))?;
            let repaired =
                repair_mesh(&raw, &MeshRepairConfig::default()).map_err(|e| e.to_string())?;
            let out_ext = output
                .extension()
                .and_then(|s| s.to_str())
                .map(str::to_ascii_lowercase)
                .unwrap_or_default();
            let bytes = if out_ext == "3mf" {
                let stem = input
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("repaired")
                    .to_owned();
                let doc = ThreeMfExportDocument {
                    resources: vec![ThreeMfResourceMesh {
                        id: 1,
                        name: stem,
                        object_type: "model".to_owned(),
                        mesh: repaired.mesh,
                    }],
                    build_items: vec![ThreeMfBuildItem {
                        object_id: 1,
                        transform: None,
                    }],
                    plates: Vec::new(),
                    project_json: None,
                };
                write_3mf_document_bytes(&doc).map_err(|e| e.to_string())?
            } else {
                write_stl_binary(&repaired.mesh)
            };
            std::fs::write(&output, &bytes).map_err(|e| format!("{}: {e}", output.display()))?;
            if json {
                let out_json =
                    serde_json::to_string_pretty(&repaired.report).map_err(|e| e.to_string())?;
                println!("{out_json}");
            } else {
                let r = &repaired.report;
                println!(
                    "repaired {} -> {} ({} -> {} tris, welded {}, removed {}, split {} T-junc, snapped {}, flipped {}, filled {} hole(s), needs_repair={})",
                    input.display(),
                    output.display(),
                    r.input_triangles,
                    r.output_triangles,
                    r.welded_vertices,
                    r.removed_degenerate + r.removed_duplicate,
                    r.t_junctions_split,
                    r.cracks_snapped,
                    r.flipped_triangles,
                    r.holes_filled,
                    r.after.needs_repair
                );
            }
            Ok(())
        }
        Cmd::Mcp {
            connect,
            standalone,
            socket,
        } => {
            let mode = if standalone {
                McpConnectMode::Standalone
            } else if connect || socket.is_some() {
                McpConnectMode::Connect
            } else {
                McpConnectMode::Auto
            };
            McpServer::new(mode, socket).serve_stdio()
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
            let bytes = std::fs::read(&gcode).map_err(|e| format!("{}: {e}", gcode.display()))?;
            let txt = if is_bgcode(&bytes) {
                decode_bgcode(&bytes)
                    .map_err(|e| format!("{}: {e}", gcode.display()))?
                    .gcode
            } else {
                String::from_utf8(bytes).map_err(|e| format!("{}: {e}", gcode.display()))?
            };
            let (report, _) = evaluate_gcode(&txt, filament_diameter, bed_width, bed_depth)
                .map_err(|e| e.to_string())?;
            let json = serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?;
            println!("{json}");
            Ok(())
        }
        Cmd::Printer { action } => match action {
            PrinterCmd::Test(conn) => {
                let prof = resolve_cli_printer(conn)?;
                let report = probe_printer(&prof).map_err(|e| e.to_string())?;
                let json = serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?;
                println!("{json}");
                Ok(())
            }
            PrinterCmd::Upload(args) => {
                let PrinterUploadArgs {
                    input,
                    conn,
                    remote_name,
                    plate,
                    config,
                    profile_dir,
                    profile_file,
                    machine,
                    filament,
                    process,
                    binary,
                    print,
                    no_overwrite,
                } = *args;
                let prof = resolve_cli_printer(conn)?;
                let in_ext = input
                    .extension()
                    .and_then(|s| s.to_str())
                    .map(str::to_ascii_lowercase)
                    .unwrap_or_default();
                let (payload, default_name) =
                    if matches!(in_ext.as_str(), "gcode" | "gco" | "g" | "bgcode") {
                        let bytes = std::fs::read(&input)
                            .map_err(|e| format!("{}: {e}", input.display()))?;
                        let fallback = input
                            .file_name()
                            .and_then(|s| s.to_str())
                            .unwrap_or("print.gcode")
                            .to_owned();
                        (bytes, fallback)
                    } else {
                        let (gcode_out, effective_cfg) = slice_cli_input(
                            &input,
                            config,
                            &profile_dir,
                            &profile_file,
                            machine,
                            filament,
                            process,
                            plate.as_deref(),
                            false,
                        )?;
                        let (rep, _) = evaluate_gcode(
                            &gcode_out.gcode,
                            effective_cfg.filament.diameter_mm,
                            effective_cfg.machine.bed_width_mm,
                            effective_cfg.machine.bed_depth_mm,
                        )
                        .map_err(|e| e.to_string())?;
                        if !rep.safety.is_clean() {
                            return Err(format!(
                                "aborted upload due to G-code safety violations: {:?}",
                                rep.safety
                            ));
                        }
                        let use_bgcode = binary
                            || remote_name
                                .as_deref()
                                .is_some_and(|n| n.to_ascii_lowercase().ends_with(".bgcode"))
                            || ((prof.prefer_bgcode || effective_cfg.machine.binary_gcode)
                                && prof.protocol.supports_bgcode()
                                && !remote_name
                                    .as_deref()
                                    .is_some_and(|n| n.to_ascii_lowercase().ends_with(".gcode")));
                        let stem = input
                            .file_stem()
                            .and_then(|s| s.to_str())
                            .unwrap_or("layercraft_out");
                        let ext = if use_bgcode { "bgcode" } else { "gcode" };
                        let bytes = if use_bgcode {
                            encode_bgcode(&gcode_out, &effective_cfg, &BgcodeOptions::default())
                                .map_err(|e| e.to_string())?
                        } else {
                            gcode_out.gcode.into_bytes()
                        };
                        (bytes, format!("{stem}.{ext}"))
                    };
                let up = upload_to_printer(
                    &prof,
                    &payload,
                    &UploadOptions {
                        remote_path: remote_name.unwrap_or(default_name),
                        storage: prof.storage.clone(),
                        start_print: print,
                        overwrite: !no_overwrite,
                    },
                )
                .map_err(|e| e.to_string())?;
                let json = serde_json::to_string_pretty(&up).map_err(|e| e.to_string())?;
                println!("{json}");
                Ok(())
            }
        },
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

fn select_cli_plate(session: &mut Session, plate_arg: Option<&str>) -> Result<(), String> {
    let Some(spec) = plate_arg.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(());
    };
    if execute(
        session,
        "plates.select",
        serde_json::json!({ "name": spec }),
    )
    .is_ok()
    {
        return Ok(());
    }
    if let Ok(num) = spec.parse::<usize>()
        && num >= 1
    {
        if num - 1 < session.plate_count()
            && execute(
                session,
                "plates.select",
                serde_json::json!({ "index": num - 1 }),
            )
            .is_ok()
        {
            return Ok(());
        }
        if execute(session, "plates.select", serde_json::json!({ "id": num })).is_ok() {
            return Ok(());
        }
    }
    let available: Vec<String> = session
        .list_plates()
        .into_iter()
        .map(|p| format!("{}: {}", p.index + 1, p.name))
        .collect();
    Err(format!(
        "plate `{spec}` not found; available plates: [{}]",
        available.join(", ")
    ))
}
