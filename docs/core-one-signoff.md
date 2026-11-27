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

### 1. Baseline Cube & `.bgcode` / PrusaLink Handshake (PLA HF)
- **Geometry:** 20 mm calibration cube (or Studio -> Sample -> Cube 20 mm)
- **Presets:** `LayerCraft CORE One+ HF 0.4mm` + `LayerCraft PLA (0.4mm HF Nozzle)` + `LayerCraft 0.20mm Balanced (0.4mm HF)`
- **CLI slice & upload:**
  ```bash
  ./target/release/layercraft-cli slice cube20.stl -o cube20.bgcode \
    --machine "LayerCraft CORE One+ HF 0.4mm" \
    --filament "LayerCraft PLA (0.4mm HF Nozzle)" \
    --process "LayerCraft 0.20mm Balanced (0.4mm HF)" \
    --binary
  ./target/release/layercraft-cli eval cube20.gcode   # if exporting plain .gcode
  ./target/release/layercraft-cli printer upload cube20.bgcode \
    --printer "CORE One+" \
    --binary
  ```
- **Inspect on hardware:**
  - [ ] LCD displays QOI thumbnail preview, `Print Time`, and `Material` with zero firmware incompatibility warnings on `M862.1` / `M862.3` / `M862.5` / `M862.6` ("Input shaper")
  - [ ] Chamber exhaust / fan (`M106 P3 S127` for PLA) starts cleanly; `G29 G` heat soak and focused `G29 P1` mesh bed leveling finish cleanly after anti-ooze retraction
  - [ ] Front-right purge strip (`X249 -> X199, Y=-2.5`) clears cleanly with zero nozzle collision on move to skirt/part
  - [ ] XY dimensions within ±0.15 mm; bed finishes with clean park & PA reset (`M572 S0`, `M84 X Y E`) in `end_gcode`

### 2. Bridges & Progressive Overhangs (PETG HF)
- **Geometry:** `BridgeArch` sample (or calibration overhang/bridge tower)
- **Presets:** `LayerCraft CORE One+ HF 0.4mm` + `LayerCraft PETG (0.4mm HF Nozzle)` + `LayerCraft 0.20mm Balanced (0.4mm HF)`
- **Inspect on hardware:**
  - [ ] 18 mm clear span under bridge deck shows minimal sag and fused bottom threads
  - [ ] Overhang speed ramp & elevated `OverhangWall` cooling prevent curling on 45°–60° slopes
  - [ ] PETG `z_hop` (0.4 mm) avoids boogers/scarring across travel moves

### 3. Tree Supports & Interface Breakaway
- **Geometry:** `CBracket` or overhanging functional part with `process.support.enabled = true`, `style = "tree"`
- **Presets:** `LayerCraft CORE One+ HF 0.4mm` + `LayerCraft PLA (0.4mm HF Nozzle)` + `LayerCraft 0.20mm Balanced (0.4mm HF)`
- **Inspect on hardware:**
  - [ ] Hollow circular tree trunks adhere cleanly to build plate and clear lower arm geometry (`on_build_plate_only`)
  - [ ] Dense `Support interface` cap snaps off cleanly with smooth supported underside (no fused welts)

### 4. Thin-Wall Arachne Modulation
- **Geometry:** `ThinRib` / tapered wedge with sub-nozzle wall widths (`process.wall_generator = "arachne"`)
- **Presets:** `LayerCraft CORE One+ HF 0.4mm` + `LayerCraft PLA (0.4mm HF Nozzle)` + `LayerCraft 0.20mm Balanced (0.4mm HF)`
- **Inspect on hardware:**
  - [ ] Narrow tapered tips print as continuous variable-width medial beads without gaps or dropped layers
  - [ ] Intermediate transition from 2 perimeters -> 1 medial bead is smooth with no blobs

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
| 1 | Cube 20 mm (`PLA HF`, `0.20 Balanced`) | `__m __s` | `__m __s` | `___%` | |
| 2 | Bridge / Overhang (`PETG HF`, `0.20 Balanced`) | `__m __s` | `__m __s` | `___%` | |
| 3 | C-Bracket (`PLA HF`, Tree Supports) | `__m __s` | `__m __s` | `___%` | |
| 4 | Thin Rib / Wedge (`PLA HF`, Arachne) | `__m __s` | `__m __s` | `___%` | |
| 5 | Cylinder (`PLA HF`, Scarf + Ironing + Arcs) | `__m __s` | `__m __s` | `___%` | |
| 6 | Multi-Part / ASA / TPU Check | `__m __s` | `__m __s` | `___%` | |
