//! `cargo xtask ladder`: runs tiered complexity ladder cases and regenerates `docs/ladder.md`.

use std::path::Path;
use std::time::Instant;

use layercraft_config::{PrintConfig, ProfileApplyResult, ProfileLibrary};
use layercraft_gcode::{emit_gcode, evaluate_gcode};
use layercraft_mesh::{ObjectTransform, auto_arrange_footprints, prepare_transformed_object};
use layercraft_slice::{slice_meshes, slice_single_mesh};
use layercraft_testkit::{ShapeSpec, build_shape};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct LadderObjectSpec {
    shape: ShapeSpec,
    #[serde(default)]
    transform: Option<ObjectTransform>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize)]
struct LadderProfileSpec {
    #[serde(default)]
    dir: Option<String>,
    #[serde(default)]
    files: Vec<String>,
    #[serde(default)]
    machine: Option<String>,
    #[serde(default)]
    filament: Option<String>,
    #[serde(default)]
    process: Option<String>,
    #[serde(default = "default_true")]
    use_builtins: bool,
}

#[derive(Debug, Deserialize)]
struct LadderRecipe {
    name: String,
    description: String,
    #[serde(default)]
    shape: Option<ShapeSpec>,
    #[serde(default)]
    objects: Vec<LadderObjectSpec>,
    #[serde(default)]
    auto_arrange_spacing_mm: Option<f64>,
    #[serde(default)]
    profile_bundle: Option<LadderProfileSpec>,
    #[serde(default)]
    config: PrintConfig,
}

#[derive(Debug, Deserialize)]
struct LadderExpected {
    layer_count: usize,
    max_z_mm: f64,
    #[serde(default)]
    require_outer_wall: bool,
    #[serde(default)]
    require_top_surface: bool,
    #[serde(default)]
    require_bottom_surface: bool,
    #[serde(default)]
    require_sparse_infill: bool,
    #[serde(default)]
    forbid_center_extrusion_mm: Option<[f64; 2]>,
    #[serde(default)]
    max_volumetric_flow_mm3_s: Option<f64>,
    #[serde(default)]
    require_first_layer_fan_off: bool,
    #[serde(default)]
    min_fan_pwm: Option<u8>,
    #[serde(default)]
    require_z_hops: bool,
    #[serde(default)]
    require_wipes: bool,
    #[serde(default)]
    require_accel_commands: bool,
    #[serde(default)]
    require_pressure_advance: bool,
    #[serde(default)]
    require_bridge: bool,
    #[serde(default)]
    require_overhang_wall: bool,
    #[serde(default)]
    require_support: bool,
    #[serde(default)]
    require_support_interface: bool,
    #[serde(default)]
    require_ironing: bool,
    #[serde(default)]
    require_arcs: bool,
    #[serde(default)]
    require_scarf_seams: bool,
    #[serde(default)]
    forbid_support: bool,
    #[serde(default)]
    min_bbox_width_mm: Option<f64>,
    #[serde(default)]
    min_volume_mm3: Option<f64>,
    #[serde(default)]
    max_volume_mm3: Option<f64>,
    #[serde(default)]
    min_wall_width_variation_ratio: Option<f64>,
    #[serde(default)]
    require_gcode_contains: Vec<String>,
    #[serde(default)]
    forbid_gcode_contains: Vec<String>,
    #[serde(default)]
    expected_machine_chain: Vec<String>,
    #[serde(default)]
    expected_filament_chain: Vec<String>,
    #[serde(default)]
    expected_process_chain: Vec<String>,
    #[serde(default)]
    expected_unmapped_keys: Vec<String>,
    #[serde(default)]
    max_profile_warnings: Option<usize>,
}

pub fn run(root: &Path) -> Result<(), String> {
    let ladder_dir = root.join("tests").join("ladder");
    let mut tiers: Vec<_> = std::fs::read_dir(&ladder_dir)
        .map_err(|e| format!("{}: {e}", ladder_dir.display()))?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    tiers.sort();

    if tiers.is_empty() {
        return Err("no ladder tiers found under tests/ladder/".to_owned());
    }

    let mut tier_summaries = Vec::new();
    let mut detail_rows = Vec::new();
    let mut failures = Vec::new();

    for tier_path in &tiers {
        let tier_name = tier_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown");
        let mut cases: Vec<_> = std::fs::read_dir(tier_path)
            .map_err(|e| format!("{}: {e}", tier_path.display()))?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.is_dir() && p.join("recipe.json").exists())
            .collect();
        cases.sort();

        let mut pass_count = 0usize;
        let mut frontier: Option<String> = None;

        for cdir in &cases {
            let recipe_txt = std::fs::read_to_string(cdir.join("recipe.json"))
                .map_err(|e| format!("reading recipe.json: {e}"))?;
            let recipe: LadderRecipe = serde_json::from_str(&recipe_txt)
                .map_err(|e| format!("parsing recipe.json: {e}"))?;
            let exp_txt = std::fs::read_to_string(cdir.join("expected.json"))
                .map_err(|e| format!("reading expected.json: {e}"))?;
            let exp: LadderExpected = serde_json::from_str(&exp_txt)
                .map_err(|e| format!("parsing expected.json: {e}"))?;

            let start = Instant::now();
            let (effective_config, profile_apply_res): (PrintConfig, Option<ProfileApplyResult>) =
                if let Some(pb) = &recipe.profile_bundle {
                    let mut lib = if pb.use_builtins {
                        ProfileLibrary::with_builtin_presets()
                    } else {
                        ProfileLibrary::new()
                    };
                    if let Some(rel_dir) = &pb.dir {
                        lib.load_dir(&cdir.join(rel_dir))
                            .map_err(|e| format!("{tier_name}/{}: {e}", recipe.name))?;
                    }
                    for rel_file in &pb.files {
                        lib.load_file(&cdir.join(rel_file))
                            .map_err(|e| format!("{tier_name}/{}: {e}", recipe.name))?;
                    }
                    let res = lib
                        .build_print_config(
                            &recipe.config,
                            pb.machine.as_deref(),
                            pb.filament.as_deref(),
                            pb.process.as_deref(),
                        )
                        .map_err(|e| format!("{tier_name}/{}: {e}", recipe.name))?;
                    (res.config.clone(), Some(res))
                } else {
                    (recipe.config.clone(), None)
                };

            let plate = if !recipe.objects.is_empty() {
                let mut transforms = Vec::with_capacity(recipe.objects.len());
                let mut raw_meshes = Vec::with_capacity(recipe.objects.len());
                let mut sizes_xy = Vec::with_capacity(recipe.objects.len());

                for obj in &recipe.objects {
                    let raw = build_shape(&obj.shape).map_err(|e| e.to_string())?;
                    let xf = obj.transform.unwrap_or_else(|| ObjectTransform {
                        position_xy_mm: [
                            0.5 * effective_config.machine.bed_width_mm,
                            0.5 * effective_config.machine.bed_depth_mm,
                        ],
                        ..ObjectTransform::default()
                    });
                    let pre = prepare_transformed_object(&raw, xf).map_err(|e| e.to_string())?;
                    let sz = pre
                        .aabb()
                        .map(|bb| bb.size())
                        .ok_or_else(|| "empty object mesh".to_owned())?;
                    sizes_xy.push((sz.x.max(0.1), sz.y.max(0.1)));
                    transforms.push(xf);
                    raw_meshes.push(raw);
                }

                if let Some(spacing) = recipe.auto_arrange_spacing_mm {
                    let centers = auto_arrange_footprints(
                        &sizes_xy,
                        effective_config.machine.bed_width_mm,
                        effective_config.machine.bed_depth_mm,
                        spacing,
                    )
                    .map_err(|e| e.to_string())?;
                    for (xf, pos) in transforms.iter_mut().zip(centers) {
                        xf.position_xy_mm = pos;
                    }
                }

                let mut prepared_meshes = Vec::with_capacity(raw_meshes.len());
                for (raw, &xf) in raw_meshes.iter().zip(transforms.iter()) {
                    prepared_meshes
                        .push(prepare_transformed_object(raw, xf).map_err(|e| e.to_string())?);
                }
                slice_meshes(&prepared_meshes, &effective_config).map_err(|e| e.to_string())?
            } else if let Some(shape) = &recipe.shape {
                let mesh = build_shape(shape).map_err(|e| e.to_string())?;
                slice_single_mesh(&mesh, &effective_config).map_err(|e| e.to_string())?
            } else {
                return Err(format!(
                    "{tier_name}/{}: recipe must specify `shape` or `objects`",
                    recipe.name
                ));
            };
            let out = emit_gcode(&plate, &effective_config).map_err(|e| e.to_string())?;
            let (rep, segs) = evaluate_gcode(
                &out.gcode,
                effective_config.filament.diameter_mm,
                effective_config.machine.bed_width_mm,
                effective_config.machine.bed_depth_mm,
            )
            .map_err(|e| e.to_string())?;
            let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;

            let mut errs = Vec::new();
            if !rep.safety.is_clean() {
                errs.push(format!("safety violation: {:?}", rep.safety));
            }
            if rep.active_layer_count != exp.layer_count {
                errs.push(format!(
                    "layers {} != {}",
                    rep.active_layer_count, exp.layer_count
                ));
            }
            if (rep.max_z_mm - exp.max_z_mm).abs() > 0.005 {
                errs.push(format!("max_z {:.3} != {:.3}", rep.max_z_mm, exp.max_z_mm));
            }
            if exp.require_outer_wall
                && rep
                    .feature_volumes_mm3
                    .get("Outer wall")
                    .copied()
                    .unwrap_or(0.0)
                    <= 0.0
            {
                errs.push("missing Outer wall extrusions".to_owned());
            }
            if exp.require_top_surface
                && rep
                    .feature_volumes_mm3
                    .get("Top surface")
                    .copied()
                    .unwrap_or(0.0)
                    <= 0.0
            {
                errs.push("missing Top surface extrusions".to_owned());
            }
            if exp.require_bottom_surface
                && rep
                    .feature_volumes_mm3
                    .get("Bottom surface")
                    .copied()
                    .unwrap_or(0.0)
                    <= 0.0
            {
                errs.push("missing Bottom surface extrusions".to_owned());
            }
            if exp.require_sparse_infill
                && rep
                    .feature_volumes_mm3
                    .get("Sparse infill")
                    .copied()
                    .unwrap_or(0.0)
                    <= 0.0
            {
                errs.push("missing Sparse infill extrusions".to_owned());
            }
            if let Some([cx, cy]) = exp.forbid_center_extrusion_mm {
                // Ensure no extrusion passes within 3 mm of center hole (cx, cy).
                let bad = segs.iter().any(|s| {
                    let mx = 0.5 * (s.x0_mm + s.x1_mm);
                    let my = 0.5 * (s.y0_mm + s.y1_mm);
                    (mx - cx).hypot(my - cy) < 3.0
                });
                if bad {
                    errs.push(format!(
                        "extrusion intruded into clear hole near ({cx}, {cy})"
                    ));
                }
            }
            if let Some(limit) = exp.max_volumetric_flow_mm3_s
                && rep.max_volumetric_flow_mm3_s > limit + 0.05
            {
                errs.push(format!(
                    "volumetric flow {:.2} mm3/s exceeded cap {:.2} mm3/s",
                    rep.max_volumetric_flow_mm3_s, limit
                ));
            }
            if exp.require_first_layer_fan_off && rep.first_layer_max_fan_pwm != 0 {
                errs.push(format!(
                    "first layer fan PWM {} != 0",
                    rep.first_layer_max_fan_pwm
                ));
            }
            if let Some(min_pwm) = exp.min_fan_pwm
                && rep.max_fan_pwm < min_pwm
            {
                errs.push(format!(
                    "max fan PWM {} below required {min_pwm}",
                    rep.max_fan_pwm
                ));
            }
            if exp.require_z_hops && rep.z_hop_count == 0 {
                errs.push("missing Z-hop retraction lifts".to_owned());
            }
            if exp.require_wipes && rep.wipe_moves_count == 0 {
                errs.push("missing wipe-on-retract moves".to_owned());
            }
            if exp.require_accel_commands && rep.accel_commands_count == 0 {
                errs.push("missing acceleration commands (M204 / SET_VELOCITY_LIMIT)".to_owned());
            }
            if exp.require_pressure_advance && rep.pressure_advance_commands_count == 0 {
                errs.push(
                    "missing pressure advance command (M572 / M900 / SET_PRESSURE_ADVANCE)"
                        .to_owned(),
                );
            }
            for (required, feat) in [
                (exp.require_bridge, "Bridge"),
                (exp.require_overhang_wall, "Overhang wall"),
                (exp.require_support, "Support"),
                (exp.require_support_interface, "Support interface"),
                (exp.require_ironing, "Ironing"),
            ] {
                if required && rep.feature_volumes_mm3.get(feat).copied().unwrap_or(0.0) <= 0.0 {
                    errs.push(format!("missing {feat} extrusions"));
                }
            }
            if exp.require_arcs && rep.arc_moves_count == 0 {
                errs.push("missing G2/G3 arc moves".to_owned());
            }
            if exp.require_scarf_seams && rep.scarf_ramp_moves_count == 0 {
                errs.push("missing scarf seam Z-ramp moves".to_owned());
            }
            if exp.forbid_support {
                for feat in ["Support", "Support interface"] {
                    if rep.feature_volumes_mm3.get(feat).copied().unwrap_or(0.0) > 0.0 {
                        errs.push(format!("unexpected {feat} extrusions"));
                    }
                }
            }
            if let Some(min_w) = exp.min_bbox_width_mm {
                let got_w = rep.extrusion_bbox_xy.map(|b| b[2] - b[0]).unwrap_or(0.0);
                if got_w < min_w {
                    errs.push(format!(
                        "extrusion bbox width {got_w:.2} mm < {min_w:.2} mm"
                    ));
                }
            }
            if let Some(min_v) = exp.min_volume_mm3
                && rep.total_filament_volume_mm3 < min_v
            {
                errs.push(format!(
                    "filament volume {:.2} mm3 < min {min_v:.2} mm3",
                    rep.total_filament_volume_mm3
                ));
            }
            if let Some(max_v) = exp.max_volume_mm3
                && rep.total_filament_volume_mm3 > max_v
            {
                errs.push(format!(
                    "filament volume {:.2} mm3 > max {max_v:.2} mm3",
                    rep.total_filament_volume_mm3
                ));
            }
            if let Some(min_ratio) = exp.min_wall_width_variation_ratio
                && rep.wall_width_variation_ratio < min_ratio
            {
                errs.push(format!(
                    "wall width variation ratio {:.3} < min {min_ratio:.3}",
                    rep.wall_width_variation_ratio
                ));
            }
            for needle in &exp.require_gcode_contains {
                if !out.gcode.contains(needle) {
                    errs.push(format!("missing expected G-code substring `{needle}`"));
                }
            }
            for needle in &exp.forbid_gcode_contains {
                if out.gcode.contains(needle) {
                    errs.push(format!("forbidden G-code substring `{needle}` present"));
                }
            }
            if !exp.expected_machine_chain.is_empty() {
                match &profile_apply_res {
                    Some(pr) if pr.machine_chain == exp.expected_machine_chain => {}
                    Some(pr) => errs.push(format!(
                        "machine chain {:?} != {:?}",
                        pr.machine_chain, exp.expected_machine_chain
                    )),
                    None => {
                        errs.push("expected profile_bundle result for machine chain".to_owned())
                    }
                }
            }
            if !exp.expected_filament_chain.is_empty() {
                match &profile_apply_res {
                    Some(pr) if pr.filament_chain == exp.expected_filament_chain => {}
                    Some(pr) => errs.push(format!(
                        "filament chain {:?} != {:?}",
                        pr.filament_chain, exp.expected_filament_chain
                    )),
                    None => {
                        errs.push("expected profile_bundle result for filament chain".to_owned())
                    }
                }
            }
            if !exp.expected_process_chain.is_empty() {
                match &profile_apply_res {
                    Some(pr) if pr.process_chain == exp.expected_process_chain => {}
                    Some(pr) => errs.push(format!(
                        "process chain {:?} != {:?}",
                        pr.process_chain, exp.expected_process_chain
                    )),
                    None => {
                        errs.push("expected profile_bundle result for process chain".to_owned())
                    }
                }
            }
            if !exp.expected_unmapped_keys.is_empty() {
                let mut want = exp.expected_unmapped_keys.clone();
                want.sort();
                match &profile_apply_res {
                    Some(pr) if pr.unmapped_keys == want => {}
                    Some(pr) => errs.push(format!(
                        "unmapped keys {:?} != {:?}",
                        pr.unmapped_keys, want
                    )),
                    None => {
                        errs.push("expected profile_bundle result for unmapped keys".to_owned())
                    }
                }
            }
            if let Some(max_w) = exp.max_profile_warnings {
                match &profile_apply_res {
                    Some(pr) if pr.warnings.len() <= max_w => {}
                    Some(pr) => errs.push(format!(
                        "profile warnings {} exceeded max {max_w}: {:?}",
                        pr.warnings.len(),
                        pr.warnings
                    )),
                    None => {
                        errs.push("expected profile_bundle result for warnings check".to_owned())
                    }
                }
            }

            let status = if errs.is_empty() {
                pass_count += 1;
                "pass"
            } else {
                if frontier.is_none() {
                    frontier = Some(recipe.name.clone());
                }
                failures.push(format!("{tier_name}/{}: {}", recipe.name, errs.join("; ")));
                "FAIL"
            };

            detail_rows.push(format!(
                "| {tier_name}/{} | {} | {} | {:.1} ms | {:.1} mm3 |",
                recipe.name, recipe.description, status, elapsed_ms, rep.total_filament_volume_mm3
            ));
        }

        tier_summaries.push(format!(
            "| {} | {} | {} pass | {} |",
            tier_name,
            cases.len(),
            pass_count,
            frontier.unwrap_or_else(|| "–".to_owned())
        ));
    }

    let doc = format!(
        "# Slicer Complexity Ladder\n\n\
         Generated by `cargo xtask ladder`.\n\n\
         ## Tiers\n\n\
         | Tier | Cases | Results | Frontier |\n\
         |---|---|---|---|\n\
         {}\n\n\
         ## Cases\n\n\
         | Case | Description | Result | Time | Filament Vol |\n\
         |---|---|---|---|---|\n\
         {}\n",
        tier_summaries.join("\n"),
        detail_rows.join("\n")
    );
    let doc_path = root.join("docs").join("ladder.md");
    std::fs::write(&doc_path, doc).map_err(|e| format!("{}: {e}", doc_path.display()))?;

    if !failures.is_empty() {
        return Err(format!("ladder failures:\n  {}", failures.join("\n  ")));
    }
    println!(
        "ladder: OK ({} cases passed across {} tiers)",
        detail_rows.len(),
        tiers.len()
    );
    Ok(())
}
