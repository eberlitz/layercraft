---
name: layercraft-oracle
description: "Use when adding, running, or debugging oracle reference tests, OrcaSlicer black-box comparisons, or complexity ladder tiers."
---

# Slicer Oracle & Complexity Ladder Workflow

## Why Physical Invariants Instead of G-Code Text Diffs
Two valid slicers differ in start vertices, travel ordering, floating-point formatting, and comments. Never assert byte-identical G-code against another slicer.

## Verification Layers
1. **Analytic Oracle (`source = "analytic"`):** Closed-form geometry checks (exact layer count, `max_z`, bounding box, zero safety violations, analytical filament volume bounds).
2. **External Black-Box CLI Oracle (`tools/orca-oracle/`):** Runs `flatpak run com.orcaslicer.OrcaSlicer --slice 0 --arrange 1 --export-statistics` out-of-process on identical synthetic `recipe.json` meshes and settings, recording `plate_1.statistics.json` + `layercraft-cli eval plate_1.gcode` metrics into `expected.json`.
   - Note: pass `"layer_change_gcode": ""` in black-box machine JSON because OrcaSlicer's default Klipper profile otherwise emits duplicate `;LAYER_CHANGE` entries in `plate_1.statistics.json`.
3. **Physical Invariant Evaluator (`layercraft-gcode::evaluate_gcode`):** Validates finite coordinates, bed bounds, positive extrusion monotonicity, balanced retractions, monotonic Z layers, per-feature extruded volumes, and 2D swept-width raster IoU against reference G-code.
4. **Complexity Ladder (`tests/ladder/`):** Tiered regression suite (`01-mesh-slice`, ...) tracking pass/fail frontier and execution timings (`cargo xtask ladder`).

## Commands
- `cargo xtask oracle` — run all oracle checks and regenerate `docs/oracle.md`
- `cargo xtask oracle --update-orca` — refresh black-box expectations via `tools/orca-oracle/run_orca_oracle.py`
- `cargo xtask ladder` — run tiered complexity ladder and regenerate `docs/ladder.md`
