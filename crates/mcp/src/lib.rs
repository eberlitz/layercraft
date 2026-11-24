//! Model Context Protocol (MCP) JSON-RPC 2.0 server over stdio with Live Studio IPC bridge.

#[cfg(not(target_arch = "wasm32"))]
pub mod bridge;

use std::io::{BufRead, Write};
use std::path::PathBuf;

use base64::Engine as _;
use layercraft_engine::{
    BatchStep, BgcodeCompression, GcodeExportFormat, PrinterProfile, Session,
    apply_config_patch_value, command_registry, describe_command, execute, execute_batch,
    list_command_summaries, slice_with_config,
};
use layercraft_viewport::{
    CameraPreset, ColorMode, OrbitCamera, RenderMode, RenderRequest, compare_png,
    render_png_with_camera,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[cfg(not(target_arch = "wasm32"))]
pub use crate::bridge::McpConnectMode;
#[cfg(not(target_arch = "wasm32"))]
use crate::bridge::{
    BridgeOp, BridgeRequest, BridgeResponse, StudioBridgeClient, StudioDiscoveryInfo,
    default_socket_path,
};

#[cfg(target_arch = "wasm32")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum McpConnectMode {
    #[default]
    Auto,
    Connect,
    Standalone,
}

const EMBEDDED_SKILL_MD: &str = include_str!("../../../skills/layercraft/SKILL.md");

/// Studio workspace view mode synchronized from MCP `set_view` / `screenshot(sync_view: true)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum UiViewMode {
    Prepare,
    Preview,
}

/// Optional viewport sync directive emitted for a live LayerCraft Studio host.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ViewSyncRequest {
    pub view_mode: Option<UiViewMode>,
    pub camera_preset: Option<CameraPreset>,
    pub max_layer: Option<usize>,
    pub single_layer_only: Option<bool>,
    pub color_mode: Option<ColorMode>,
}

/// Connection/host metadata surfaced via `version`, `inspect_plate`, and `layercraft://plate/summary`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModeInfo {
    pub mode: String,
    pub studio_running: bool,
    pub studio_pid: Option<u32>,
    pub socket_path: Option<String>,
}

impl Default for ModeInfo {
    fn default() -> Self {
        Self {
            mode: "headless".to_owned(),
            studio_running: false,
            studio_pid: None,
            socket_path: None,
        }
    }
}

/// Result produced by executing one MCP tool locally or on a Studio session.
#[derive(Debug, Clone)]
pub struct LocalDispatchOutput {
    pub mcp_result: Value,
    pub activity_label: Option<String>,
    pub executed_command: Option<String>,
    pub view_sync: Option<ViewSyncRequest>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionAffinity {
    Undecided,
    Studio,
    Local,
}

/// Stateful MCP server wrapping a [`layercraft_engine::Session`] and optional Live Studio IPC client.
pub struct McpServer {
    pub session: Session,
    pub last_screenshot_png: Option<Vec<u8>>,
    mode: McpConnectMode,
    socket_override: Option<PathBuf>,
    affinity: SessionAffinity,
    #[cfg(not(target_arch = "wasm32"))]
    bridge_client: StudioBridgeClient,
}

impl Default for McpServer {
    fn default() -> Self {
        Self::new(McpConnectMode::Standalone, None)
    }
}

#[derive(Debug, Deserialize)]
struct JsonRpcReq {
    #[serde(default)]
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct EmptyArgs {}

#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(default)]
struct ListCommandsArgs {
    /// Optional command name prefix filter (e.g. `"plate."`, `"config."`, `"slice."`, `"gcode."`).
    prefix: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct DescribeCommandArgs {
    /// Exact command name from `list_commands`.
    name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ExecuteArgs {
    command: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct BatchArgs {
    steps: Vec<BatchStep>,
    /// Roll back session state if any step fails (default `true`).
    #[serde(default = "default_true")]
    atomic: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ConfigPatchArgs {
    /// Partial JSON object merged onto `PrintConfig`.
    patch: Value,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct InspectLayerArgs {
    index: usize,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(default)]
struct AnalyzePrintabilityArgs {
    /// Automatically slice if no cached slice exists yet (default `true`).
    #[serde(default = "default_true")]
    auto_slice: bool,
}

impl Default for AnalyzePrintabilityArgs {
    fn default() -> Self {
        Self { auto_slice: true }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SaveGcodeArgs {
    path: String,
    #[serde(default = "default_true")]
    evaluate: bool,
    #[serde(default)]
    format: GcodeExportFormat,
    #[serde(default)]
    compression: Option<BgcodeCompression>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(default)]
struct PrinterStatusArgs {
    /// Saved printer ID or name (optional if `profile` is given, or omit to list/probe default).
    printer_id: Option<String>,
    /// Optional inline printer connection profile override (not saved to `.3mf`).
    profile: Option<PrinterProfile>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(default)]
struct UploadToPrinterArgs {
    /// Saved printer ID or name (optional when one printer is saved or `profile` is provided).
    printer_id: Option<String>,
    /// Optional inline printer connection profile (URL, protocol, auth).
    profile: Option<PrinterProfile>,
    /// Remote filename or relative path on printer storage (e.g. `"gears.bgcode"`).
    filename: Option<String>,
    /// Storage target override (`"usb"`, `"local"`, `"gcodes"`).
    storage: Option<String>,
    /// Output format (`"auto"`, `"gcode"`, `"bgcode"`).
    format: GcodeExportFormat,
    /// Optional `.bgcode` compression override (`"heatshrink11_4"`, `"heatshrink12_4"`, `"deflate"`, `"none"`).
    compression: Option<BgcodeCompression>,
    /// Automatically start printing after upload completes (defaults `false` for safety).
    start_print: bool,
    /// Overwrite existing file with same remote path (default `true`).
    #[serde(default = "default_true")]
    overwrite: bool,
    /// Slice automatically before upload if not sliced yet (default `true`).
    #[serde(default = "default_true")]
    auto_slice: bool,
    /// Evaluate G-code safety invariants prior to upload (default `true`).
    #[serde(default = "default_true")]
    evaluate: bool,
    /// Abort upload if any safety violation is detected (default `true`).
    #[serde(default = "default_true")]
    require_clean_safety: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ScreenshotCamera {
    #[default]
    Isometric,
    Top,
    Front,
    Right,
    /// Capture from the live Studio orbit camera (falls back to `isometric` in headless mode).
    Current,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ScreenshotArgs {
    #[serde(default)]
    mode: RenderMode,
    #[serde(default)]
    camera: ScreenshotCamera,
    max_layer: Option<usize>,
    #[serde(default)]
    single_layer_only: bool,
    #[serde(default)]
    color_mode: ColorMode,
    width: Option<u32>,
    height: Option<u32>,
    /// Sync the live Studio viewport camera/mode/layer to match this screenshot request.
    #[serde(default)]
    sync_view: bool,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(default)]
struct CompareSlicesArgs {
    patch_a: Option<Value>,
    patch_b: Value,
    pixel_mm: Option<f64>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(default)]
struct CompareScreenshotArgs {
    patch_a: Option<Value>,
    patch_b: Option<Value>,
    mode: RenderMode,
    camera: ScreenshotCamera,
    max_layer: Option<usize>,
    single_layer_only: bool,
    color_mode: ColorMode,
    width: Option<u32>,
    height: Option<u32>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(default)]
struct SetViewArgs {
    mode: Option<UiViewMode>,
    camera: Option<CameraPreset>,
    max_layer: Option<usize>,
    single_layer_only: Option<bool>,
    color_mode: Option<ColorMode>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(default)]
struct SkillReferenceArgs {
    /// Optional section heading from `SKILL.md` (e.g. `"Live Studio Connection"`, `"Printability"`).
    section: Option<String>,
}

#[derive(Debug, Serialize)]
struct ToolDef {
    name: &'static str,
    description: &'static str,
    #[serde(rename = "inputSchema")]
    input_schema: Value,
}

fn default_true() -> bool {
    true
}

fn schema_val<T: JsonSchema>() -> Value {
    serde_json::to_value(schemars::schema_for!(T)).unwrap_or_else(|_| json!({ "type": "object" }))
}

fn text_content(val: Value) -> Result<Value, String> {
    let text = serde_json::to_string_pretty(&val).map_err(|e| e.to_string())?;
    Ok(json!({ "content": [{ "type": "text", "text": text }] }))
}

fn tool_definitions() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "version",
            description: "Report server version and whether MCP is connected to a live LayerCraft Studio instance or running headlessly.",
            input_schema: schema_val::<EmptyArgs>(),
        },
        ToolDef {
            name: "list_commands",
            description: "Token-efficient listing of command names and one-line summaries (optionally filtered by prefix).",
            input_schema: schema_val::<ListCommandsArgs>(),
        },
        ToolDef {
            name: "describe_command",
            description: "Return full JSON input/output schemas for one engine command by exact name.",
            input_schema: schema_val::<DescribeCommandArgs>(),
        },
        ToolDef {
            name: "commands",
            description: "List every LayerCraft engine command with full JSON input and output schemas.",
            input_schema: schema_val::<EmptyArgs>(),
        },
        ToolDef {
            name: "execute",
            description: "Execute a single LayerCraft engine command (`plate.add_mesh_file`, `profile.apply`, `config.patch`, `slice.run`, etc.).",
            input_schema: schema_val::<ExecuteArgs>(),
        },
        ToolDef {
            name: "batch",
            description: "Execute multiple engine commands sequentially with optional atomic rollback on failure (default true).",
            input_schema: schema_val::<BatchArgs>(),
        },
        ToolDef {
            name: "config_patch",
            description: "Apply a partial JSON merge patch onto the active PrintConfig and validate it.",
            input_schema: schema_val::<ConfigPatchArgs>(),
        },
        ToolDef {
            name: "inspect_plate",
            description: "Return active print config, mesh inspection, object/modifier counts, and slicing summary for the current plate.",
            input_schema: schema_val::<EmptyArgs>(),
        },
        ToolDef {
            name: "inspect_layer",
            description: "Inspect a single sliced layer (Z height, region count, per-role toolpath lengths).",
            input_schema: schema_val::<InspectLayerArgs>(),
        },
        ToolDef {
            name: "analyze_printability",
            description: "Run Printability / DFM diagnostics (overhang risk, floating islands, thin walls, bed contact, per-feature breakdown).",
            input_schema: schema_val::<AnalyzePrintabilityArgs>(),
        },
        ToolDef {
            name: "save_gcode",
            description: "Emit G-code or .bgcode if needed, write directly to a file on disk, and optionally run physical safety evaluation.",
            input_schema: schema_val::<SaveGcodeArgs>(),
        },
        ToolDef {
            name: "printer_status",
            description: "Query live firmware info, nozzle/bed temperatures, state, and storage list from a configured or inline printer (or list redacted saved printers).",
            input_schema: schema_val::<PrinterStatusArgs>(),
        },
        ToolDef {
            name: "upload_to_printer",
            description: "Slice if needed, verify physical safety invariants, encode .bgcode or .gcode, and upload directly to a printer over LAN (PrusaLink, Prusa Connect Local, OctoPrint, Moonraker).",
            input_schema: schema_val::<UploadToPrinterArgs>(),
        },
        ToolDef {
            name: "screenshot",
            description: "Render the build plate, sliced 3D toolpaths, or a top-down single layer (`plate`, `preview`, `layer_top_down`) as PNG. Supports `camera: \"current\"` and `sync_view: true` in live Studio.",
            input_schema: schema_val::<ScreenshotArgs>(),
        },
        ToolDef {
            name: "compare_slices",
            description: "Non-destructively slice and compare two config patches quantitatively (volume, time, travel, arcs, per-layer 2D raster IoU).",
            input_schema: schema_val::<CompareSlicesArgs>(),
        },
        ToolDef {
            name: "compare_screenshot",
            description: "Render a 3-panel A | B | Diff PNG comparison between two config variants (`patch_a` vs `patch_b`) or between the last screenshot and current state.",
            input_schema: schema_val::<CompareScreenshotArgs>(),
        },
        ToolDef {
            name: "set_view",
            description: "Update live LayerCraft Studio view state (`mode`, `camera`, `max_layer`, `single_layer_only`, `color_mode`).",
            input_schema: schema_val::<SetViewArgs>(),
        },
        ToolDef {
            name: "skill_reference",
            description: "Read LayerCraft's bundled AI Agent workflow & DFM guide (`SKILL.md`), full or by section.",
            input_schema: schema_val::<SkillReferenceArgs>(),
        },
    ]
}

fn is_local_metadata_tool(name: &str) -> bool {
    matches!(
        name,
        "list_commands" | "describe_command" | "commands" | "skill_reference"
    )
}

fn extract_skill_section(query: &str) -> Result<String, String> {
    let q = query.trim().to_ascii_lowercase();
    if q.is_empty() {
        return Ok(EMBEDDED_SKILL_MD.to_owned());
    }

    let mut headings = Vec::new();
    let mut active: Option<(String, String)> = None;
    let mut matches = Vec::new();

    for line in EMBEDDED_SKILL_MD.lines() {
        if let Some(title) = line.strip_prefix("## ") {
            if let Some((h, body)) = active.take()
                && h.to_ascii_lowercase().contains(&q)
            {
                matches.push(format!("## {h}\n{body}"));
            }
            headings.push(title.trim().to_owned());
            active = Some((title.trim().to_owned(), String::new()));
        } else if let Some((_, ref mut body)) = active {
            body.push_str(line);
            body.push('\n');
        }
    }
    if let Some((h, body)) = active.take()
        && h.to_ascii_lowercase().contains(&q)
    {
        matches.push(format!("## {h}\n{body}"));
    }

    if matches.is_empty() {
        Err(format!(
            "section matching `{query}` not found; available sections: {}",
            headings.join(", ")
        ))
    } else {
        Ok(matches.join("\n"))
    }
}

#[allow(clippy::too_many_arguments)]
fn render_screenshot_bytes(
    session: &Session,
    mode: RenderMode,
    camera: ScreenshotCamera,
    max_layer: Option<usize>,
    single_layer_only: bool,
    color_mode: ColorMode,
    width: u32,
    height: u32,
    live_camera: Option<&OrbitCamera>,
) -> Result<(Vec<u8>, CameraPreset), String> {
    let preset = match camera {
        ScreenshotCamera::Isometric | ScreenshotCamera::Current => CameraPreset::Isometric,
        ScreenshotCamera::Top => CameraPreset::Top,
        ScreenshotCamera::Front => CameraPreset::Front,
        ScreenshotCamera::Right => CameraPreset::Right,
    };
    let custom_cam = if camera == ScreenshotCamera::Current {
        live_camera
    } else {
        None
    };
    let req = RenderRequest {
        width,
        height,
        mode,
        camera_preset: preset,
        max_layer,
        single_layer_only,
        color_mode,
    };
    let bytes = render_png_with_camera(
        session.prepared_mesh.as_deref(),
        session.sliced.as_ref(),
        &session.config,
        &req,
        custom_cam,
    )
    .map_err(|e| e.to_string())?;
    Ok((bytes, preset))
}

/// Dispatch one MCP tool against an arbitrary mutable [`Session`] and screenshot cache.
///
/// Used both by headless [`McpServer`] and by the live desktop Studio bridge in `apps/layercraft`.
pub fn dispatch_tool_on_session(
    session: &mut Session,
    last_screenshot_png: &mut Option<Vec<u8>>,
    name: &str,
    args: Value,
    mode_info: &ModeInfo,
    live_camera: Option<&OrbitCamera>,
) -> Result<LocalDispatchOutput, String> {
    match name {
        "version" => {
            let payload = json!({
                "server": "layercraft-mcp",
                "crate_version": env!("CARGO_PKG_VERSION"),
                "protocol_version": "2024-11-05",
                "mode": mode_info.mode,
                "studio_running": mode_info.studio_running,
                "studio_pid": mode_info.studio_pid,
                "socket_path": mode_info.socket_path,
            });
            Ok(LocalDispatchOutput {
                mcp_result: text_content(payload)?,
                activity_label: None,
                executed_command: None,
                view_sync: None,
            })
        }
        "list_commands" => {
            let parsed: ListCommandsArgs = if args.is_null() {
                ListCommandsArgs::default()
            } else {
                serde_json::from_value(args).map_err(|e| format!("invalid arguments: {e}"))?
            };
            let list = list_command_summaries(parsed.prefix.as_deref());
            Ok(LocalDispatchOutput {
                mcp_result: text_content(serde_json::to_value(list).map_err(|e| e.to_string())?)?,
                activity_label: None,
                executed_command: None,
                view_sync: None,
            })
        }
        "describe_command" => {
            let parsed: DescribeCommandArgs =
                serde_json::from_value(args).map_err(|e| format!("invalid arguments: {e}"))?;
            let desc = describe_command(&parsed.name)
                .ok_or_else(|| format!("unknown engine command `{}`", parsed.name))?;
            Ok(LocalDispatchOutput {
                mcp_result: text_content(serde_json::to_value(desc).map_err(|e| e.to_string())?)?,
                activity_label: None,
                executed_command: None,
                view_sync: None,
            })
        }
        "commands" => Ok(LocalDispatchOutput {
            mcp_result: text_content(
                serde_json::to_value(command_registry()).map_err(|e| e.to_string())?,
            )?,
            activity_label: None,
            executed_command: None,
            view_sync: None,
        }),
        "skill_reference" => {
            let parsed: SkillReferenceArgs = if args.is_null() {
                SkillReferenceArgs::default()
            } else {
                serde_json::from_value(args).map_err(|e| format!("invalid arguments: {e}"))?
            };
            let text = match parsed.section.as_deref() {
                Some(sec) => extract_skill_section(sec)?,
                None => EMBEDDED_SKILL_MD.to_owned(),
            };
            Ok(LocalDispatchOutput {
                mcp_result: json!({ "content": [{ "type": "text", "text": text }] }),
                activity_label: None,
                executed_command: None,
                view_sync: None,
            })
        }
        "execute" => {
            let parsed: ExecuteArgs =
                serde_json::from_value(args).map_err(|e| format!("invalid arguments: {e}"))?;
            let cmd_name = parsed.command.clone();
            let res =
                execute(session, &parsed.command, parsed.params).map_err(|e| e.to_string())?;
            Ok(LocalDispatchOutput {
                mcp_result: text_content(res)?,
                activity_label: Some(cmd_name.clone()),
                executed_command: Some(cmd_name),
                view_sync: None,
            })
        }
        "batch" => {
            let parsed: BatchArgs =
                serde_json::from_value(args).map_err(|e| format!("invalid arguments: {e}"))?;
            let batch_res = execute_batch(session, &parsed.steps, parsed.atomic);
            let last_cmd = parsed.steps.last().map(|s| s.command.clone());
            let mcp_result =
                text_content(serde_json::to_value(&batch_res).map_err(|e| e.to_string())?)?;
            if !batch_res.ok {
                return Err(batch_res
                    .error
                    .unwrap_or_else(|| "batch execution failed".to_owned()));
            }
            Ok(LocalDispatchOutput {
                mcp_result,
                activity_label: Some(format!("batch ({} steps)", batch_res.completed_steps)),
                executed_command: last_cmd.or_else(|| Some("batch".to_owned())),
                view_sync: None,
            })
        }
        "config_patch" => {
            let patch_params = match &args {
                Value::Object(map) if map.contains_key("patch") => {
                    let parsed: ConfigPatchArgs = serde_json::from_value(args)
                        .map_err(|e| format!("invalid arguments: {e}"))?;
                    json!({ "patch": parsed.patch })
                }
                other => json!({ "patch": other }),
            };
            let res = execute(session, "config.patch", patch_params).map_err(|e| e.to_string())?;
            Ok(LocalDispatchOutput {
                mcp_result: text_content(res)?,
                activity_label: Some("config.patch".to_owned()),
                executed_command: Some("config.patch".to_owned()),
                view_sync: None,
            })
        }
        "inspect_plate" => {
            let mesh_info = execute(session, "plate.inspect", Value::Null).ok();
            let layer_count = session.sliced.as_ref().map(|s| s.layers.len());
            let gcode_stats = session.last_gcode.as_ref().map(|g| &g.stats);
            let payload = json!({
                "mode": mode_info.mode,
                "object_count": session.objects.len(),
                "modifier_count": session.modifiers.len(),
                "active_plate": {
                    "id": session.active_plate_id(),
                    "index": session.active_plate_index(),
                    "name": session.active_plate_name(),
                },
                "plates": session.list_plates(),
                "active_profiles": session.active_profiles,
                "history": session.history_status(),
                "mesh": mesh_info,
                "layer_count": layer_count,
                "gcode_stats": gcode_stats,
                "config": session.config,
            });
            Ok(LocalDispatchOutput {
                mcp_result: text_content(payload)?,
                activity_label: None,
                executed_command: None,
                view_sync: None,
            })
        }
        "inspect_layer" => {
            let parsed: InspectLayerArgs =
                serde_json::from_value(args).map_err(|e| format!("invalid arguments: {e}"))?;
            let res = execute(session, "layer.inspect", json!({ "index": parsed.index }))
                .map_err(|e| e.to_string())?;
            Ok(LocalDispatchOutput {
                mcp_result: text_content(res)?,
                activity_label: None,
                executed_command: None,
                view_sync: None,
            })
        }
        "analyze_printability" => {
            let parsed: AnalyzePrintabilityArgs = if args.is_null() {
                AnalyzePrintabilityArgs::default()
            } else {
                serde_json::from_value(args).map_err(|e| format!("invalid arguments: {e}"))?
            };
            let was_unsliced = session.sliced.is_none();
            let res = execute(
                session,
                "plate.analyze_printability",
                json!({ "auto_slice": parsed.auto_slice }),
            )
            .map_err(|e| e.to_string())?;
            let executed_cmd = if was_unsliced && parsed.auto_slice {
                Some("plate.analyze_printability".to_owned())
            } else {
                None
            };
            Ok(LocalDispatchOutput {
                mcp_result: text_content(res)?,
                activity_label: Some("analyze_printability".to_owned()),
                executed_command: executed_cmd,
                view_sync: None,
            })
        }
        "save_gcode" => {
            let parsed: SaveGcodeArgs =
                serde_json::from_value(args).map_err(|e| format!("invalid arguments: {e}"))?;
            let res = execute(
                session,
                "gcode.save_file",
                json!({
                    "path": parsed.path,
                    "evaluate": parsed.evaluate,
                    "format": parsed.format,
                    "compression": parsed.compression,
                }),
            )
            .map_err(|e| e.to_string())?;
            Ok(LocalDispatchOutput {
                mcp_result: text_content(res)?,
                activity_label: Some("save_gcode".to_owned()),
                executed_command: Some("gcode.save_file".to_owned()),
                view_sync: None,
            })
        }
        "printer_status" => {
            let parsed: PrinterStatusArgs = if args.is_null() {
                PrinterStatusArgs::default()
            } else {
                serde_json::from_value(args).map_err(|e| format!("invalid arguments: {e}"))?
            };
            let res = if parsed.printer_id.is_none()
                && parsed.profile.is_none()
                && session.printers.len() != 1
            {
                let saved =
                    execute(session, "printer.list", Value::Null).map_err(|e| e.to_string())?;
                json!({
                    "saved_printers": saved,
                    "note": "Specify `printer_id` or `profile` to query live printer telemetry."
                })
            } else {
                execute(
                    session,
                    "printer.status",
                    json!({
                        "printer_id": parsed.printer_id,
                        "profile": parsed.profile,
                    }),
                )
                .map_err(|e| e.to_string())?
            };
            Ok(LocalDispatchOutput {
                mcp_result: text_content(res)?,
                activity_label: Some("printer_status".to_owned()),
                executed_command: None,
                view_sync: None,
            })
        }
        "upload_to_printer" => {
            let parsed: UploadToPrinterArgs = if args.is_null() {
                UploadToPrinterArgs::default()
            } else {
                serde_json::from_value(args).map_err(|e| format!("invalid arguments: {e}"))?
            };
            let res = execute(
                session,
                "printer.upload",
                json!({
                    "printer_id": parsed.printer_id,
                    "profile": parsed.profile,
                    "filename": parsed.filename,
                    "storage": parsed.storage,
                    "format": parsed.format,
                    "compression": parsed.compression,
                    "start_print": parsed.start_print,
                    "overwrite": parsed.overwrite,
                    "auto_slice": parsed.auto_slice,
                    "evaluate": parsed.evaluate,
                    "require_clean_safety": parsed.require_clean_safety,
                }),
            )
            .map_err(|e| e.to_string())?;
            Ok(LocalDispatchOutput {
                mcp_result: text_content(res)?,
                activity_label: Some("upload_to_printer".to_owned()),
                executed_command: Some("printer.upload".to_owned()),
                view_sync: None,
            })
        }
        "screenshot" => {
            let parsed: ScreenshotArgs = if args.is_null() {
                serde_json::from_value(json!({})).map_err(|e| e.to_string())?
            } else {
                serde_json::from_value(args).map_err(|e| format!("invalid arguments: {e}"))?
            };
            let w = parsed.width.unwrap_or(800);
            let h = parsed.height.unwrap_or(600);
            let (png_bytes, preset) = render_screenshot_bytes(
                session,
                parsed.mode,
                parsed.camera,
                parsed.max_layer,
                parsed.single_layer_only,
                parsed.color_mode,
                w,
                h,
                live_camera,
            )?;
            *last_screenshot_png = Some(png_bytes.clone());
            let b64 = base64::engine::general_purpose::STANDARD.encode(&png_bytes);
            let layer_desc = match (session.sliced.as_ref(), parsed.max_layer) {
                (Some(s), Some(idx)) => format!(
                    "layer {}/{}",
                    idx.min(s.layers.len().saturating_sub(1)) + 1,
                    s.layers.len()
                ),
                (Some(s), None) => format!("{} layers", s.layers.len()),
                (None, _) => "unsliced".to_owned(),
            };
            let view_sync = if parsed.sync_view {
                Some(ViewSyncRequest {
                    view_mode: Some(match parsed.mode {
                        RenderMode::Plate => UiViewMode::Prepare,
                        RenderMode::Preview | RenderMode::LayerTopDown => UiViewMode::Preview,
                    }),
                    camera_preset: if parsed.camera == ScreenshotCamera::Current {
                        None
                    } else {
                        Some(preset)
                    },
                    max_layer: parsed.max_layer,
                    single_layer_only: Some(parsed.single_layer_only),
                    color_mode: Some(parsed.color_mode),
                })
            } else {
                None
            };
            Ok(LocalDispatchOutput {
                mcp_result: json!({
                    "content": [
                        {
                            "type": "image",
                            "data": b64,
                            "mimeType": "image/png"
                        },
                        {
                            "type": "text",
                            "text": format!(
                                "Rendered {}x{} {:?} ({layer_desc})",
                                w.clamp(64, 1920),
                                h.clamp(64, 1920),
                                parsed.mode
                            )
                        }
                    ]
                }),
                activity_label: Some("screenshot".to_owned()),
                executed_command: None,
                view_sync,
            })
        }
        "compare_slices" => {
            let parsed: CompareSlicesArgs =
                serde_json::from_value(args).map_err(|e| format!("invalid arguments: {e}"))?;
            let res = execute(
                session,
                "slice.compare",
                json!({
                    "patch_a": parsed.patch_a,
                    "patch_b": parsed.patch_b,
                    "pixel_mm": parsed.pixel_mm,
                }),
            )
            .map_err(|e| e.to_string())?;
            Ok(LocalDispatchOutput {
                mcp_result: text_content(res)?,
                activity_label: Some("compare_slices".to_owned()),
                executed_command: None,
                view_sync: None,
            })
        }
        "compare_screenshot" => {
            let parsed: CompareScreenshotArgs = if args.is_null() {
                CompareScreenshotArgs::default()
            } else {
                serde_json::from_value(args).map_err(|e| format!("invalid arguments: {e}"))?
            };
            let w = parsed.width.unwrap_or(640);
            let h = parsed.height.unwrap_or(480);

            let (png_a, png_b) = if let Some(patch_b) = &parsed.patch_b {
                let cfg_a = match &parsed.patch_a {
                    Some(pa) => {
                        apply_config_patch_value(&session.config, pa).map_err(|e| e.to_string())?
                    }
                    None => session.config.clone(),
                };
                let cfg_b = apply_config_patch_value(&session.config, patch_b)
                    .map_err(|e| e.to_string())?;
                let plate_a = if parsed.mode == RenderMode::Plate {
                    None
                } else {
                    Some(slice_with_config(session, &cfg_a).map_err(|e| e.to_string())?)
                };
                let plate_b = if parsed.mode == RenderMode::Plate {
                    None
                } else {
                    Some(slice_with_config(session, &cfg_b).map_err(|e| e.to_string())?)
                };
                let preset = match parsed.camera {
                    ScreenshotCamera::Isometric | ScreenshotCamera::Current => {
                        CameraPreset::Isometric
                    }
                    ScreenshotCamera::Top => CameraPreset::Top,
                    ScreenshotCamera::Front => CameraPreset::Front,
                    ScreenshotCamera::Right => CameraPreset::Right,
                };
                let custom_cam = if parsed.camera == ScreenshotCamera::Current {
                    live_camera
                } else {
                    None
                };
                let req = RenderRequest {
                    width: w,
                    height: h,
                    mode: parsed.mode,
                    camera_preset: preset,
                    max_layer: parsed.max_layer,
                    single_layer_only: parsed.single_layer_only,
                    color_mode: parsed.color_mode,
                };
                let bytes_a = render_png_with_camera(
                    session.prepared_mesh.as_deref(),
                    plate_a.as_ref(),
                    &cfg_a,
                    &req,
                    custom_cam,
                )
                .map_err(|e| e.to_string())?;
                let bytes_b = render_png_with_camera(
                    session.prepared_mesh.as_deref(),
                    plate_b.as_ref(),
                    &cfg_b,
                    &req,
                    custom_cam,
                )
                .map_err(|e| e.to_string())?;
                (bytes_a, bytes_b)
            } else {
                let prev = last_screenshot_png.clone().ok_or_else(|| {
                    "no prior screenshot cached; capture `screenshot` first or supply `patch_b`"
                        .to_owned()
                })?;
                let (cur, _) = render_screenshot_bytes(
                    session,
                    parsed.mode,
                    parsed.camera,
                    parsed.max_layer,
                    parsed.single_layer_only,
                    parsed.color_mode,
                    w,
                    h,
                    live_camera,
                )?;
                (prev, cur)
            };

            let diff = compare_png(&png_a, &png_b).map_err(|e| e.to_string())?;
            let b64 = base64::engine::general_purpose::STANDARD.encode(&diff.png);
            let summary = json!({
                "composite_width": diff.width,
                "composite_height": diff.height,
                "diff_pixels": diff.diff_pixels,
                "total_pixels": diff.total_pixels,
                "diff_percent": diff.diff_percent,
                "mean_abs_diff": diff.mean_abs_diff,
            });
            Ok(LocalDispatchOutput {
                mcp_result: json!({
                    "content": [
                        {
                            "type": "image",
                            "data": b64,
                            "mimeType": "image/png"
                        },
                        {
                            "type": "text",
                            "text": serde_json::to_string_pretty(&summary).map_err(|e| e.to_string())?
                        }
                    ]
                }),
                activity_label: Some("compare_screenshot".to_owned()),
                executed_command: None,
                view_sync: None,
            })
        }
        "set_view" => {
            let parsed: SetViewArgs = if args.is_null() {
                SetViewArgs::default()
            } else {
                serde_json::from_value(args).map_err(|e| format!("invalid arguments: {e}"))?
            };
            let view_sync = ViewSyncRequest {
                view_mode: parsed.mode,
                camera_preset: parsed.camera,
                max_layer: parsed.max_layer,
                single_layer_only: parsed.single_layer_only,
                color_mode: parsed.color_mode,
            };
            let payload = json!({
                "ok": true,
                "live_studio": mode_info.studio_running,
                "applied": view_sync,
            });
            Ok(LocalDispatchOutput {
                mcp_result: text_content(payload)?,
                activity_label: Some("set_view".to_owned()),
                executed_command: None,
                view_sync: Some(view_sync),
            })
        }
        other => Err(format!("unknown MCP tool `{other}`")),
    }
}

/// Read one MCP resource URI against `session`.
pub fn read_resource_on_session(
    session: &mut Session,
    uri: &str,
    mode_info: &ModeInfo,
) -> Result<Value, String> {
    match uri {
        "layercraft://skill" => Ok(json!({
            "contents": [{
                "uri": uri,
                "mimeType": "text/markdown",
                "text": EMBEDDED_SKILL_MD,
            }]
        })),
        "layercraft://commands" => {
            let text = serde_json::to_string_pretty(&list_command_summaries(None))
                .map_err(|e| e.to_string())?;
            Ok(json!({
                "contents": [{
                    "uri": uri,
                    "mimeType": "application/json",
                    "text": text,
                }]
            }))
        }
        "layercraft://printers" => {
            let saved = execute(session, "printer.list", Value::Null).map_err(|e| e.to_string())?;
            let text = serde_json::to_string_pretty(&saved).map_err(|e| e.to_string())?;
            Ok(json!({
                "contents": [{
                    "uri": uri,
                    "mimeType": "application/json",
                    "text": text,
                }]
            }))
        }
        "layercraft://plate/summary" => {
            let mesh_info = execute(session, "plate.inspect", Value::Null).ok();
            let payload = json!({
                "mode": mode_info.mode,
                "object_count": session.objects.len(),
                "modifier_count": session.modifiers.len(),
                "active_plate": {
                    "id": session.active_plate_id(),
                    "index": session.active_plate_index(),
                    "name": session.active_plate_name(),
                },
                "plates": session.list_plates(),
                "active_profiles": session.active_profiles,
                "history": session.history_status(),
                "mesh": mesh_info,
                "layer_count": session.sliced.as_ref().map(|s| s.layers.len()),
                "gcode_stats": session.last_gcode.as_ref().map(|g| &g.stats),
            });
            let text = serde_json::to_string_pretty(&payload).map_err(|e| e.to_string())?;
            Ok(json!({
                "contents": [{
                    "uri": uri,
                    "mimeType": "application/json",
                    "text": text,
                }]
            }))
        }
        _ => Err(format!("unknown resource URI `{uri}`")),
    }
}

/// Helper used by a live [`bridge::StudioBridgeHost`] to process one incoming IPC request.
#[cfg(not(target_arch = "wasm32"))]
pub fn handle_studio_bridge_request(
    session: &mut Session,
    last_screenshot_png: &mut Option<Vec<u8>>,
    info: &StudioDiscoveryInfo,
    live_camera: Option<&OrbitCamera>,
    req: BridgeRequest,
) -> (BridgeResponse, Option<LocalDispatchOutput>) {
    let mode_info = ModeInfo {
        mode: "live_studio".to_owned(),
        studio_running: true,
        studio_pid: Some(info.pid),
        socket_path: Some(info.endpoint.clone()),
    };
    match req.op {
        BridgeOp::Ping => (
            BridgeResponse {
                id: req.id,
                ok: true,
                result: serde_json::to_value(info).unwrap_or(Value::Null),
                error: None,
            },
            None,
        ),
        BridgeOp::ToolCall { name, arguments } => {
            match dispatch_tool_on_session(
                session,
                last_screenshot_png,
                &name,
                arguments,
                &mode_info,
                live_camera,
            ) {
                Ok(out) => (
                    BridgeResponse {
                        id: req.id,
                        ok: true,
                        result: out.mcp_result.clone(),
                        error: None,
                    },
                    Some(out),
                ),
                Err(msg) => (
                    BridgeResponse {
                        id: req.id,
                        ok: false,
                        result: Value::Null,
                        error: Some(msg),
                    },
                    None,
                ),
            }
        }
        BridgeOp::ReadResource { uri } => {
            match read_resource_on_session(session, &uri, &mode_info) {
                Ok(res) => (
                    BridgeResponse {
                        id: req.id,
                        ok: true,
                        result: res,
                        error: None,
                    },
                    None,
                ),
                Err(msg) => (
                    BridgeResponse {
                        id: req.id,
                        ok: false,
                        result: Value::Null,
                        error: Some(msg),
                    },
                    None,
                ),
            }
        }
    }
}

impl McpServer {
    /// Construct an MCP server configured with connection mode and optional custom Studio socket.
    pub fn new(mode: McpConnectMode, socket_override: Option<PathBuf>) -> Self {
        let affinity = match mode {
            McpConnectMode::Standalone => SessionAffinity::Local,
            McpConnectMode::Connect => SessionAffinity::Studio,
            McpConnectMode::Auto => SessionAffinity::Undecided,
        };
        #[allow(unused_mut)]
        let mut session = Session::default();
        #[cfg(not(target_arch = "wasm32"))]
        {
            let _ = session.load_printers_from_disk();
        }
        Self {
            session,
            last_screenshot_png: None,
            mode,
            socket_override,
            affinity,
            #[cfg(not(target_arch = "wasm32"))]
            bridge_client: StudioBridgeClient::new(),
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn target_endpoint(&self) -> PathBuf {
        self.socket_override
            .clone()
            .unwrap_or_else(default_socket_path)
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn local_mode_info(&self, probe: Option<&StudioDiscoveryInfo>) -> ModeInfo {
        match (self.mode, self.affinity, probe) {
            (McpConnectMode::Standalone, _, _) | (_, SessionAffinity::Local, _) => ModeInfo {
                mode: "headless".to_owned(),
                studio_running: false,
                studio_pid: None,
                socket_path: None,
            },
            (_, _, Some(info)) => ModeInfo {
                mode: "live_studio".to_owned(),
                studio_running: true,
                studio_pid: Some(info.pid),
                socket_path: Some(info.endpoint.clone()),
            },
            (McpConnectMode::Connect, _, None) => ModeInfo {
                mode: "live_studio_disconnected".to_owned(),
                studio_running: false,
                studio_pid: None,
                socket_path: Some(self.target_endpoint().to_string_lossy().into_owned()),
            },
            (McpConnectMode::Auto, _, None) => ModeInfo {
                mode: "headless".to_owned(),
                studio_running: false,
                studio_pid: None,
                socket_path: None,
            },
        }
    }

    /// Handle one JSON-RPC 2.0 request line, returning `None` for notifications.
    pub fn handle_line(&mut self, line: &str) -> Option<String> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return None;
        }
        let req: JsonRpcReq = match serde_json::from_str(trimmed) {
            Ok(r) => r,
            Err(e) => {
                return Some(
                    json!({
                        "jsonrpc": "2.0",
                        "id": Value::Null,
                        "error": { "code": -32700, "message": format!("parse error: {e}") }
                    })
                    .to_string(),
                );
            }
        };

        // Notifications have no `id` and must not emit a response.
        let id = req.id?;

        let response = match req.method.as_str() {
            "initialize" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "serverInfo": { "name": "layercraft-mcp", "version": env!("CARGO_PKG_VERSION") },
                    "capabilities": { "tools": {}, "resources": {} }
                }
            }),
            "ping" => json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
            "tools/list" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": { "tools": tool_definitions() }
            }),
            "tools/call" => self.handle_tool_call(id, req.params),
            "resources/list" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "resources": [
                        {
                            "uri": "layercraft://skill",
                            "name": "LayerCraft AI Agent Guide",
                            "description": "Bundled workflow, MCP, and Printability/DFM skill guide.",
                            "mimeType": "text/markdown"
                        },
                        {
                            "uri": "layercraft://plate/summary",
                            "name": "Active Plate Summary",
                            "description": "Current objects, modifiers, profiles, layer count, and G-code statistics.",
                            "mimeType": "application/json"
                        },
                        {
                            "uri": "layercraft://commands",
                            "name": "Command Summaries",
                            "description": "Compact list of all LayerCraft engine commands.",
                            "mimeType": "application/json"
                        },
                        {
                            "uri": "layercraft://printers",
                            "name": "Saved Printers",
                            "description": "Redacted list of configured LAN/local printer profiles.",
                            "mimeType": "application/json"
                        }
                    ]
                }
            }),
            "resources/read" => self.handle_resource_read(id, req.params),
            other => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": format!("method not found: {other}") }
            }),
        };

        Some(response.to_string())
    }

    fn handle_resource_read(&mut self, id: Value, params: Value) -> Value {
        let Some(uri) = params.get("uri").and_then(Value::as_str) else {
            return json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32602, "message": "missing resource `uri`" }
            });
        };

        match self.dispatch_resource(uri) {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err(msg) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32602, "message": msg }
            }),
        }
    }

    fn dispatch_resource(&mut self, uri: &str) -> Result<Value, String> {
        #[cfg(not(target_arch = "wasm32"))]
        if uri == "layercraft://plate/summary" && self.should_route_studio(false)? {
            let endpoint = self.target_endpoint();
            let resp = self.bridge_client.send_request(
                &endpoint,
                BridgeOp::ReadResource {
                    uri: uri.to_owned(),
                },
            )?;
            return if resp.ok {
                Ok(resp.result)
            } else {
                Err(resp
                    .error
                    .unwrap_or_else(|| "bridge resource read failed".to_owned()))
            };
        }

        let mode_info = self.current_mode_info();
        read_resource_on_session(&mut self.session, uri, &mode_info)
    }

    fn current_mode_info(&self) -> ModeInfo {
        #[cfg(not(target_arch = "wasm32"))]
        {
            if self.mode == McpConnectMode::Standalone || self.affinity == SessionAffinity::Local {
                return self.local_mode_info(None);
            }
            let probe = StudioBridgeClient::probe_studio(self.socket_override.as_deref());
            self.local_mode_info(probe.as_ref())
        }
        #[cfg(target_arch = "wasm32")]
        {
            ModeInfo::default()
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn should_route_studio(&mut self, lock_local_if_absent: bool) -> Result<bool, String> {
        match self.mode {
            McpConnectMode::Standalone => Ok(false),
            McpConnectMode::Connect => {
                let endpoint = self.target_endpoint();
                if StudioBridgeClient::probe_studio(Some(&endpoint)).is_none() {
                    self.bridge_client.reset();
                    return Err(format!(
                        "LayerCraft Studio is not running (expected bridge at {}). Launch `layercraft` desktop app or run `layercraft-cli mcp --standalone`.",
                        endpoint.display()
                    ));
                }
                self.affinity = SessionAffinity::Studio;
                Ok(true)
            }
            McpConnectMode::Auto => match self.affinity {
                SessionAffinity::Local => Ok(false),
                SessionAffinity::Studio => {
                    let endpoint = self.target_endpoint();
                    if StudioBridgeClient::probe_studio(Some(&endpoint)).is_some() {
                        Ok(true)
                    } else {
                        self.bridge_client.reset();
                        Err(format!(
                            "Lost connection to live LayerCraft Studio at {} (restart Studio or restart `layercraft-cli mcp` for a standalone session).",
                            endpoint.display()
                        ))
                    }
                }
                SessionAffinity::Undecided => {
                    let endpoint = self.target_endpoint();
                    if StudioBridgeClient::probe_studio(Some(&endpoint)).is_some() {
                        self.affinity = SessionAffinity::Studio;
                        Ok(true)
                    } else {
                        if lock_local_if_absent {
                            self.affinity = SessionAffinity::Local;
                        }
                        Ok(false)
                    }
                }
            },
        }
    }

    fn handle_tool_call(&mut self, id: Value, params: Value) -> Value {
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32602, "message": "missing tool `name`" }
            });
        };
        let args = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));

        match self.dispatch_tool(name, args) {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err(msg) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "isError": true,
                    "content": [{ "type": "text", "text": msg }]
                }
            }),
        }
    }

    fn dispatch_tool(&mut self, name: &str, args: Value) -> Result<Value, String> {
        #[cfg(not(target_arch = "wasm32"))]
        {
            if is_local_metadata_tool(name) {
                let info = ModeInfo::default();
                let out = dispatch_tool_on_session(
                    &mut self.session,
                    &mut self.last_screenshot_png,
                    name,
                    args,
                    &info,
                    None,
                )?;
                return Ok(out.mcp_result);
            }

            if name == "version" {
                let probe = if self.mode == McpConnectMode::Standalone
                    || self.affinity == SessionAffinity::Local
                {
                    None
                } else {
                    StudioBridgeClient::probe_studio(self.socket_override.as_deref())
                };
                if probe.is_some() && self.mode == McpConnectMode::Auto {
                    self.affinity = SessionAffinity::Studio;
                }
                let info = self.local_mode_info(probe.as_ref());
                let out = dispatch_tool_on_session(
                    &mut self.session,
                    &mut self.last_screenshot_png,
                    name,
                    args,
                    &info,
                    None,
                )?;
                return Ok(out.mcp_result);
            }

            let is_mutating = matches!(
                name,
                "execute"
                    | "batch"
                    | "config_patch"
                    | "analyze_printability"
                    | "save_gcode"
                    | "upload_to_printer"
            );
            if self.should_route_studio(is_mutating)? {
                let endpoint = self.target_endpoint();
                let resp = self.bridge_client.send_request(
                    &endpoint,
                    BridgeOp::ToolCall {
                        name: name.to_owned(),
                        arguments: args,
                    },
                )?;
                return if resp.ok {
                    Ok(resp.result)
                } else {
                    Err(resp
                        .error
                        .unwrap_or_else(|| "Studio bridge error".to_owned()))
                };
            }
        }

        let info = self.current_mode_info();
        let out = dispatch_tool_on_session(
            &mut self.session,
            &mut self.last_screenshot_png,
            name,
            args,
            &info,
            None,
        )?;
        Ok(out.mcp_result)
    }

    /// Run blocking JSON-RPC 2.0 loop over stdin/stdout until EOF.
    pub fn serve_stdio(&mut self) -> Result<(), String> {
        let stdin = std::io::stdin();
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        for line_res in stdin.lock().lines() {
            let line = line_res.map_err(|e| format!("stdin read error: {e}"))?;
            if let Some(resp) = self.handle_line(&line) {
                writeln!(out, "{resp}").map_err(|e| format!("stdout write error: {e}"))?;
                out.flush()
                    .map_err(|e| format!("stdout flush error: {e}"))?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call_tool(srv: &mut McpServer, id: u64, name: &str, args: Value) -> Value {
        let req = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": { "name": name, "arguments": args }
        })
        .to_string();
        serde_json::from_str(&srv.handle_line(&req).unwrap()).unwrap()
    }

    #[test]
    fn mcp_lifecycle_slice_and_screenshot_png() {
        let mut srv = McpServer::default();

        let init: Value = serde_json::from_str(
            &srv.handle_line(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(init["result"]["serverInfo"]["name"], "layercraft-mcp");
        assert!(init["result"]["capabilities"]["resources"].is_object());

        // Notifications emit no response.
        assert!(
            srv.handle_line(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
                .is_none()
        );

        let list: Value = serde_json::from_str(
            &srv.handle_line(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(list["result"]["tools"].as_array().unwrap().len(), 18);

        let load_resp = call_tool(
            &mut srv,
            3,
            "execute",
            json!({"command": "plate.add_sample_shape", "params": {"shape": "cube20"}}),
        );
        assert!(load_resp["result"]["isError"].is_null());

        let slice_resp = call_tool(&mut srv, 4, "execute", json!({"command": "slice.run"}));
        assert!(slice_resp["result"]["isError"].is_null());

        let shot = call_tool(
            &mut srv,
            5,
            "screenshot",
            json!({"mode": "preview", "max_layer": 25, "width": 320, "height": 240}),
        );
        let b64 = shot["result"]["content"][0]["data"].as_str().unwrap();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .unwrap();
        assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert!(bytes.len() > 1000);
    }

    #[test]
    fn mcp_power_up_tools_and_resources_work_headlessly() {
        let mut srv = McpServer::default();

        let list_cmds = call_tool(&mut srv, 1, "list_commands", json!({"prefix": "config."}));
        let txt = list_cmds["result"]["content"][0]["text"].as_str().unwrap();
        assert!(txt.contains("config.patch"));

        let desc = call_tool(
            &mut srv,
            2,
            "describe_command",
            json!({"name": "plate.analyze_printability"}),
        );
        assert!(desc["result"]["isError"].is_null());
        assert!(
            desc["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("auto_slice")
        );

        let skill = call_tool(
            &mut srv,
            3,
            "skill_reference",
            json!({"section": "Live Studio"}),
        );
        assert!(skill["result"]["isError"].is_null());
        assert!(
            skill["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("layercraft-cli mcp --connect")
        );

        let batch = call_tool(
            &mut srv,
            4,
            "batch",
            json!({
                "steps": [
                    { "command": "plate.add_sample_shape", "params": { "shape": "bridge_arch" } },
                    { "command": "config.patch", "params": { "process": { "sparse_infill_density": 0.15 } } }
                ]
            }),
        );
        assert!(batch["result"]["isError"].is_null());

        let dfm = call_tool(&mut srv, 5, "analyze_printability", json!({}));
        assert!(dfm["result"]["isError"].is_null());
        let dfm_json: Value =
            serde_json::from_str(dfm["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(dfm_json["sliced"], true);

        let cmp_slice = call_tool(
            &mut srv,
            6,
            "compare_slices",
            json!({ "patch_b": { "process": { "sparse_infill_density": 0.40 } } }),
        );
        assert!(cmp_slice["result"]["isError"].is_null());

        let cmp_shot = call_tool(
            &mut srv,
            7,
            "compare_screenshot",
            json!({
                "patch_b": { "process": { "support": { "enabled": true } } },
                "width": 160,
                "height": 120
            }),
        );
        assert!(cmp_shot["result"]["isError"].is_null());
        let cmp_b64 = cmp_shot["result"]["content"][0]["data"].as_str().unwrap();
        let cmp_png = base64::engine::general_purpose::STANDARD
            .decode(cmp_b64)
            .unwrap();
        assert!(cmp_png.starts_with(b"\x89PNG\r\n\x1a\n"));

        // Resources list & read
        let res_list: Value = serde_json::from_str(
            &srv.handle_line(r#"{"jsonrpc":"2.0","id":8,"method":"resources/list"}"#)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(res_list["result"]["resources"].as_array().unwrap().len(), 4);

        let res_skill: Value = serde_json::from_str(
            &srv.handle_line(
                r#"{"jsonrpc":"2.0","id":9,"method":"resources/read","params":{"uri":"layercraft://skill"}}"#,
            )
            .unwrap(),
        )
        .unwrap();
        assert!(
            res_skill["result"]["contents"][0]["text"]
                .as_str()
                .unwrap()
                .contains("LayerCraft Agent Slicing Guide")
        );
    }

    #[test]
    fn hostile_json_rpc_never_panics() {
        let mut srv = McpServer::default();
        assert!(srv.handle_line("").is_none());
        let bad = srv.handle_line("{not json").unwrap();
        let v: Value = serde_json::from_str(&bad).unwrap();
        assert_eq!(v["error"]["code"], -32700);
        let unknown = srv
            .handle_line(r#"{"jsonrpc":"2.0","id":9,"method":"bogus"}"#)
            .unwrap();
        let vu: Value = serde_json::from_str(&unknown).unwrap();
        assert_eq!(vu["error"]["code"], -32601);
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn live_studio_ipc_bridge_connects_and_syncs_state() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::thread;
        use std::time::Duration;

        use crate::bridge::StudioBridgeHost;

        let sock_path = std::env::temp_dir().join(format!(
            "lc_bridge_{}_{}.sock",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));

        // Before Studio starts, `--connect` returns a clear error.
        let mut missing_client = McpServer::new(McpConnectMode::Connect, Some(sock_path.clone()));
        let err_resp = call_tool(&mut missing_client, 1, "inspect_plate", json!({}));
        assert_eq!(err_resp["result"]["isError"], true);
        assert!(
            err_resp["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("LayerCraft Studio is not running")
        );

        let woken = Arc::new(AtomicBool::new(false));
        let woken_cb = Arc::clone(&woken);
        let host = StudioBridgeHost::bind(
            Some(sock_path.clone()),
            Arc::new(move || {
                woken_cb.store(true, Ordering::Relaxed);
            }),
        )
        .unwrap();
        let host_info = host.info().clone();
        let stop_ui = Arc::new(AtomicBool::new(false));
        let stop_ui_loop = Arc::clone(&stop_ui);
        let shared_layers = Arc::new(std::sync::Mutex::new(0usize));
        let shared_layers_loop = Arc::clone(&shared_layers);

        let ui_thread = thread::spawn(move || {
            let mut session = Session::default();
            let mut last_shot = None;
            let live_cam = OrbitCamera::from_preset(CameraPreset::Front, 250.0, 250.0, 20.0);
            while !stop_ui_loop.load(Ordering::Relaxed) {
                for incoming in host.drain_requests() {
                    let (resp, out) = handle_studio_bridge_request(
                        &mut session,
                        &mut last_shot,
                        &host_info,
                        Some(&live_cam),
                        incoming.request,
                    );
                    if let Some(o) = out
                        && o.executed_command.is_some()
                    {
                        *shared_layers_loop.lock().unwrap() =
                            session.sliced.as_ref().map(|s| s.layers.len()).unwrap_or(0);
                    }
                    let _ = incoming.reply.send(resp);
                }
                thread::sleep(Duration::from_millis(10));
            }
        });

        // Auto mode automatically attaches to the running Studio host.
        let mut mcp_client = McpServer::new(McpConnectMode::Auto, Some(sock_path.clone()));
        let ver = call_tool(&mut mcp_client, 10, "version", json!({}));
        let ver_json: Value =
            serde_json::from_str(ver["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(ver_json["mode"], "live_studio");
        assert_eq!(ver_json["studio_running"], true);

        let batch = call_tool(
            &mut mcp_client,
            11,
            "batch",
            json!({
                "steps": [
                    { "command": "plate.add_sample_shape", "params": { "shape": "cube20" } },
                    { "command": "slice.run", "params": {} }
                ]
            }),
        );
        assert!(batch["result"]["isError"].is_null());
        assert_eq!(*shared_layers.lock().unwrap(), 100);
        assert!(woken.load(Ordering::Relaxed));

        let shot = call_tool(
            &mut mcp_client,
            12,
            "screenshot",
            json!({ "camera": "current", "sync_view": true, "width": 200, "height": 150 }),
        );
        assert!(shot["result"]["isError"].is_null());
        assert_eq!(shot["result"]["content"][0]["mimeType"], "image/png");

        let set_view = call_tool(
            &mut mcp_client,
            13,
            "set_view",
            json!({ "mode": "preview", "camera": "top", "max_layer": 40 }),
        );
        assert!(set_view["result"]["isError"].is_null());

        stop_ui.store(true, Ordering::Relaxed);
        ui_thread.join().unwrap();
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn mcp_bgcode_save_printers_resource_and_upload_work() {
        use std::io::{Read as _, Write as _};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let srv_handle = std::thread::spawn(move || {
            for _ in 0..4 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                let mut header_end = None;
                while header_end.is_none() {
                    let n = stream.read(&mut tmp).unwrap();
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        header_end = Some(pos + 4);
                    }
                }
                let h_end = header_end.unwrap();
                let head = String::from_utf8_lossy(&buf[..h_end]).into_owned();
                let req_line = head.lines().next().unwrap_or("");
                let content_len: usize = head
                    .lines()
                    .find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        if k.trim().eq_ignore_ascii_case("content-length") {
                            v.trim().parse().ok()
                        } else {
                            None
                        }
                    })
                    .unwrap_or(0);
                while buf.len() < h_end + content_len {
                    let n = stream.read(&mut tmp).unwrap();
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                }
                let (st, resp_json) = if req_line.starts_with("GET /api/v1/info ") {
                    ("200 OK", r#"{"firmware":"6.2.0","type":"COREONE"}"#)
                } else if req_line.starts_with("GET /api/v1/status ") {
                    ("200 OK", r#"{"printer":{"state":"IDLE"}}"#)
                } else if req_line.starts_with("GET /api/v1/storage ") {
                    (
                        "200 OK",
                        r#"{"storage_list":[{"name":"usb","ready":true}]}"#,
                    )
                } else {
                    assert!(req_line.starts_with("PUT /api/v1/files/usb/"));
                    assert!(layercraft_engine::is_bgcode(
                        &buf[h_end..h_end + content_len]
                    ));
                    ("201 Created", r#"{"ok":true}"#)
                };
                let raw = format!(
                    "HTTP/1.1 {st}\r\nContent-Length: {}\r\nConnection: close\r\nContent-Type: application/json\r\n\r\n{resp_json}",
                    resp_json.len()
                );
                stream.write_all(raw.as_bytes()).unwrap();
            }
        });

        let mut mcp = McpServer::new(McpConnectMode::Standalone, None);
        mcp.session.printers.clear();
        call_tool(
            &mut mcp,
            1,
            "execute",
            json!({
                "command": "plate.add_sample_shape",
                "params": { "shape": "cube20" }
            }),
        );
        call_tool(&mut mcp, 2, "execute", json!({ "command": "slice.run" }));

        let bg_path =
            std::env::temp_dir().join(format!("layercraft_mcp_test_{}.bgcode", std::process::id()));
        let saved = call_tool(
            &mut mcp,
            3,
            "save_gcode",
            json!({ "path": bg_path.to_string_lossy(), "evaluate": true }),
        );
        assert!(saved["result"]["isError"].is_null());
        assert!(
            saved["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("\"format\": \"bgcode\"")
        );
        assert!(layercraft_engine::is_bgcode(
            &std::fs::read(&bg_path).unwrap()
        ));
        let _ = std::fs::remove_file(&bg_path);

        call_tool(
            &mut mcp,
            4,
            "execute",
            json!({
                "command": "printer.upsert",
                "params": {
                    "profile": {
                        "id": "coreone-test",
                        "name": "CORE One Test",
                        "protocol": "prusa_link",
                        "base_url": format!("http://{addr}"),
                        "auth": { "kind": "api_key", "api_key": "super-secret-api-key" }
                    }
                }
            }),
        );

        let res_raw = mcp
            .handle_line(r#"{"jsonrpc":"2.0","id":5,"method":"resources/read","params":{"uri":"layercraft://printers"}}"#)
            .unwrap();
        let res_val: Value = serde_json::from_str(&res_raw).unwrap();
        let text = res_val["result"]["contents"][0]["text"].as_str().unwrap();
        assert!(text.contains("coreone-test"));
        assert!(!text.contains("super-secret-api-key"));

        let st_resp = call_tool(
            &mut mcp,
            6,
            "printer_status",
            json!({ "printer_id": "coreone-test" }),
        );
        assert!(st_resp["result"]["isError"].is_null());
        assert!(
            st_resp["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("COREONE")
        );

        let up_resp = call_tool(
            &mut mcp,
            7,
            "upload_to_printer",
            json!({ "printer_id": "coreone-test", "filename": "cube.bgcode" }),
        );
        assert!(up_resp["result"]["isError"].is_null());
        assert!(
            up_resp["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("\"start_print_requested\": false")
        );
        srv_handle.join().unwrap();
    }
}
