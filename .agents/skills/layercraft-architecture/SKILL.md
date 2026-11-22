---
name: layercraft-architecture
description: "Use when designing or editing LayerCraft crates, fixed-point geometry types, the layercraft-clip seam, or commands in layercraft-engine."
---

# LayerCraft Architecture & Conventions

## Layer Hierarchy
- **L0 (`layercraft-geom`):** Fixed-point `i64` nanometer 2D geometry (`Point2`, `Polygon`, `ExPolygon`), `TriangleMesh`, `Aabb3`, coordinate conversion (`NM_PER_MM = 1_000_000`). No workspace dependencies.
- **L1 (`layercraft-clip`, `layercraft-mesh`):**
  - `layercraft-clip` is the **sole 2D kernel seam**. All polygon booleans, offsetting, line-segment clipping, and simplification pass through it using `layercraft-geom` types. Only `layercraft-clip` may depend on `clipper2_rust` / `i_overlay` / `boostvoronoi`.
  - `layercraft-mesh` handles mesh inspection, bed grounding/centering, sample primitives, and exact half-open edge Z-plane slicing into normalized `ExPolygon` slices.
- **L2 (`layercraft-config`, `layercraft-slice`, `layercraft-gcode`):**
  - `layercraft-config`: Validated machine, filament, and process settings.
  - `layercraft-slice`: Deterministic slicing pipeline (`Slice -> Perimeters -> Skins -> Infill -> Adhesion`).
  - `layercraft-gcode`: Volumetric stadium-bead extrusion math, Marlin/Klipper emitters, and the G-code Physical Invariant Evaluator (`evaluate_gcode` + raster IoU comparison).
- **L3 (`layercraft-io`):** Pure-Rust STL (binary/ASCII) and 3MF readers/writers, WASM-safe core with native file helpers gated via `#[cfg(not(target_arch = "wasm32"))]`.
- **L4 (`layercraft-engine`):** Stateful `Session` and typed `CommandRegistry` with `schemars` JSON schemas.
- **L5 (`layercraft-viewport`, `layercraft-viewport-gpu`, `layercraft-ui-egui`, `layercraft-mcp`):** Camera & CPU PNG renderer, `wgpu` + `egui-wgpu` 3D renderer, shared desktop/browser `eframe` UI, and stdio JSON-RPC 2.0 MCP server.

## Coding Invariants
- Never use `unwrap()`, `expect()`, `panic!()`, `unsafe`, or unchecked slice indexing `a[i]` in non-test code (`get(...)` / `Result` everywhere).
- Keep all `L0`–`L5` UI/viewport crates compiling cleanly on `wasm32-unknown-unknown` (`cargo xtask wasm`).
- Enforce layer and seam rules with `cargo xtask layers`.
