# LayerCraft Roadmap

- **Phase 0 — Governance, Clean-Room Harness & Oracle (Done):** Workspace crates, `cargo xtask` checks (`layers`, `licences`, `wasm`, `fast`, `oracle`, `ladder`), `AGENTS.md`, skills, G-code physical invariant evaluator, analytic + black-box `OrcaSlicer 2.5.2` oracle harness.
- **Phase 1 — Tier 01 Slice MVP (Current):** Exact Z-plane mesh slicing, classic perimeter offsetting, top/bottom solid skin detection, rectilinear & grid infill, Klipper/Marlin G-code emission, STL & 3MF input, `layercraft-cli`.
- **Phase 2 — Engine Extensions, MCP Server & Visual Viewport:** `layercraft-mcp` server, `wgpu` + `egui` desktop plate & toolpath preview (`L5`), `layercraft-web` WASM app.
- **Phase 3 — Print-Quality & Profile Parity:** OrcaSlicer/Bambu JSON profile inheritance importer, bridges & overhangs, Gyroid/Cubic/Lightning infill, seam placement & avoid-crossing-perimeters, cooling & volumetric flow capping, Grid/Snug supports.
- **Phase 4 — Advanced Slicing Capabilities:** Variable-width walls (Medial-Axis / Arachne via `boostvoronoi`), tree supports, scarf seams, ironing, arc fitting (`G2`/`G3`), multi-material wipe tower, variable layer height & modifiers.
