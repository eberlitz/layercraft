---
name: layercraft
description: "Workflow guide for driving LayerCraft's clean-room 3D printer slicer over MCP, including connecting to a live LayerCraft Studio desktop window, running Printability/DFM diagnostics, comparing parameter variants, and exporting verified G-code."
---

# LayerCraft Agent Slicing Guide

## Overview

LayerCraft exposes a stateful, schema-typed slicing session through `layercraft-mcp` (`layercraft-cli mcp`) backed by the exact same `layercraft-engine` pipeline that powers the desktop Studio, browser WASM app, and headless CLI.

Key design rules for AI agents:
- Use `list_commands` (optionally with `prefix`) + `describe_command` instead of loading every JSON Schema up front.
- Use `config_patch` for targeted parameter edits rather than replacing the full `PrintConfig`.
- Use `batch` with `atomic: true` to combine plate setup, config tuning, and slicing into a single round-trip with automatic rollback on error.
- Use `save_gcode` (`gcode.save_file`) to write `.gcode` directly to disk and receive compact stats + safety evaluation instead of pulling multi-megabyte G-code text into context.

## Live Studio Connection

Like SolidCraft, `layercraft-cli mcp` automatically detects a running **LayerCraft Studio** desktop instance over local IPC and executes operations live against its open plate, camera, and 3D viewport:

- **`auto` mode (default):** `layercraft-cli mcp` connects to a running `layercraft` Studio window automatically when present (`LAYERCRAFT_SOCKET` or `${XDG_RUNTIME_DIR:-/tmp}/layercraft-studio.sock` on Unix). If no Studio window is open, it runs seamlessly in an isolated headless session.
- **`--connect` mode:** `layercraft-cli mcp --connect` (or `--socket <path>`) requires an open LayerCraft Studio window and errors explicitly if none is found.
- **`--standalone` mode:** `layercraft-cli mcp --standalone` forces an isolated headless session even when Studio is open (ideal for batch automation or parallel test runs).
- Call `version` first to check `mode` (`"live_studio"` vs `"headless"`), `studio_running`, `studio_pid`, and `socket_path`.
- Call `set_view` (or `screenshot` with `sync_view: true`) to steer the visible Studio camera, switch between `prepare` and `preview`, scrub `max_layer`, or change `color_mode`. Pass `camera: "current"` to capture the user's live orbit camera angle.

## Recommended Workflow

1. **Check status & discover commands:** Call `version` and, if needed, `list_commands` + `describe_command` for any unfamiliar operation.
2. **Load geometry & presets (`batch`):**
   - Load STL/3MF files with `plate.add_mesh_file` (or multi-plate `.3mf` projects with `project.load_file`), arrange multiple objects with `plate.auto_arrange`, and apply printer/filament/process presets via `profile.apply`.
   - Manage multi-plate `.3mf` projects with `plates.list`, `plates.select` (`index`, `id`, or `name`), `plates.add`, `plates.rename`, and `plates.remove` (inspect all plates at once via `inspect_plate`).
   - Use `config_patch` for quick overrides (layer height, infill density, wall generator, supports, brim, thumbnails).
3. **Slice & run DFM diagnostics:**
   - Call `analyze_printability` (automatically slices if needed). Review `overall_risk`, `issues`, `floating_islands`, `overhang_triangle_area_mm2`, `thin_wall_dropped_layers`, `bed_contact_area_mm2`, and `by_feature`.
   - Use `execute` with `session.undo` / `session.redo` / `session.history` to step through or inspect edit history, and `project.save_file` / `project.load_file` to save or reload full `.3mf` projects (objects, transforms, modifiers, active profiles, and print settings).
4. **Compare variants visually & quantitatively (optional):**
   - Use `compare_slices` (`patch_b` vs baseline or `patch_a`) to measure filament volume, print time, travel distance, arc count, and per-layer 2D raster IoU without modifying the active session.
   - Use `compare_screenshot` to get a side-by-side 3-panel `A | B | Diff` PNG (`image/png`).
5. **Inspect visually:**
   - Call `screenshot` in `preview` mode (`isometric`) and `layer_top_down` on critical overhang, bridge, thin-wall, or top-ironing layers (`inspect_layer` gives exact numeric lengths).
6. **Export verified G-code or `.bgcode`:**
   - Call `save_gcode` with `{ "path": "/abs/path/part.gcode", "evaluate": true }` (or `.bgcode` / `"format": "bgcode"` for compressed Binary G-code with CRC32 and thumbnails) and check `evaluation.safety` (`non_finite_values == 0`, `out_of_bounds_moves == 0`, `z_regressions == 0`).
7. **Direct Printer Status & LAN Upload (optional):**
   - Check saved printers via `layercraft://printers` (credentials redacted) or query live telemetry/temperatures/storages via `printer_status`.
   - Call `upload_to_printer` (`printer_id` or inline `profile`) to slice (if needed), run safety verification, encode `.bgcode`/`.gcode`, and upload directly over LAN (`PrusaLink`, `Prusa Connect Local`, `OctoPrint`, or `Moonraker`). Keep `start_print: false` (default) unless explicitly asked to start printing immediately.

## Printability & DFM Heuristics

When `analyze_printability` reports issues, apply targeted fixes via `config_patch` or `plate.add_modifier_primitive`:
- **Floating islands / steep unsupported overhangs:** Enable supports (`{"process": {"support": {"enabled": true, "style": "tree"}}}` or `"snug"`). If a specific area shouldn't be supported, add a `"support_blocker"` box/cylinder modifier.
- **Thin walls dropped (`thin_wall_dropped_layers`):** Switch to variable-width walls (`{"process": {"wall_generator": "arachne"}}`) or lower `process.outer_wall_width_mm`.
- **Low bed contact / tall slender part (`height_to_base_ratio > 3.5`):** Add outer adhesion (`{"process": {"brim_width_mm": 5.0}}`) or reorient larger flat faces down.
- **Localized strength without wasting filament:** Add a `plate.add_modifier_primitive` (`role: "modifier"`) with higher `wall_loops` or `sparse_infill_density` only around fastener bosses or stress zones.
- **Travel stringing across outer shells:** Enable combing (`{"process": {"avoid_crossing_perimeters": true}}`) and inspect outer-first vs inner-first (`process.wall_order`).

## CAD-to-Print Loop

When paired with an upstream CAD MCP server (such as SolidCraft):
1. Design or modify the parametric part in CAD and export binary STL or 3MF to a known path.
2. Load or reload it onto the LayerCraft plate (clearing prior objects or replacing on the same plate), run `analyze_printability`, and inspect the top-down/preview screenshots.
3. Feed DFM findings back to CAD (e.g., chamfer sharp 90° overhangs to 45°, thicken sub-0.4 mm fins to >= 0.8 mm, or widen mounting bosses) or resolve them cleanly in slicing with tree supports, Arachne walls, and modifier volumes.
4. Once `overall_risk` is `low` and visual screenshots look clean, run `save_gcode(..., evaluate: true)`.

## Troubleshooting

- **`LayerCraft Studio is not running` (`--connect` mode):** Start the `layercraft` desktop app on the same machine/user session, verify `LAYERCRAFT_SOCKET` if overridden, or drop `--connect` / pass `--standalone` to run headlessly.
- **Mesh outside bed (`fits_bed: false`):** Run `plate.auto_arrange`, inspect bounds with `inspect_plate`, or switch `profile.apply` to the target printer bed dimensions.
- **Out-of-bounds or unsafe G-code warning:** Confirm no manual object `position_xy_mm` places part geometry outside `0..bed_width_mm` x `0..bed_depth_mm`.
