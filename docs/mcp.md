# AI Agent Integration (MCP)

LayerCraft includes a built-in [Model Context Protocol](https://modelcontextprotocol.io) server (`layercraft-cli mcp`) implemented in `layercraft-mcp`. Because every plate edit, profile lookup, slicing pass, DFM check, and G-code verification goes through a schema-typed command registry (`layercraft-engine`), AI agents drive the exact same deterministic pipeline as the desktop Studio, browser WASM app, and headless CLI.

## Why LayerCraft for AI Agents

- **Connects live to an open LayerCraft Studio window (SolidCraft-style):** Just launch the `layercraft` desktop app and let your MCP client run `layercraft-cli mcp` — tool calls execute live on your visible Studio plate, camera, and preview, with a subtle `MCP Live` badge in the status bar. Prefer isolated headless automation? Pass `--standalone`.
- **Token-efficient discovery & atomic batching:** Agents use `list_commands` + `describe_command` instead of loading every JSON schema at once, patch single settings with `config_patch`, and chain setup + slicing into one rollback-safe `batch`.
- **Printability & DFM diagnostics:** `analyze_printability` (`plate.analyze_printability`) detects steep overhangs, detached floating islands, thin-wall collapses, low bed adhesion, non-manifold edges, and breaks down filament volume and print time per feature.
- **Visual closed-loop & A/B inspection:** `screenshot` renders `plate`, `preview`, or `layer_top_down` PNGs (including `camera: "current"` and `sync_view: true`), while `compare_screenshot` returns a 3-panel `A | B | Diff` composite and `compare_slices` computes volume/time/travel deltas plus per-layer 2D raster IoU.
- **Zero modal dialogs & panic-free errors:** Every invalid parameter, malformed STL/3MF, or unsliced state returns a structured error (`isError: true`) rather than panicking or blocking on UI popups.
- **Direct-to-disk verified G-code export:** `save_gcode` (`gcode.save_file`) writes `.gcode` straight to disk and returns compact stats plus physical invariant verification (`non_finite_values`, `out_of_bounds_moves`, `z_regressions`, volumetric flow, Z-hops, arcs).
- **Composable CAD-to-Print pipelines:** Pair LayerCraft with an upstream CAD MCP server (such as [SolidCraft](https://github.com/tangency-labs/solidcraft)) so an agent can design a parametric part, export STL/3MF, watch it land live in LayerCraft Studio, run DFM checks, visually inspect sliced layers, and save printer-ready G-code in one loop.

## Setup

### 1. Build Both Binaries

```sh
cargo build --release -p layercraft -p layercraft-cli
```

- Desktop Studio with Live MCP bridge: `./target/release/layercraft`
- MCP server CLI: `./target/release/layercraft-cli mcp`

### 2. Connection Modes (`layercraft-cli mcp`)

| Mode | Command | Behavior |
|---|---|---|
| **Auto (default)** | `layercraft-cli mcp` | Connects automatically to a running `layercraft` Studio instance if listening; otherwise runs in a headless session. |
| **Require Live Studio** | `layercraft-cli mcp --connect` | Connects to the running Studio window over IPC (errors clearly if Studio isn't open). |
| **Isolated Headless** | `layercraft-cli mcp --standalone` | Always runs an isolated headless session even when Studio is open. |
| **Custom Socket** | `layercraft-cli mcp --socket /path/to.sock` | Overrides `LAYERCRAFT_SOCKET` / `${XDG_RUNTIME_DIR:-/tmp}/layercraft-studio.sock` (implies `--connect`). |

On the desktop Studio side, the bridge starts automatically (`--bridge-socket <path>` to customize, `--no-bridge` to disable).

### 3. Register with Your MCP Client

#### Claude Code

```sh
claude mcp add layercraft -- /absolute/path/to/layercraft/target/release/layercraft-cli mcp
```

#### Generic MCP JSON (`Claude Desktop`, `Cursor`, `Antigravity`, etc.)

```json
{
  "mcpServers": {
    "layercraft": {
      "command": "/absolute/path/to/layercraft/target/release/layercraft-cli",
      "args": ["mcp"]
    }
  }
}
```

To force attaching only to an already-open desktop Studio window, pass `"args": ["mcp", "--connect"]`.

Protocol details (`crates/mcp/src/lib.rs`, `crates/mcp/src/bridge.rs`): JSON-RPC 2.0 over `stdin`/`stdout`, protocol version `2024-11-05`, server name `layercraft-mcp`, advertising `tools` and `resources`.

## MCP Tools (16)

| Tool | Purpose | Parameters |
|---|---|---|
| `version` | Report crate/protocol versions and live Studio vs headless connection state (`studio_running`, `studio_pid`, `socket_path`). | `{}` |
| `list_commands` | Token-efficient listing of command names and one-line summaries. | `prefix` (`string`, optional) |
| `describe_command` | Full JSON input/output schemas for one command. | `name` (`string`) |
| `commands` | List all engine commands with full JSON input and output schemas. | `{}` |
| `execute` | Execute any `layercraft-engine` command on the active session. | `command` (`string`), `params` (`object`, optional) |
| `batch` | Execute multiple engine commands cleanly with atomic rollback on error. | `steps` (`array`), `atomic` (`bool`, default `true`) |
| `config_patch` | Apply a partial JSON merge patch onto `PrintConfig` and validate. | `patch` (`object`) |
| `inspect_plate` | Return active `config`, `object_count`, `modifier_count`, `active_profiles`, `mesh`, `layer_count`, `gcode_stats`, `mode`. | `{}` |
| `inspect_layer` | Inspect a single sliced layer (Z height, region count, path count, per-role lengths). | `index` (`integer`) |
| `analyze_printability` | Run Printability / DFM diagnostics (auto-slices if needed). | `auto_slice` (`bool`, default `true`) |
| `save_gcode` | Emit G-code if needed, write directly to a file on disk, and optionally run physical safety evaluation. | `path` (`string`), `evaluate` (`bool`, default `true`) |
| `screenshot` | Render `plate`, `preview`, or `layer_top_down` as inline `image/png`. | See table below |
| `compare_slices` | Non-destructively A/B slice two configs (`patch_a` vs `patch_b`), reporting volume/time/travel/arc deltas and 2D IoU. | `patch_a` (`object`, opt), `patch_b` (`object`), `pixel_mm` (`number`, opt) |
| `compare_screenshot` | Render a 3-panel `A \| B \| Diff` PNG + pixel diff stats (`patch_a` vs `patch_b`, or cached prior screenshot vs current state). | `patch_a`/`patch_b` + render fields |
| `set_view` | Update live Studio view state (`mode`, `camera`, `max_layer`, `single_layer_only`, `color_mode`). | All optional |
| `skill_reference` | Read bundled first-party workflow & DFM guide (`skills/layercraft/SKILL.md`), full or by `##` section. | `section` (`string`, optional) |

### `screenshot` Parameters

| Field | Type | Default | Values / Notes |
|---|---|---|---|
| `mode` | string | `"preview"` | `"plate"`, `"preview"`, or `"layer_top_down"` (`"layer_topdown"` alias) |
| `camera` | string | `"isometric"` | `"isometric"`, `"top"`, `"front"`, `"right"`, or `"current"` (uses live Studio orbit camera) |
| `max_layer` | integer \| null | `null` | Highest 0-based layer index to render (`null` = all layers) |
| `single_layer_only` | boolean | `false` | Render only the layer at `max_layer` (ideal for top-down slice checks) |
| `color_mode` | string | `"feature"` | `"feature"`, `"speed"`, `"layer"`, `"layer_height"` |
| `width` | integer \| null | `800` | PNG width in pixels (clamped to `64..=1920`) |
| `height` | integer \| null | `600` | PNG height in pixels (clamped to `64..=1920`) |
| `sync_view` | boolean | `false` | When `true` in live Studio mode, synchronizes the open viewport to match |

## MCP Resources

- `layercraft://skill` (`text/markdown`) — First-party bundled AI agent workflow & DFM guide ([skills/layercraft/SKILL.md](../skills/layercraft/SKILL.md)).
- `layercraft://plate/summary` (`application/json`) — Live plate summary (routed through Studio when connected).
- `layercraft://commands` (`application/json`) — Compact engine command summaries.

## Engine Command Catalog (`execute`)

Call `list_commands` / `describe_command` (or `commands`) via MCP, or run `cargo run -p layercraft-cli -- commands`, for exact JSON Schemas across all **33** commands (`28` cross-platform + `5` native disk I/O commands):

### Plate & Objects

- `plate.clear` — Clear loaded objects, modifiers, cached slices, and generated G-code.
- `plate.add_mesh_file` *(native)* — Load an STL or 3MF file from disk (`path`, `append`, `name`).
- `plate.add_stl_bytes` — Load a binary or ASCII STL from a byte array (`bytes`, `append`, `name`).
- `plate.add_3mf_bytes` — Load a 3MF archive from a byte array (`bytes`, `append`, `name`).
- `plate.add_sample_shape` — Load a built-in watertight test shape (`shape`: `"cube20"`, `"cylinder"`, `"hollow_tube"`, `"stepped_block"`, `"thin_rib"`, `"bridge_arch"`, `"dome"`, `"tapered_wedge"`).
- `plate.inspect` — Inspect merged plate bounds, triangle count, volume, and manifold edges.
- `plate.analyze_printability` — Run Printability / DFM diagnostics (overhangs, floating islands, thin walls, adhesion, feature breakdown).
- `plate.list_objects` — List objects on the plate with IDs, names, transforms, and AABBs.
- `plate.remove_object` — Remove a plate object by `id`.
- `plate.transform_object` — Update `position_xy_mm`, `rotation_deg`, or `scale` on an object by `id`.
- `plate.auto_arrange` — Pack all objects onto the bed with non-overlapping spacing (`spacing_mm`).

### 3D Modifier Volumes

- `plate.add_modifier_primitive` — Add a parametric modifier (`primitive`: `{"kind": "box", ...}` or `{"kind": "cylinder", ...}`, `role`: `"modifier"`, `"support_blocker"`, `"support_enforcer"`).
- `plate.add_modifier_mesh_file` *(native)* — Load an STL or 3MF mesh from disk as a modifier volume.
- `plate.add_modifier_stl_bytes` — Add an STL mesh modifier volume from bytes.
- `plate.add_modifier_3mf_bytes` — Add a 3MF mesh modifier volume from bytes.
- `plate.list_modifiers` — List active modifier volumes with IDs, roles, transforms, and overrides.
- `plate.update_modifier` — Update role, transform, name, or parameter overrides by `id`.
- `plate.remove_modifier` — Remove a modifier volume by `id`.

### Config & Profiles

- `config.get` — Return the complete active `PrintConfig` JSON.
- `config.set` — Validate and replace the active `PrintConfig`.
- `config.patch` — Partially patch the active `PrintConfig` via RFC 7396 JSON Merge Patch and validate.
- `profile.list` — List loaded machine, filament, and process profiles (`kind`, `include_abstract`).
- `profile.load_json` — Load OrcaSlicer/Bambu-compatible profile JSON into the session library.
- `profile.load_file` *(native)* — Load JSON profile(s) from a file on disk.
- `profile.load_dir` *(native)* — Recursively load JSON profiles from a directory tree.
- `profile.resolve` — Resolve a named profile with its full `inherits` / `machine_model` chain.
- `profile.apply` — Apply `machine`, `filament`, and/or `process` presets onto the active `PrintConfig`.

### Slicing, Inspection, Comparison & G-code

- `slice.run` — Slice all loaded objects and modifiers with the active `PrintConfig`.
- `slice.compare` — Non-destructively slice and compare two config variants (deltas + 2D raster IoU).
- `layer.inspect` — Inspect layer `index` (Z height, layer height, fan %, estimated time, region count, path count, per-role lengths).
- `gcode.export` — Emit Klipper, Marlin, or Prusa `MarlinBuddy` G-code and print statistics.
- `gcode.evaluate` — Run the physical G-code invariant evaluator on a G-code string.
- `gcode.save_file` *(native)* — Emit G-code if needed, write directly to `path`, and optionally run physical safety evaluation.

## Example End-to-End Agent Session

1. **Confirm Live Studio or Headless mode** (`version`):

   ```json
   {}
   ```

2. **Load geometry, apply CORE One+ presets, and tune settings atomically** (`batch`):

   ```json
   {
     "atomic": true,
     "steps": [
       { "command": "plate.add_sample_shape", "params": { "shape": "bridge_arch" } },
       {
         "command": "profile.apply",
         "params": {
           "machine": "LayerCraft CORE One+ HF 0.4mm",
           "filament": "LayerCraft PLA (0.4mm HF Nozzle)",
           "process": "LayerCraft 0.20mm Balanced (0.4mm HF)",
           "reset_to_defaults": true
         }
       },
       { "command": "config.patch", "params": { "patch": { "process": { "avoid_crossing_perimeters": true } } } }
     ]
   }
   ```

3. **Run Printability / DFM diagnostics** (`analyze_printability`):
   Inspect `overall_risk`, `issues`, `floating_islands`, `overhang_triangle_area_mm2`, `thin_wall_dropped_layers`, `bed_contact_area_mm2`, and `by_feature`.

4. **Compare a support or layer-height trade-off** (`compare_slices` + `compare_screenshot`):

   ```json
   {
     "patch_b": { "process": { "support": { "enabled": true, "style": "tree" } } }
   }
   ```

5. **Sync Studio view and inspect critical layers visually** (`screenshot` + `inspect_layer`):

   ```json
   { "mode": "preview", "camera": "isometric", "color_mode": "feature", "sync_view": true }
   ```

6. **Save verified G-code directly to disk** (`save_gcode`):

   ```json
   { "path": "/abs/path/part.gcode", "evaluate": true }
   ```

   Verify `evaluation.safety.non_finite_values == 0` and `evaluation.safety.out_of_bounds_moves == 0`.
