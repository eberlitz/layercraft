---
name: layercraft-ground-rules
description: "Use before adding any dependency, algorithm reference, test oracle, or profile asset to LayerCraft. Enforces strict clean-room independence from GPL/LGPL/AGPL slicers and permissive licensing."
---

# Clean-Room Independence & Permissive Licensing

## Never Read or Link Copyleft Slicer Code
- **Strictly forbidden sources (do not read, clone, decompile, copy, or link):** OrcaSlicer, PrusaSlicer, BambuStudio, SuperSlicer, CuraEngine, Slic3r, CGAL, OpenCASCADE, or any other `GPL`, `LGPL`, or `AGPL` codebase.
- Never use binary reverse-engineering / decompilation tools against another slicer's executable to recover implementation code.

## Approved Sources
- Public file-format specifications (STL binary/ASCII, 3MF Core Spec, OBJ, standard Marlin/Klipper/RepRap G-code).
- Published academic and algorithmic literature (e.g., Vatti polygon clipping, fixed-point segment Voronoi medial axis / variable-width wall literature, tree-support path planning papers).
- Out-of-process black-box CLI outputs (`tools/orca-oracle/`) running an installed `OrcaSlicer` binary on synthetic test meshes to record physical statistics, bounding boxes, and G-code safety/coverage invariants.
- Permissive pure-Rust crates (`MIT`, `Apache-2.0`, `BSD-2-Clause`, `BSD-3-Clause`, `BSL-1.0`, `ISC`, `Zlib`, `CC0-1.0`, `0BSD`, `Unlicense`).

## Mechanical Enforcement
- Run `cargo xtask licences --check` after any `Cargo.toml` or `Cargo.lock` change.
- Update `THIRD-PARTY-LICENSES.txt` with `cargo xtask licences` when approved dependencies change.
- Document project inspirations and external acknowledgments in `ATTRIBUTION.md`.
