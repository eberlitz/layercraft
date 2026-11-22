# AI Agent Integration (MCP)

LayerCraft includes a built-in [Model Context Protocol](https://modelcontextprotocol.io) server (`layercraft-cli mcp`) implemented in `layercraft-mcp`. Because every plate edit, profile lookup, slicing pass, and G-code check goes through a schema-typed command registry (`layercraft-engine`), AI agents drive the exact same deterministic pipeline as the desktop app, browser app, and CLI.

## Why LayerCraft for AI Agents

- **Schema-typed commands:** Every operation is registered with generated JSON input and output schemas (`commands` tool / `layercraft-cli commands`), so agents inspect exact parameter types instead of guessing UI clicks or undocumented flags.
- **Multimodal visual inspection:** The `screenshot` tool renders the unsliced build plate, multi-layer 3D toolpaths, or a single top-down layer directly to an inline `image/png` (`mode`: `plate`, `preview`, `layer_top_down`), letting vision-capable agents spot overhangs, floating regions, support gaps, or thin-wall collapses.
- **Physical G-code verification:** `gcode.evaluate` checks physical safety and motion invariants on generated G-code (non-finite values, out-of-bounds moves, layer counts, Z-hops, G2/G3 arcs, extruded volume, wall-width variation, layer-height variation) before a file ever reaches a printer.
- **Zero modal dialogs & panic-free errors:** Every invalid parameter, malformed STL/3MF, or unsliced state returns a structured error (`isError: true`) rather than panicking or blocking on UI popups.
- **Composable CAD-to-Print pipelines:** Pair LayerCraft with a CAD MCP server (such as [SolidCraft](https://github.com/tangency-labs/solidcraft)) so an agent can design a parametric part, export STL/3MF, load it onto the LayerCraft build plate, tune supports or infill modifiers, visually inspect sliced layers, and verify printer-ready G-code end to end.

## Setup

### 1. Build `layercraft-cli`

```sh
cargo build --release -p layercraft-cli
```

The compiled binary lives at `./target/release/layercraft-cli`. Alternatively, you can invoke it directly through `cargo run --release --quiet -p layercraft-cli -- mcp` from the repository root (useful during development, though prebuilding a release binary avoids Cargo startup latency).

### 2. Register with Your MCP Client

#### Claude Code

```sh
claude mcp add layercraft -- /absolute/path/to/layercraft/target/release/layercraft-cli mcp
```

#### Generic MCP JSON (`Claude Desktop`, `Cursor`, `Antigravity`, etc.)

Using a compiled release binary:

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

Or via `cargo run` inside the repository working directory:

```json
{
  "mcpServers": {
    "layercraft": {
      "command": "cargo",
      "args": [
        "run",
        "--release",
        "--quiet",
        "-p",
        "layercraft-cli",
        "--",
        "mcp"
      ],
      "cwd": "/absolute/path/to/layercraft"
    }
  }
}
```

Protocol details (`crates/mcp/src/lib.rs`): JSON-RPC 2.0 over `stdin`/`stdout`, protocol version `2024-11-05`, server name `layercraft-mcp`.

## MCP Tools

`layercraft-mcp` exposes five top-level MCP tools:

| Tool | Purpose | Parameters |
|---|---|---|
| `commands` | List all engine commands with JSON input and output schemas. | `{}` |
| `execute` | Execute any `layercraft-engine` command on the active session. | `command` (`string`), `params` (`object`, optional) |
| `inspect_plate` | Return active `config`, `mesh` inspection, `layer_count`, and `gcode_stats`. | `{}` |
| `inspect_layer` | Inspect a single sliced layer (Z height, region count, path count, per-role lengths). | `index` (`integer`) |
| `screenshot` | Render the build plate, 3D toolpaths, or a single top-down layer as `image/png`. | See table below |

### `screenshot` Parameters

| Field | Type | Default | Values / Notes |
|---|---|---|---|
| `mode` | string | `"preview"` | `"plate"`, `"preview"`, or `"layer_top_down"` (`"layer_topdown"` alias) |
| `camera` | string | `"isometric"` | `"isometric"`, `"top"`, `"front"`, `"right"` |
| `max_layer` | integer \| null | `null` | Highest 0-based layer index to render (`null` = all layers) |
| `single_layer_only` | boolean | `false` | Render only the layer at `max_layer` (ideal for top-down slice checks) |
| `color_mode` | string | `"feature"` | `"feature"`, `"speed"`, `"layer"`, `"layer_height"` |
| `width` | integer \| null | `800` | PNG width in pixels (clamped to `64..=1920`) |
| `height` | integer \| null | `600` | PNG height in pixels (clamped to `64..=1920`) |

Returns both a base64 `image` content block (`mimeType: "image/png"`) and a `text` summary reporting rendered dimensions, mode, and layer count.

## Engine Command Catalog (`execute`)

Call `commands` via MCP or run `cargo run -p layercraft-cli -- commands` to retrieve exact JSON Schemas for all **29** commands (`25` cross-platform + `4` native disk I/O commands):

### Plate & Objects

- `plate.clear` — Clear loaded objects, modifiers, cached slices, and generated G-code.
- `plate.add_mesh_file` *(native)* — Load an STL or 3MF file from disk (`path`, `append`, `name`).
- `plate.add_stl_bytes` — Load a binary or ASCII STL from a byte array (`bytes`, `append`, `name`).
- `plate.add_3mf_bytes` — Load a 3MF archive from a byte array (`bytes`, `append`, `name`).
- `plate.add_sample_shape` — Load a built-in watertight test shape (`shape`: `"cube20"`, `"cylinder"`, `"hollow_tube"`, `"stepped_block"`, `"thin_rib"`, `"bridge_arch"`, `"dome"`).
- `plate.inspect` — Inspect merged plate bounds, triangle count, volume, and manifold edges.
- `plate.list_objects` — List objects on the plate with IDs, names, transforms, and AABBs.
- `plate.remove_object` — Remove a plate object by `id`.
- `plate.transform_object` — Update `position_xy_mm`, `rotation_deg`, or `scale` on an object by `id`.
- `plate.auto_arrange` — Pack all objects onto the build plate with non-overlapping spacing (`spacing_mm`).

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
- `profile.list` — List loaded machine, filament, and process profiles (`kind`, `include_abstract`).
- `profile.load_json` — Load OrcaSlicer/Bambu-compatible profile JSON into the session library.
- `profile.load_file` *(native)* — Load JSON profile(s) from a file on disk.
- `profile.load_dir` *(native)* — Recursively load JSON profiles from a directory tree.
- `profile.resolve` — Resolve a named profile with its full `inherits` / `machine_model` chain.
- `profile.apply` — Apply `machine`, `filament`, and/or `process` presets onto the active `PrintConfig`.

### Slicing, Inspection & G-code

- `slice.run` — Slice all loaded objects and modifiers with the active `PrintConfig`.
- `layer.inspect` — Inspect layer `index` (Z height, layer height, fan %, estimated time, region count, path count, per-role lengths).
- `gcode.export` — Emit Klipper, Marlin, or Prusa `MarlinBuddy` G-code and print statistics.
- `gcode.evaluate` — Run the physical G-code invariant evaluator on a G-code string.

## Example End-to-End Agent Session

1. **Load a part onto the plate** (`execute` -> `plate.add_mesh_file` or `plate.add_sample_shape`):

   ```json
   {
     "command": "plate.add_sample_shape",
     "params": { "shape": "dome" }
   }
   ```

2. **Inspect available presets or apply starter profiles** (`execute` -> `profile.apply`):

   ```json
   {
     "command": "profile.apply",
     "params": {
       "machine": "LayerCraft CORE One+ HF 0.4mm",
       "filament": "LayerCraft PLA (0.4mm HF Nozzle)",
       "process": "LayerCraft 0.20mm Balanced (0.4mm HF)",
       "reset_to_defaults": true
     }
   }
   ```

3. **Optionally add a local reinforcement modifier** (`execute` -> `plate.add_modifier_primitive`):

   ```json
   {
     "command": "plate.add_modifier_primitive",
     "params": {
       "primitive": { "kind": "box", "width_mm": 12.0, "depth_mm": 12.0, "height_mm": 6.0 },
       "role": "modifier",
       "overrides": { "sparse_infill_density": 0.6, "wall_loops": 4 }
     }
   }
   ```

4. **Slice the plate** (`execute` -> `slice.run`):

   ```json
   { "command": "slice.run", "params": {} }
   ```

5. **Visually inspect both the 3D preview and an interior cross-section** (`screenshot`):

   ```json
   { "mode": "preview", "camera": "isometric", "color_mode": "feature", "width": 960, "height": 640 }
   ```

   ```json
   { "mode": "layer_top_down", "max_layer": 20, "single_layer_only": true, "width": 640, "height": 640 }
   ```

6. **Inspect exact metrics and verify exported G-code** (`inspect_layer`, `execute` -> `gcode.export`, `execute` -> `gcode.evaluate`):

   - Call `inspect_layer` with `{"index": 20}` to verify layer height, fan speed, and per-role lengths.
   - Call `execute` with `{"command": "gcode.export"}` to obtain the G-code text and print statistics (`layer_count`, `filament_volume_mm3`, etc.).
   - Call `execute` with `{"command": "gcode.evaluate", "params": {"gcode": "<exported G-code>"}}` to verify `safety.non_finite_values == 0` and `safety.out_of_bounds_moves == 0`.

## Replaying as Deterministic Scripts

The same command sequence used over MCP can be saved as a JSON array and executed without an LLM via `layercraft-cli run`:

```json
[
  { "command": "plate.add_sample_shape", "params": { "shape": "bridge_arch" } },
  {
    "command": "profile.apply",
    "params": {
      "machine": "LayerCraft Generic Klipper 250mm",
      "filament": "LayerCraft PETG (0.4mm HF Nozzle)",
      "process": "LayerCraft 0.16mm Quality",
      "reset_to_defaults": true
    }
  },
  { "command": "slice.run", "params": {} },
  { "command": "gcode.export", "params": {} }
]
```

```sh
cargo run -p layercraft-cli -- run script.json
```
