//! `cargo xtask oracle [--update-orca]`: checks LayerCraft against analytic and
//! external OrcaSlicer black-box CLI expectations, and regenerates `docs/oracle.md`.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use layercraft_config::PrintConfig;
use layercraft_gcode::{emit_gcode, evaluate_gcode};
use layercraft_slice::slice_single_mesh;
use layercraft_testkit::{ShapeSpec, build_shape};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct OracleRecipe {
    name: String,
    description: String,
    shape: ShapeSpec,
    config: PrintConfig,
}

#[derive(Debug, Deserialize)]
struct AnalyticExpected {
    layer_count: usize,
    max_z_mm: f64,
    expected_bbox_xy: [f64; 4],
    outer_wall_volume_mm3: f64,
}

#[derive(Debug, Deserialize)]
struct OrcaOracleExpected {
    generator: Option<String>,
    layer_count: Option<usize>,
    total_volume_mm3: Option<f64>,
    feature_volumes_mm3: Option<BTreeMap<String, f64>>,
    eval: Option<layercraft_gcode::GcodeReport>,
}

#[derive(Debug, Deserialize)]
struct OracleTolerances {
    max_z_tol_mm: f64,
    bbox_tol_mm: f64,
    outer_wall_rel_tol: f64,
    total_volume_rel_tol: f64,
}

#[derive(Debug, Deserialize)]
struct OracleExpected {
    source: String,
    analytic: AnalyticExpected,
    orca_oracle: Option<OrcaOracleExpected>,
    tolerances: OracleTolerances,
}

pub fn run(root: &Path, update_orca: bool) -> Result<(), String> {
    if update_orca {
        let status = Command::new("python3")
            .current_dir(root)
            .arg("tools/orca-oracle/run_orca_oracle.py")
            .status()
            .map_err(|e| format!("failed to run tools/orca-oracle/run_orca_oracle.py: {e}"))?;
        if !status.success() {
            return Err("tools/orca-oracle/run_orca_oracle.py failed".to_owned());
        }
    }

    let oracle_dir = root.join("tests").join("oracle");
    let mut dirs: Vec<_> = std::fs::read_dir(&oracle_dir)
        .map_err(|e| format!("{}: {e}", oracle_dir.display()))?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_dir() && p.join("recipe.json").exists())
        .collect();
    dirs.sort();

    if dirs.is_empty() {
        return Err("no oracle cases found under tests/oracle/".to_owned());
    }

    let mut rows = Vec::new();
    let mut passed = 0usize;
    let mut failures = Vec::new();

    for dir in &dirs {
        let recipe_txt = std::fs::read_to_string(dir.join("recipe.json"))
            .map_err(|e| format!("reading recipe.json: {e}"))?;
        let recipe: OracleRecipe =
            serde_json::from_str(&recipe_txt).map_err(|e| format!("parsing recipe.json: {e}"))?;

        let exp_txt = std::fs::read_to_string(dir.join("expected.json"))
            .map_err(|e| format!("reading expected.json: {e}"))?;
        let exp: OracleExpected =
            serde_json::from_str(&exp_txt).map_err(|e| format!("parsing expected.json: {e}"))?;

        let mesh = build_shape(&recipe.shape).map_err(|e| e.to_string())?;
        let plate = slice_single_mesh(&mesh, &recipe.config).map_err(|e| e.to_string())?;
        let out = emit_gcode(&plate, &recipe.config).map_err(|e| e.to_string())?;
        let (rep, _) = evaluate_gcode(
            &out.gcode,
            recipe.config.filament.diameter_mm,
            recipe.config.machine.bed_width_mm,
            recipe.config.machine.bed_depth_mm,
        )
        .map_err(|e| e.to_string())?;

        let mut errs = Vec::new();
        if !rep.safety.is_clean() {
            errs.push(format!("safety check failed: {:?}", rep.safety));
        }
        if rep.active_layer_count != exp.analytic.layer_count {
            errs.push(format!(
                "layer count {} != analytic {}",
                rep.active_layer_count, exp.analytic.layer_count
            ));
        }
        if (rep.max_z_mm - exp.analytic.max_z_mm).abs() > exp.tolerances.max_z_tol_mm {
            errs.push(format!(
                "max_z {:.4} != analytic {:.4}",
                rep.max_z_mm, exp.analytic.max_z_mm
            ));
        }

        if let Some(bbox) = rep.extrusion_bbox_xy {
            for (idx, (&got, &want)) in bbox
                .iter()
                .zip(exp.analytic.expected_bbox_xy.iter())
                .enumerate()
            {
                if (got - want).abs() > exp.tolerances.bbox_tol_mm {
                    errs.push(format!("bbox[{idx}] {got:.3} != {want:.3}"));
                }
            }
        } else {
            errs.push("missing extrusion bbox".to_owned());
        }

        let got_outer = rep
            .feature_volumes_mm3
            .get("Outer wall")
            .copied()
            .unwrap_or(0.0);
        let rel_outer = (got_outer - exp.analytic.outer_wall_volume_mm3).abs()
            / exp.analytic.outer_wall_volume_mm3.max(1e-6);
        if rel_outer > exp.tolerances.outer_wall_rel_tol {
            errs.push(format!(
                "outer wall vol {got_outer:.2} mm3 differs {:.2}% from analytic {:.2} mm3",
                rel_outer * 100.0,
                exp.analytic.outer_wall_volume_mm3
            ));
        }

        let mut orca_vol_str = "-".to_owned();
        if let Some(oo) = &exp.orca_oracle {
            if let Some(olayers) = oo.layer_count
                && rep.active_layer_count != olayers
            {
                errs.push(format!(
                    "layer count {} != orca {}",
                    rep.active_layer_count, olayers
                ));
            }
            if let Some(ovol) = oo.total_volume_mm3 {
                orca_vol_str = format!("{ovol:.1}");
                let rel = (rep.total_filament_volume_mm3 - ovol).abs() / ovol.max(1e-6);
                if rel > exp.tolerances.total_volume_rel_tol {
                    errs.push(format!(
                        "total vol {:.2} mm3 differs {:.2}% from orca {ovol:.2} mm3",
                        rep.total_filament_volume_mm3,
                        rel * 100.0
                    ));
                }
            }
            if let Some(o_outer) = oo
                .feature_volumes_mm3
                .as_ref()
                .and_then(|m| m.get("Outer wall"))
                .copied()
            {
                let rel_o = (got_outer - o_outer).abs() / o_outer.max(1e-6);
                if rel_o > exp.tolerances.outer_wall_rel_tol {
                    errs.push(format!(
                        "outer wall vol {got_outer:.2} differs {:.2}% from orca {o_outer:.2}",
                        rel_o * 100.0
                    ));
                }
            }
            if let Some(oeval) = &oo.eval
                && let (Some(gb), Some(ob)) = (rep.extrusion_bbox_xy, oeval.extrusion_bbox_xy)
            {
                for (idx, (&g, &o)) in gb.iter().zip(ob.iter()).enumerate() {
                    if (g - o).abs() > exp.tolerances.bbox_tol_mm {
                        errs.push(format!("bbox[{idx}] {g:.3} != orca_eval {o:.3}"));
                    }
                }
            }
            let _ = &oo.generator;
        }

        let status = if errs.is_empty() {
            passed += 1;
            "pass"
        } else {
            failures.push(format!("{}: {}", recipe.name, errs.join("; ")));
            "FAIL"
        };

        rows.push(format!(
            "| {} | {} | {} | {} | {:.1} mm3 | {} mm3 | {} |",
            recipe.name,
            exp.source,
            status,
            rep.active_layer_count,
            rep.total_filament_volume_mm3,
            orca_vol_str,
            recipe.description
        ));
    }

    let doc = format!(
        "# Reference Oracle Tests\n\n\
         Generated by `cargo xtask oracle`. Analytic baselines come from `tools/oracle-analytic/`; \
         external black-box baselines come from running `OrcaSlicer 2.5.2` out-of-process via `tools/orca-oracle/`.\n\n\
         **{passed} / {} cases pass.**\n\n\
         | Case | Source | Result | Layers | LayerCraft Vol | Orca Vol | Notes |\n\
         |---|---|---|---|---|---|---|\n\
         {}\n",
        dirs.len(),
        rows.join("\n")
    );
    let doc_path = root.join("docs").join("oracle.md");
    std::fs::write(&doc_path, doc).map_err(|e| format!("{}: {e}", doc_path.display()))?;

    if !failures.is_empty() {
        return Err(format!("oracle failures:\n  {}", failures.join("\n  ")));
    }
    println!("oracle: OK ({passed}/{} cases passed)", dirs.len());
    Ok(())
}
