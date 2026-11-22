# Architecture

| Layer | Crates | Role |
|---|---|---|
| L0 | `layercraft-geom` | Fixed-point `i64` nanometer 2D geometry (`Point2`, `Polygon`, `ExPolygon`), 3D `TriangleMesh`, `Aabb3`, exact coordinate conversions |
| L1 | `layercraft-clip`, `layercraft-mesh` | `layercraft-clip` is the sole 2D geometry kernel seam (booleans, offsetting, line clipping, simplification); `layercraft-mesh` performs mesh inspection, bed grounding/centering, and Z-plane slicing |
| L2 | `layercraft-config`, `layercraft-slice`, `layercraft-gcode` | Validated print configuration; deterministic slicing stages (slices, perimeters, top/bottom skins, solid/sparse infill, adhesion); volumetric extrusion math, Marlin/Klipper G-code emitters, and the G-code physical invariant evaluator |
| L3 | `layercraft-io` | Pure-Rust binary/ASCII STL and 3MF parsers/writers |
| L4 | `layercraft-engine` | Stateful slicing `Session` and JSON-schema typed `CommandRegistry` |
| Testkit | `layercraft-testkit` | Procedural watertight test meshes and hostile input generators (dev-dependencies only) |
| Apps | `layercraft-cli` | Headless CLI (`slice`, `commands`, `run`, `eval`, `compare`, `licences`) |
| Tooling | `xtask` | Workspace gates (`layers`, `licences`, `wasm`, `fast`, `oracle`, `ladder`, `ci`) |

How layer rules and the 2D kernel seam are enforced: [layers.md](layers.md).
