# Workspace Layers & Kernel Seam

Every crate in `crates/*`, `apps/*`, and `xtask` is classified in `xtask/src/layers.rs`.

## Rules

1. **Strict downward dependencies (`L0`–`L5`):** A layered crate may only depend on lower-numbered layers or an allowed earlier sibling in `INTRA_LAYER_ORDER` (`clip -> mesh`; `config -> slice -> gcode`; `viewport -> viewport-gpu -> ui-egui`; `viewport -> mcp`).
2. **Testkit isolation:** `layercraft-testkit` sits above `L5` and may only appear under `[dev-dependencies]`.
3. **No UI/GPU crates below L5:** `egui`, `eframe`, `wgpu`, `winit`, and `egui-wgpu` are forbidden anywhere inside `L0`–`L4` (checked transitively via `cargo metadata`).
4. **One 2D geometry kernel seam:** Only `layercraft-clip` may depend on `clipper2_rust`, `i_overlay`, or `boostvoronoi`. All upper crates pass `layercraft-geom` types across the `layercraft-clip` boundary.
5. **WASM readiness:** Every `L0`–`L4` crate plus `layercraft-viewport`, `layercraft-viewport-gpu`, `layercraft-ui-egui`, and `layercraft-web` must compile cleanly on `wasm32-unknown-unknown` (`cargo xtask wasm`).

Verify locally with `cargo xtask layers`.
