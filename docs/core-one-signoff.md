# Prusa CORE One+ (2nd Gen) Physical Print Sign-Off Checklist

Before cutting a `v1.0` hardware release tag, run these 6 physical validation prints on the **Prusa CORE One+ (2nd Gen, High-Flow 0.4 mm nozzle)** and record observed results below. All software slicing, kinematics, safety evaluation, and `.bgcode` CRC32 roundtrips for this suite are automated in `core_one_physical_signoff_suite` (`crates/engine/src/lib.rs`).

## Quick Preflight (Automated Software Gate)

```bash
export PATH="$HOME/.cargo/bin:/home/linuxbrew/.linuxbrew/bin:$PATH"
cargo test -p layercraft-engine core_one_physical_signoff_suite
cargo xtask fast
cargo build --release -p layercraft-cli
```

Confirm printer LAN connectivity (`PrusaLink` or `Prusa Connect Local`) before sending prints:

```bash
./target/release/layercraft-cli printer test \
  --url http://<printer-ip> \
  --protocol prusa-link \
  --username maker \
  --password <prusalink-password>
```

*(Or `--printer <saved-profile-name>` once saved in LayerCraft Studio -> Machine -> Direct Printer Upload.)*

---

## Physical Print Validation Suite

### 1. Baseline Cube & `.bgcode` / PrusaLink Handshake (PLA HF / PETG HF)
- **Geometry:** 20 mm calibration cube (or Studio -> Sample -> Cube 20 mm)
- **Presets:** `LayerCraft CORE One+ HF 0.4mm` + `LayerCraft PETG (0.4mm HF Nozzle)` (verified first print; also test `LayerCraft PLA (0.4mm HF Nozzle)` when loaded) + `LayerCraft 0.20mm Balanced (0.4mm HF)`
- **CLI slice & upload:**
  ```bash
  ./target/release/layercraft-cli slice cube20.stl -o cube20.bgcode \
    --machine "LayerCraft CORE One+ HF 0.4mm" \
    --filament "LayerCraft PETG (0.4mm HF Nozzle)" \
    --process "LayerCraft 0.20mm Balanced (0.4mm HF)" \
    --binary
  ./target/release/layercraft-cli eval cube20.gcode   # if exporting plain .gcode
  ./target/release/layercraft-cli printer upload cube20.bgcode \
    --printer "CORE One+" \
    --binary
  ```
- **Inspect on hardware:**
  - [x] LCD displays QOI thumbnail preview, `Print Time`, and `Material` (`PETG 3g`) with zero firmware incompatibility warnings on `M862.1` / `M862.3` / `M862.5` / `M862.6` ("Input shaper")
  - [x] `G29 G` heat soak, focused `G29 P1` mesh bed leveling, anti-ooze retraction, and `M141 S35` (`37°C` chamber) finished cleanly on PETG HF (PLA `M106 P3 S127` still pending a PLA load)
  - [x] Front-right purge strip (`X249 -> X199, Y=-2.5`) clears cleanly with zero nozzle collision and zero ooze tail on move to Layer 0
  - [x] Measured `20.01 × 20.02 × 20.05 mm` (within ±0.15 mm); clean park & PA reset (`M572 S0`, `M84 X Y E`) in `end_gcode` (`end_gcode` updated post-run to park before `Z100` drop to prevent final top-corner string)

### 2. Bridges & Progressive Overhangs (PETG HF)
- **Geometry:** `BridgeArch` sample (or calibration overhang/bridge tower)
- **Presets:** `LayerCraft CORE One+ HF 0.4mm` + `LayerCraft PETG (0.4mm HF Nozzle)` + `LayerCraft 0.20mm Balanced (0.4mm HF)`
- **Inspect on hardware:**
  - [x] 18 mm clear span under bridge deck shows minimal sag and clean pier-to-pier threads (total height `11.03 mm` vs `11.00 mm` nominal; mid-span deck thickness `3.57 mm` vs `3.00 mm` nominal; zero collapsed loops or archway stringing)
  - [ ] Overhang speed ramp & elevated `OverhangWall` cooling prevent curling on 45°–60° slopes
  - [x] PETG `z_hop` (0.4 mm) avoids boogers/scarring across travel moves (final-layer wipe-retract added post-run before `end_gcode` to remove the finishing corner wisp)

### 3. Tree Supports & Interface Breakaway (`PETG HF`)
- **Geometry:** `CBracket` (`24 × 12 × 14 mm`) with `process.support.enabled = true`, `style = "tree"`, `on_build_plate_only = true`
- **Presets:** `LayerCraft CORE One+ HF 0.4mm` + `LayerCraft PETG (0.4mm HF Nozzle)` (verified third print; also test `LayerCraft PLA (0.4mm HF Nozzle)` when loaded) + `LayerCraft 0.20mm Balanced (0.4mm HF)`
- **Inspect on hardware:**
  - [x] Hollow circular tree trunks adhere cleanly to build plate and clear lower arm geometry (`on_build_plate_only`) — 3 merged trunk clusters adhered flat on PEI with zero fusion to the lower arm
  - [x] Dense `Support interface` cap snaps off cleanly with smooth supported underside (no fused welts) — continuous 2-layer cross-hatched interface canopy + corner seeding (`6705742`) produced crisp 90° corners and clean breakaway on PETG HF with only a tiny inner-corner flake

### 4. Thin-Wall Arachne Modulation (`PETG HF`)
- **Geometry:** `ThinRib` / `TaperedWedge` (`36 × 4.0→0.25 × 8.0 mm`) with sub-nozzle wall widths (`process.wall_generator = "arachne"`)
- **Presets:** `LayerCraft CORE One+ HF 0.4mm` + `LayerCraft PETG (0.4mm HF Nozzle)` (verified fourth print; also test `LayerCraft PLA (0.4mm HF Nozzle)` when loaded) + `LayerCraft 0.20mm Balanced (0.4mm HF)`
- **Inspect on hardware:**
  - [x] Narrow tapered tips print as continuous variable-width medial beads without gaps or dropped layers — measured `35.78 mm` length (vs `36.00 mm` nominal) and `0.49 mm` tip width on a `0.40 mm` nozzle with zero dropped layers or tip lifting
  - [x] Intermediate transition from 2 perimeters -> 1 medial bead is smooth with no blobs — gap-free, blob-free solid transitions across `4→3→2→1` walls after Arachne transition/ramp fix (`aae97eb`) (minor cosmetic step noted at the outer `2→1` pinch where the seam started)

### 5. Surface & Motion Finish (Scarf Seam + Ironing + G2/G3 Arcs + PA)
- **Geometry:** Cylinder (`SampleShape::Cylinder`) with `scarf_seam.enabled = true`, `ironing.mode = "topmost_surface"`, `arc_fitting = true`
- **Presets:** `LayerCraft CORE One+ HF 0.4mm` + `LayerCraft PLA (0.4mm HF Nozzle)` + `LayerCraft 0.20mm Balanced (0.4mm HF)`
- **Inspect on hardware:**
  - [ ] Outer vertical seam step is visibly reduced by the Z/E-ramped scarf overlap
  - [ ] Cylindrical walls move smoothly via `G2`/`G3` arcs with crisp corners/PA (`M572`) and no stuttering
  - [ ] Top circular cap shows uniform glossy low-flow ironed finish with clean perimeter transition

### 6. Engineering Filaments & Multi-Object Cancel (`ASA`, `TPU 95A`, `M486`)
- **ASA Test (`LayerCraft ASA (0.4mm HF Nozzle)`):**
  - [ ] Chamber setpoint `M141 S55` engages cleanly; minimal fan (`0–30%`) avoids warping/delamination on a medium part
- **TPU 95A Test (`LayerCraft TPU 95A (0.4mm HF Nozzle)`):**
  - [ ] `5.0 mm³/s` max volumetric flow prevents extruder skipping/buckling on HF hotend
- **Multi-Object Cancel Test:** Two parts on plate (`machine.label_objects = true`)
  - [ ] Cancelling one object mid-print on the Prusa CORE One+ LCD (`M486` object labels) leaves sibling part finishing cleanly

---

## Estimated vs. Actual Print Time Calibration Log

| Print # | Part & Preset | Estimated Time (Studio) | Actual Time (CORE One+ LCD) | Error (%) | Notes / Adjustments |
|---|---|---|---|---|---|
| 1 | Cube 20 mm (`PETG HF`, `0.20 Balanced`) | `14m 25s` | `22m 53s` | `+59% (incl. heat/soak/MBL)` | `20.01 × 20.02 × 20.05 mm` (within ±0.15 mm); LCD `PETG 3g`, chamber `37°C`; clean gyroid/corners; `end_gcode` updated to park before `Z100` drop |
| 2 | BridgeArch (`PETG HF`, `0.20 Balanced`) | `8m 9s` | `16m 35s` | `+8m 26s (startup)` | `11.03 mm` height (nom `11.00`), `3.57 mm` mid-span deck (nom `3.00`); `PETG 1g`, chamber `34°C`; startup overhead matches Print #1 (`+8m 28s`) within 2 s |
| 3 | C-Bracket (`PETG HF`, Tree Supports) | `12m 29s` | `20m 45s` | `+8m 16s (startup)` | `PETG 3g`, chamber `35°C`; continuous interface canopy & corner seeding `6705742`; clean breakaway & crisp 90° corners |
| 4 | TaperedWedge (`PETG HF`, Arachne) | `5m 34s` | `13m 37s` | `+8m 3s (startup)` | `35.78 mm` len (nom `36.00`), `0.49 mm` tip; `PETG 1g`, chamber `37°C`; gap-free `4→3→2→1` transitions after `aae97eb` |
| 5 | Cylinder (`PLA HF`, Scarf + Ironing + Arcs) | `__m __s` | `__m __s` | `___%` | |
| 6 | Multi-Part / ASA / TPU Check | `__m __s` | `__m __s` | `___%` | |
