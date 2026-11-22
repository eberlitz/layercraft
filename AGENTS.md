# LayerCraft: Instructions for Agents

LayerCraft is a clean-room, pure-Rust 3D printer slicer (`MIT OR Apache-2.0`). This file is the router for hard rules and subsystem skills.

## Hard Rules (and what enforces each)

- **Clean-room independence:** Never read, clone, inspect, copy, translate, decompile, or link GPL/LGPL/AGPL slicer or geometry code (OrcaSlicer, PrusaSlicer, BambuStudio, SuperSlicer, CuraEngine, Slic3r, CGAL, OpenCASCADE). Never copy icons, strings, or profile trees from another slicer. Read `.agents/skills/layercraft-ground-rules/SKILL.md`.
- **Permissive dependencies only:** Every shipped dependency must be permissive (`MIT`, `Apache-2.0`, `BSD-*`, `BSL-1.0`, `ISC`, `Zlib`, etc.). Enforced by `cargo xtask licences --check`.
- **Never crash:** No `unwrap`, `expect`, `panic`, `indexing_slicing`, or `unsafe` in non-test code; errors return `Result`; hostile inputs never panic (`hostile_params_never_panic`). Enforced by `clippy -D warnings` (`clippy.toml` allows test-only usage) and `[workspace.lints]`.
- **Strict layering & single 2D kernel seam:** Lower layers never depend on higher layers; UI/GPU crates (`egui`, `wgpu`, `winit`, `eframe`) never appear below L5; only `layercraft-clip` may depend on the 2D geometry kernel crate (`clipper2_rust`). Enforced by `cargo xtask layers`; see `docs/layers.md`.
- **Pure Rust & WASM-clean core:** Library crates (`L0`–`L4`) must build cleanly for `wasm32-unknown-unknown`. Enforced by `cargo xtask wasm`.
- **Everything is a command:** All slicing and session actions go through typed commands with JSON schemas in `layercraft-engine`; programmatic calls never open modal dialogs.
- **Oracle & complexity ladder verification:** Behavior is validated against closed-form analytic ground truth (`tools/oracle-analytic/`) and out-of-process black-box CLI output (`tools/orca-oracle/`). Enforced by `cargo xtask oracle` and `cargo xtask ladder`.
- **Pre-commit gate:** Run `cargo xtask fast` before each commit; run `cargo xtask ci` for the full oracle + ladder gate.

## Skills

| Skill | When to read |
|---|---|
| `.agents/skills/layercraft-ground-rules/SKILL.md` | Before adding dependencies, inspecting external references, or creating test fixtures |
| `.agents/skills/layercraft-architecture/SKILL.md` | Before adding crates, editing `layercraft-clip`, or adding commands in `layercraft-engine` |
| `.agents/skills/layercraft-oracle/SKILL.md` | Before running, debugging, or adding analytic/black-box oracle or ladder cases |
