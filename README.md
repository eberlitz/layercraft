<p align="center">
  <img src="docs/assets/layercraft-mark.svg" width="88" height="88" alt="LayerCraft" />
</p>

<h1 align="center">LayerCraft</h1>

<p align="center">
  <strong>An open-source, clean-room 3D printer slicer built in pure Rust — designed from the ground up for both humans and AI agents.</strong><br />
  Slice STL and 3MF meshes with fixed-point geometry, inspect toolpaths visually, verify G-code physics, and let AI agents drive every step through the built-in MCP server. Runs natively on desktop, in the browser via WebAssembly, and headlessly via CLI.
</p>

<p align="center">
  <a href="#built-for-ai-agents-mcp">AI Agents (MCP)</a> ·
  <a href="#what-it-does">Capabilities</a> ·
  <a href="#quick-start">Quick Start</a> ·
  <a href="#architecture--verification">Architecture</a> ·
  <a href="docs/oracle.md">Oracle (6/6)</a> ·
  <a href="docs/ladder.md">Complexity Ladder (47/47)</a> ·
  <a href="ROADMAP.md">Roadmap</a>
</p>

<p align="center">
  <a href="LICENSE-MIT"><img src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-14B8A6" alt="License: MIT OR Apache-2.0" /></a>
  <a href="docs/layers.md"><img src="https://img.shields.io/badge/Pure%20Rust-WASM--Clean-0D9488" alt="Pure Rust WASM-Clean" /></a>
  <a href="docs/mcp.md"><img src="https://img.shields.io/badge/MCP-Live%20Studio%20%2B%20PNG-14B8A6" alt="MCP: Live Studio + PNG" /></a>
  <a href="docs/oracle.md"><img src="https://img.shields.io/badge/Oracle-6%2F6%20Passing-10B981" alt="Oracle: 6/6 Passing" /></a>
  <a href="docs/ladder.md"><img src="https://img.shields.io/badge/Ladder-11%20Tiers%20%C2%B7%2047%2F47-10B981" alt="Ladder: 11 Tiers · 47/47" /></a>
</p>

<p align="center">
  <img src="docs/screenshots/preview-dark.png" alt="LayerCraft Dark Studio Preview — 90-layer sliced dome with 4x MSAA viewport, floating ViewCube, vertical layer scrubber, and print metrics HUD" />
</p>

<p align="center">
  <em>LayerCraft Desktop Studio (Dark Studio theme, 4× MSAA <code>wgpu</code> viewport): a 90-layer sliced dome inspected with floating ViewCube navigation, per-feature toolpath coloring, interactive vertical layer scrubbing, and live print metrics.</em>
</p>

| Dark Studio — Prepare | Light Drafting — Preview |
|---|---|
| ![Dark Studio Prepare](docs/screenshots/prepare-dark.png) | ![Light Drafting Preview](docs/screenshots/preview-light.png) |
| Build plate setup, hierarchical Plate Outliner, contextual Print / Filament / Supports / Output ribbon, and preset card. | Light Drafting theme inspecting 90-layer toolpaths cleanly against translucent HUD overlays. |

---

## Built for AI Agents (MCP)

A core goal of LayerCraft is making **3D printing workflows first-class for AI agents**. Rather than bolting automation onto a GUI later, every action in LayerCraft goes through a deterministic, schema-typed command engine (`layercraft-engine`) exposed directly over the [Model Context Protocol](https://modelcontextprotocol.io) (`layercraft-cli mcp`).

- **Live Studio connection (or headless on demand):** Launch `layercraft` Studio and run `layercraft-cli mcp` — tool calls attach over local IPC so you can watch the agent load parts, slice, toggle views, and capture `camera: "current"` screenshots directly in the open window (`--connect` to require Studio, `--standalone` for isolated headless CI).
- **Token-efficient discovery, patching & atomic batches:** Agents list concise command summaries (`list_commands`), fetch schemas on demand (`describe_command`), tweak individual fields (`config_patch`), and combine steps safely with rollback (`batch`).
- **Printability & DFM diagnostics:** `analyze_printability` reports overhang risk, detached floating islands, thin-wall collapses, bed adhesion metrics, and per-feature filament/time breakdowns with remediation hints.
- **Visual closed-loop & A/B inspection:** `screenshot` renders `plate`, `preview`, or `layer_top_down` PNGs, while `compare_slices` and `compare_screenshot` compare two configs quantitatively and visually with a 3-panel `A | B | Diff` composite.
- **Direct verified G-code export:** `save_gcode` writes `.gcode` straight to disk and runs physical invariant checks (non-finite values, out-of-bounds moves, layer continuity, volumetric flow, Z-hops, arcs) without flooding agent context.
- **Natural companion to CAD MCP servers:** Pair LayerCraft with an upstream CAD MCP server (such as [SolidCraft](https://github.com/tangency-labs/solidcraft)) so an agent can generate parametric geometry, load the exported STL/3MF live onto LayerCraft's plate, tune settings or modifier volumes, inspect screenshots, and produce verified G-code in one loop.

### Connect in Seconds

Build both desktop Studio and CLI (`layercraft-cli mcp`):

```sh
cargo build --release -p layercraft -p layercraft-cli
```

Register with **Claude Code**:

```sh
claude mcp add layercraft -- /absolute/path/to/layercraft/target/release/layercraft-cli mcp
```

Or add to any MCP client (`Claude Desktop`, `Cursor`, `Antigravity`):

```json
{
  "mcpServers": {
    "layercraft": {
      "command": "/absolute/path/to/layercraft/target/release/layercraft-cli",
      "args": ["mcp"]
    }
  }
}
```

### MCP Highlights (`layercraft-mcp`)

16 MCP tools + 3 MCP resources (`layercraft://skill`, `layercraft://plate/summary`, `layercraft://commands`), including:

| Tool | What the Agent Gets |
|---|---|
| `version` / `set_view` | Check `live_studio` vs `headless` connection state and steer Studio view mode, camera, layer, and color mode. |
| `list_commands` / `describe_command` | Compact schema-on-demand discovery across all **33** `layercraft-engine` commands. |
| `execute` / `batch` / `config_patch` | Run single commands, rollback-safe atomic batches, or partial `PrintConfig` patches. |
| `analyze_printability` | Overhang area, floating islands, thin-wall checks, bed contact ratio, and per-feature filament/time breakdown. |
| `screenshot` / `compare_screenshot` / `compare_slices` | Inline PNG inspection (including live `camera: "current"` + `sync_view`), 3-panel `A \| B \| Diff` renders, and 2D IoU slice diffs. |
| `save_gcode` | Write G-code directly to disk with built-in physical safety evaluation. |
| `skill_reference` | Built-in `SKILL.md` workflow and DFM guide (`skills/layercraft/SKILL.md`). |

Full MCP setup, Live Studio flags (`--connect`, `--standalone`, `--socket`), parameter schemas, and agent walkthrough: **[docs/mcp.md](docs/mcp.md)**.

---

## What It Does

Everything below runs through the same pure-Rust engine across the desktop app, browser app, headless CLI, and MCP server:

- **Fixed-Point `i64` Nanometer Slicing & Wall Generation:** Exact Z-plane triangle slicing, classic perimeter offsetting, and Voronoi medial-axis **Arachne** variable-width walls (`layercraft-clip` + `layercraft-slice`) that eliminate thin-wall gaps and collapses.
- **Surfaces, Infill & Motion Finish:** Top/bottom skin detection, monotonic top/bottom solid fill, Rectilinear, Grid, 3D Gyroid, and 3D Cubic infill patterns, top-surface low-flow monotonic ironing, outer-wall ramped scarf seams, and `G2`/`G3` circular arc fitting.
- **Bridges, Overhangs & Supports:** Anchored bridge orientation and cooling/flow tuning, overhang wall detection and slowdown, **Grid** and **Snug** automatic supports, and model-avoiding branching **Organic Tree** supports with hollow trunks, dense interface layers, and build-plate-only routing.
- **Adaptive Layers & 3D Modifier Volumes:** Curvature-driven adaptive layer heights (`0.08–0.28 mm` and custom schedules), explicit Z height-range bands, physical shell thicknesses, and primitive (`box`/`cylinder`) or arbitrary STL/3MF `Modifier`, `SupportBlocker`, and `SupportEnforcer` volumes.
- **Printer-Ready G-code & JSON Profiles:** `Klipper`, `Marlin`, and Prusa `MarlinBuddy` emitters with volumetric flow capping, pressure/linear advance, per-role acceleration, chamber heating, `M73` progress, and safe `{if}/{elsif}/{else}` + arithmetic macro templates. Includes built-in starter presets (`LayerCraft Generic Klipper 250mm`, `LayerCraft Generic Marlin 220mm`, `LayerCraft CORE One+ HF 0.2mm`, `LayerCraft CORE One+ HF 0.4mm`) and Orca/Bambu-compatible JSON profile inheritance.
- **Studio Desktop App, Browser WASM, CLI & MCP:** Native `eframe` + `4× MSAA` `wgpu` studio with Dark Studio / Light Drafting / System themes, interactive ViewCube & layer scrubber, browser WASM build (`apps/layercraft-web`), headless PNG & G-code CLI, and JSON-RPC 2.0 MCP server.

See [docs/architecture.md](docs/architecture.md) for crate structure and [ROADMAP.md](ROADMAP.md) for completed milestones and upcoming phases (`.3mf` project files, parallel/sweep-line slicing, mesh repair).

---

## Quick Start

### Desktop App

```sh
cargo run --release -p layercraft                                   # desktop studio GUI
cargo run --release -p layercraft -- --sample dome --preview        # launch with sliced sample dome
cargo run --release -p layercraft -- --sample dome --theme light    # Light Drafting theme
cargo run --release -p layercraft -- --sample dome --preview --screenshot preview.png
```

### Headless CLI

```sh
cargo run -p layercraft-cli -- slice part.stl -o part.gcode
cargo run -p layercraft-cli -- profiles
cargo run -p layercraft-cli -- render part.stl -o preview.png --mode preview
cargo run -p layercraft-cli -- eval part.gcode
cargo run -p layercraft-cli -- compare a.gcode b.gcode
cargo run -p layercraft-cli -- commands
cargo run -p layercraft-cli -- mcp
```

### Browser (WASM)

All core crates (`L0`–`L4`), software/GPU viewports, and the shared `egui` studio compile cleanly to `wasm32-unknown-unknown`:

```sh
cd apps/layercraft-web && trunk serve
```

### Verification Gates

```sh
cargo xtask fast   # cargo fmt --check, clippy -D warnings, layers, licences, wasm, unit tests
cargo xtask ci     # fast + analytic/OrcaSlicer oracle + 11-tier complexity ladder
```

---

## Automation Scripts

Any sequence of UI or MCP operations can be saved as a JSON command array and executed headlessly with `layercraft-cli run`:

```json
[
  { "command": "plate.add_sample_shape", "params": { "shape": "bridge_arch" } },
  {
    "command": "profile.apply",
    "params": {
      "machine": "LayerCraft CORE One+ HF 0.4mm",
      "filament": "LayerCraft PLA (0.4mm HF Nozzle)",
      "process": "LayerCraft 0.20mm Balanced (0.4mm HF)",
      "reset_to_defaults": true
    }
  },
  { "command": "slice.run", "params": {} },
  { "command": "gcode.export", "params": {} }
]
```

```sh
cargo run -p layercraft-cli -- run script.json
```

---

## Architecture & Verification

LayerCraft enforces strict architectural seams and automated quality gates via `cargo xtask`:

- **Strict `L0`–`L5` Dependency Layering ([docs/layers.md](docs/layers.md)):** Lower layers never depend on higher layers; UI/GPU crates never appear below `L5`; `layercraft-clip` is the sole 2D geometry kernel seam (`cargo xtask layers`).
- **Permissive-Only Dependencies:** Every shipped crate is audited against an allowlist (`MIT`, `Apache-2.0`, `BSD-*`, `BSL-1.0`, `ISC`, `Zlib`, etc.) and recorded in [THIRD-PARTY-LICENSES.txt](THIRD-PARTY-LICENSES.txt) (`cargo xtask licences --check`).
- **No-Crash Policy:** `unsafe_code = "forbid"` and workspace Clippy lints deny `unwrap`, `expect`, `panic`, and `indexing_slicing` in non-test code.
- **Analytic & Black-Box Oracle ([docs/oracle.md](docs/oracle.md)):** **6 / 6** reference cases validated against closed-form analytic geometry (`tools/oracle-analytic/`) and out-of-process `OrcaSlicer 2.5.2` CLI statistics (`tools/orca-oracle/`).
- **Complexity Ladder ([docs/ladder.md](docs/ladder.md)):** **11 tiers / 47 regression cases** passing from basic calibration primitives (`01-mesh-slice`) through dimensional compensations, combing, and embedded thumbnails (`11-real-printer-polish`).

---

## Clean-Room Policy & Licence

LayerCraft is dual-licensed under **[MIT](LICENSE-MIT) OR [Apache-2.0](LICENSE-APACHE)** and contains **zero GPL, LGPL, or AGPL code**. Contributors and AI agents follow strict [clean-room rules](AGENTS.md) and never read, copy, translate, link, or decompile copyleft slicers (`OrcaSlicer`, `PrusaSlicer`, `BambuStudio`, `CuraEngine`, `Slic3r`) or C++ geometry kernels (`CGAL`, `OpenCASCADE`).

LayerCraft is an independent project and is not affiliated with, sponsored by, or endorsed by any third-party slicer or printer vendor; see [DISCLAIMER.md](DISCLAIMER.md) and [ATTRIBUTION.md](ATTRIBUTION.md).
