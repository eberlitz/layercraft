# LayerCraft Roadmap

- **Phase 0 — Governance, Clean-Room Harness & Oracle (Done):** Workspace crates, `cargo xtask` checks (`layers`, `licences`, `wasm`, `fast`, `oracle`, `ladder`), `AGENTS.md`, skills, G-code physical invariant evaluator, analytic + black-box `OrcaSlicer 2.5.2` oracle harness.
- **Phase 1 — Tier 01 Slice MVP (Done):** Exact Z-plane mesh slicing, classic perimeter offsetting, top/bottom solid skin detection, rectilinear & grid infill, Klipper/Marlin G-code emission, STL & 3MF input, `layercraft-cli`.
- **Phase 2 — Engine Extensions, MCP Server, 3D Viewport & Web App (Done):** `layercraft-viewport` (camera + headless PNG renderer), `layercraft-viewport-gpu` (`wgpu`), `layercraft-ui-egui` (Prepare/Preview UI + layer scrubber), `layercraft-mcp` (JSON-RPC 2.0 + PNG screenshots), `layercraft` desktop app, `layercraft-web` WASM target.
- **Phase 3 — Print-Quality & Profile Parity:**
  - **Phase 3A — Real-Printer G-code Essentials (Done):** Custom start/end/layer-change G-code macro templates, part-cooling fan (`M106`/`M107`) + min-layer-time cooling slowdown, volumetric flow capping (`mm^3/s`), seam placement (`aligned`, `back`, `nearest`, `random`), wipe-on-retract, Z-hop, first-layer temperatures & per-role speed overrides, Complexity Ladder Tier 02 (`02-print-quality`).
  - **Phase 3B–3D (Next):** Bridges & overhangs, Gyroid/Cubic infill + monotonic solid fill, Grid/Snug supports, multi-object plate layout, clean-room OrcaSlicer/Bambu JSON profile inheritance loader.
- **Phase 4 — Advanced Slicing Capabilities:** Variable-width walls (Medial-Axis / Arachne via `boostvoronoi`), tree supports, scarf seams, ironing, arc fitting (`G2`/`G3`), multi-material wipe tower, variable layer height & modifiers.
