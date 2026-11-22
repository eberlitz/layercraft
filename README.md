# LayerCraft

**An open-source, clean-room 3D printer slicer built in pure Rust.**  
Dual-licensed under **MIT OR Apache-2.0**.

LayerCraft runs the same pure-Rust engine across desktop GUI (`layercraft`), browser WebAssembly (`layercraft-web`), headless CLI (`layercraft-cli`), and a Model Context Protocol server (`layercraft-cli mcp`) with visual screenshot inspection.

## Clean-Room Policy

LayerCraft contains **zero GPL, LGPL, or AGPL code**. Contributors and AI agents never read, copy, translate, or decompile copyleft slicers (`OrcaSlicer`, `PrusaSlicer`, `BambuStudio`, `CuraEngine`, `Slic3r`) or C++ geometry kernels (`CGAL`, `OpenCASCADE`). Behavioral parity is validated exclusively through closed-form analytic math and out-of-process black-box CLI oracle comparisons (`tools/orca-oracle/`).

## Workspace Crates

See [docs/architecture.md](docs/architecture.md) and [docs/layers.md](docs/layers.md):

- **L0:** `layercraft-geom` — `i64` nanometer 2D geometry (`Point2`, `Polygon`, `ExPolygon`), 3D `TriangleMesh`
- **L1:** `layercraft-clip` (2D geometry kernel seam), `layercraft-mesh` (mesh inspection, grounding, Z-plane slicing, built-in samples)
- **L2:** `layercraft-config` (typed machine/filament/process config), `layercraft-slice` (perimeters, skins, infill), `layercraft-gcode` (volumetric extrusion, Klipper/Marlin emitters, physical G-code evaluator & raster IoU)
- **L3:** `layercraft-io` — pure-Rust STL (binary/ASCII) and 3MF readers
- **L4:** `layercraft-engine` — stateful `Session` & schema-typed command registry
- **L5:** `layercraft-viewport` (camera + headless CPU PNG renderer), `layercraft-viewport-gpu` (`wgpu` + `egui-wgpu`), `layercraft-ui-egui` (shared GUI), `layercraft-mcp` (stdio MCP server)
- **Apps:** `layercraft` (desktop), `layercraft-cli` (CLI + MCP + headless PNG render), `layercraft-web` (WASM)

## Quick Start

```sh
cargo run --release -p layercraft                    # desktop GUI
cargo run -p layercraft-cli -- commands              # typed JSON command registry
cargo run -p layercraft-cli -- mcp                   # stdio MCP server
cargo xtask fast                                     # fmt, clippy, layers, licences, wasm, tests
cargo xtask ci                                       # fast + oracle + complexity ladder
```
