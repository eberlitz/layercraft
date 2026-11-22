# LayerCraft

**An open-source, clean-room 3D printer slicer built in pure Rust.**  
Dual-licensed under **MIT OR Apache-2.0**.

LayerCraft targets multi-platform slicing from a single pure-Rust engine: headless CLI (`layercraft-cli`), WebAssembly (`wasm32-unknown-unknown`), future `wgpu`/`egui` desktop UI, and typed JSON-schema commands (`layercraft-engine`) suitable for scripting and MCP automation.

## Clean-Room Policy

LayerCraft contains **zero GPL, LGPL, or AGPL code**. Contributors and AI agents never read, copy, translate, or decompile copyleft slicers (`OrcaSlicer`, `PrusaSlicer`, `BambuStudio`, `CuraEngine`, `Slic3r`) or C++ geometry kernels (`CGAL`, `OpenCASCADE`). Behavioral parity is validated exclusively through closed-form analytic math and out-of-process black-box CLI oracle comparisons (`tools/orca-oracle/`).

## Workspace Crates

See [docs/architecture.md](docs/architecture.md) and [docs/layers.md](docs/layers.md):

- **L0:** `layercraft-geom` — `i64` nanometer 2D geometry (`Point2`, `Polygon`, `ExPolygon`), 3D `TriangleMesh`
- **L1:** `layercraft-clip` (2D geometry kernel seam), `layercraft-mesh` (mesh inspection, grounding, Z-plane slicing)
- **L2:** `layercraft-config` (typed machine/filament/process config), `layercraft-slice` (perimeters, skins, infill), `layercraft-gcode` (volumetric extrusion, Klipper/Marlin emitters, physical G-code evaluator & raster IoU)
- **L3:** `layercraft-io` — pure-Rust STL (binary/ASCII) and 3MF readers
- **L4:** `layercraft-engine` — stateful `Session` & schema-typed command registry
- **Apps & Tooling:** `layercraft-cli`, `layercraft-testkit`, `xtask`

## Quick Start

```sh
cargo run -p layercraft-cli -- commands
cargo xtask fast
cargo xtask oracle
cargo xtask ladder
```
