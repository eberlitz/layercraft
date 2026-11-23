#!/usr/bin/env python3
"""
Side-by-side black-box OrcaSlicer vs LayerCraft model comparison tool.

CLEAN-ROOM BOUNDARY:
  This script invokes an externally installed OrcaSlicer binary out-of-process
  solely as a black-box CLI oracle using synthetic settings JSON derived from
  LayerCraft's own resolved PrintConfig. No OrcaSlicer source code or vendor
  profile files are read, copied, or linked.
"""

from __future__ import annotations

import argparse
import contextlib
import json
import os
import pathlib
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
from typing import Any

SCRIPT_DIR = pathlib.Path(__file__).resolve().parent
ROOT = SCRIPT_DIR.parents[1]
if str(SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPT_DIR))

import run_orca_oracle as roo


SHAPES: dict[str, dict[str, Any]] = {
    "cube20": {"kind": "box", "width_mm": 20.0, "depth_mm": 20.0, "height_mm": 20.0},
    "cylinder": {"kind": "cylinder", "radius_mm": 12.0, "height_mm": 20.0, "segments": 64},
    "hollow_cylinder": {
        "kind": "hollow_cylinder",
        "outer_radius_mm": 16.0,
        "inner_radius_mm": 8.0,
        "height_mm": 20.0,
        "segments": 64,
    },
    "stepped_block": {
        "kind": "stepped_block",
        "base_width_mm": 30.0,
        "base_depth_mm": 30.0,
        "base_height_mm": 6.0,
        "top_width_mm": 16.0,
        "top_depth_mm": 16.0,
        "top_height_mm": 10.0,
    },
}


def find_layercraft_cli() -> pathlib.Path:
    for candidate in (
        ROOT / "target" / "release" / "layercraft-cli",
        ROOT / "target" / "debug" / "layercraft-cli",
    ):
        if candidate.exists():
            return candidate
    which = shutil.which("layercraft-cli")
    if which:
        return pathlib.Path(which)
    raise FileNotFoundError("Could not find layercraft-cli binary in target/{release,debug} or PATH")


def resolve_layercraft_config(
    cli_bin: pathlib.Path,
    machine: str | None,
    filament: str | None,
    process: str | None,
) -> dict[str, Any]:
    apply_params: dict[str, Any] = {}
    if machine:
        apply_params["machine"] = machine
    if filament:
        apply_params["filament"] = filament
    if process:
        apply_params["process"] = process
    script = [
        {"command": "profile.apply", "params": apply_params},
        {"command": "config.get", "params": {}},
    ]
    res = subprocess.run(
        [str(cli_bin), "run", "/dev/stdin"],
        input=json.dumps(script),
        capture_output=True,
        text=True,
        timeout=30,
        check=True,
    )
    return json.loads(res.stdout)[-1]


def build_synthetic_orca_profiles(cfg: dict[str, Any]) -> tuple[dict[str, Any], dict[str, Any], dict[str, Any]]:
    mach = cfg["machine"]
    fil = cfg["filament"]
    proc = cfg["process"]

    w = mach["bed_width_mm"]
    d = mach["bed_depth_mm"]
    rel_e = bool(mach.get("relative_extrusion", False))
    raw_flavor = mach.get("flavor", "klipper")
    orca_flavor = "marlin2" if raw_flavor == "marlin_buddy" else raw_flavor

    machine_json: dict[str, Any] = {
        "type": "machine",
        "name": "layercraft_compare_machine",
        "from": "User",
        "gcode_flavor": orca_flavor,
        "nozzle_diameter": [str(mach["nozzle_diameter_mm"])],
        "printable_area": ["0x0", f"{w:g}x0", f"{w:g}x{d:g}", f"0x{d:g}"],
        "printable_height": str(mach["bed_height_mm"]),
        "use_relative_e_distances": "1" if rel_e else "0",
        "retraction_length": [str(mach["retraction_length_mm"])],
        "retraction_speed": [str(mach["retraction_speed_mm_s"])],
        "travel_speed": str(mach["travel_speed_mm_s"]),
        "z_hop": [str(mach["z_hop_mm"])],
        "wipe": ["1" if mach.get("wipe_on_retract", False) else "0"],
        "wipe_distance": [str(mach.get("wipe_distance_mm", 0.0))],
        "layer_change_gcode": "G92 E0" if rel_e else "",
        "machine_start_gcode": "G28\nG90\nM83" if rel_e else "G28\nG90\nM82",
        "machine_end_gcode": "M104 S0\nM140 S0\nM84",
    }

    brim_w = float(proc.get("brim_width_mm", 0.0))
    process_json: dict[str, Any] = {
        "type": "process",
        "name": "layercraft_compare_process",
        "from": "User",
        "layer_height": str(proc["layer_height_mm"]),
        "initial_layer_print_height": str(proc["first_layer_height_mm"]),
        "wall_loops": str(proc["wall_loops"]),
        "top_shell_layers": str(proc["top_shell_layers"]),
        "bottom_shell_layers": str(proc["bottom_shell_layers"]),
        "sparse_infill_density": f"{proc['sparse_infill_density'] * 100.0:g}%",
        "sparse_infill_pattern": proc.get("sparse_infill_pattern", "grid"),
        "wall_generator": proc.get("wall_generator", "classic"),
        "reduce_infill_retraction": "1" if proc.get("reduce_infill_retraction", True) else "0",
        "skirt_loops": str(proc.get("skirt_loops", 0)),
        "brim_type": "no_brim" if brim_w <= 0 else "outer_only",
        "brim_width": str(brim_w),
        "enable_support": "1" if proc.get("support", {}).get("enabled", False) else "0",
        "line_width": str(proc["outer_wall_width_mm"]),
        "outer_wall_line_width": str(proc["outer_wall_width_mm"]),
        "inner_wall_line_width": str(proc["inner_wall_width_mm"]),
        "sparse_infill_line_width": str(proc["sparse_infill_width_mm"]),
        "internal_solid_infill_line_width": str(proc["solid_infill_width_mm"]),
        "top_surface_line_width": str(proc["solid_infill_width_mm"]),
        "outer_wall_speed": str(proc["outer_wall_speed_mm_s"]),
        "inner_wall_speed": str(proc["inner_wall_speed_mm_s"]),
        "sparse_infill_speed": str(proc["infill_speed_mm_s"]),
        "internal_solid_infill_speed": str(
            proc.get("solid_infill_speed_mm_s") or proc["infill_speed_mm_s"]
        ),
        "top_surface_speed": str(proc.get("top_surface_speed_mm_s") or proc["infill_speed_mm_s"]),
        "initial_layer_speed": str(proc["first_layer_speed_mm_s"]),
        "initial_layer_infill_speed": str(
            proc.get("first_layer_infill_speed_mm_s") or proc["first_layer_speed_mm_s"]
        ),
    }
    for src_key, dst_key in (
        ("default_acceleration_mm_s2", "default_acceleration"),
        ("outer_wall_acceleration_mm_s2", "outer_wall_acceleration"),
        ("inner_wall_acceleration_mm_s2", "inner_wall_acceleration"),
        ("first_layer_acceleration_mm_s2", "initial_layer_acceleration"),
        ("travel_acceleration_mm_s2", "travel_acceleration"),
    ):
        val = proc.get(src_key)
        if val is not None:
            process_json[dst_key] = str(int(round(val)))

    nozzle_t = fil["nozzle_temp_c"]
    nozzle_t0 = fil.get("first_layer_nozzle_temp_c") or nozzle_t
    bed_t = fil["bed_temp_c"]
    bed_t0 = fil.get("first_layer_bed_temp_c") or bed_t
    filament_json: dict[str, Any] = {
        "type": "filament",
        "name": "layercraft_compare_filament",
        "from": "User",
        "filament_type": [fil.get("filament_type") or "PLA"],
        "filament_diameter": [str(fil["diameter_mm"])],
        "filament_density": [str(fil["density_g_cm3"])],
        "filament_flow_ratio": [str(fil.get("flow_ratio", 1.0))],
        "filament_max_volumetric_speed": [str(fil["max_volumetric_speed_mm3_s"])],
        "nozzle_temperature": [str(nozzle_t)],
        "nozzle_temperature_initial_layer": [str(nozzle_t0)],
        "hot_plate_temp": [str(bed_t)],
        "hot_plate_temp_initial_layer": [str(bed_t0)],
        "fan_min_speed": [str(fil["fan_min_percent"])],
        "fan_max_speed": [str(fil["fan_max_percent"])],
        "close_fan_the_first_x_layers": [str(fil["disable_fan_first_layers"])],
        "full_fan_speed_layer": [str(fil["full_fan_speed_layer"])],
    }
    return machine_json, process_json, filament_json


def count_gcode_metrics(gcode_path: pathlib.Path) -> dict[str, int]:
    text = gcode_path.read_text(encoding="utf-8", errors="replace")
    retracts = 0
    for line in text.splitlines():
        s = line.strip()
        if (s.startswith("G1 ") or s.startswith("G0 ")) and re.search(r"(?:^|\s)E-", s):
            retracts += 1
    return {
        "retract_lines": retracts,
        "lines": len(text.splitlines()),
    }


def parse_duration_str_s(raw: str | None) -> float | None:
    if not raw:
        return None
    total = 0.0
    matched = False
    for num, unit in re.findall(r"(\d+(?:\.\d+)?)\s*([dhms])", raw.lower()):
        matched = True
        v = float(num)
        if unit == "d":
            total += v * 86400.0
        elif unit == "h":
            total += v * 3600.0
        elif unit == "m":
            total += v * 60.0
        elif unit == "s":
            total += v
    return total if matched else None


def fmt_time(sec: float | None) -> str:
    if sec is None:
        return "n/a"
    s = int(round(max(0.0, sec)))
    h, rem = divmod(s, 3600)
    m, sec_rem = divmod(rem, 60)
    if h > 0:
        return f"{h}h {m}m {sec_rem}s"
    if m > 0:
        return f"{m}m {sec_rem}s"
    return f"{sec_rem}s"


def main() -> int:
    parser = argparse.ArgumentParser(description="Compare LayerCraft vs OrcaSlicer on an STL model")
    parser.add_argument("model", nargs="?", help="Optional path to an input .stl file")
    parser.add_argument(
        "--shape",
        choices=sorted(SHAPES.keys()),
        default="cube20",
        help="Built-in synthetic STL shape if no model path is passed (default: cube20)",
    )
    parser.add_argument("--machine", default="LayerCraft CORE One+ HF 0.4mm")
    parser.add_argument("--filament", default="LayerCraft PLA (0.4mm HF Nozzle)")
    parser.add_argument("--process", default="LayerCraft 0.20mm Balanced (0.4mm HF)")
    parser.add_argument("--out-dir", help="Optional directory to keep artifacts")
    parser.add_argument("--json", action="store_true", help="Print JSON report in addition to table")
    args = parser.parse_args()

    cli_bin = find_layercraft_cli()
    cfg = resolve_layercraft_config(cli_bin, args.machine, args.filament, args.process)
    fil_d = cfg["filament"]["diameter_mm"]
    bw = cfg["machine"]["bed_width_mm"]
    bd = cfg["machine"]["bed_depth_mm"]

    if args.out_dir:
        out_root = pathlib.Path(args.out_dir).resolve()
        out_root.mkdir(parents=True, exist_ok=True)
        cm: Any = contextlib.nullcontext(str(out_root))
    else:
        cm = tempfile.TemporaryDirectory(prefix="lc_orca_compare_")

    with cm as td_str:
        td = pathlib.Path(td_str)
        if args.model:
            stl_path = pathlib.Path(args.model).resolve()
            if not stl_path.exists():
                raise FileNotFoundError(f"Model file not found: {stl_path}")
            model_label = stl_path.name
        else:
            stl_path = td / f"{args.shape}.stl"
            roo.write_ascii_stl(stl_path, roo.build_tris_from_shape(SHAPES[args.shape]))
            model_label = args.shape

        lc_gcode = td / "layercraft.gcode"
        lc_stats_path = td / "layercraft.stats.json"
        subprocess.run(
            [
                str(cli_bin),
                "slice",
                str(stl_path),
                "-o",
                str(lc_gcode),
                "--stats",
                str(lc_stats_path),
                "--machine",
                args.machine,
                "--filament",
                args.filament,
                "--process",
                args.process,
            ],
            check=True,
            capture_output=True,
            text=True,
            timeout=120,
        )
        lc_stats = json.loads(lc_stats_path.read_text(encoding="utf-8"))

        mach_j, proc_j, fil_j = build_synthetic_orca_profiles(cfg)
        m_path = td / "orca_machine.json"
        p_path = td / "orca_process.json"
        f_path = td / "orca_filament.json"
        m_path.write_text(json.dumps(mach_j, indent=2), encoding="utf-8")
        p_path.write_text(json.dumps(proc_j, indent=2), encoding="utf-8")
        f_path.write_text(json.dumps(fil_j, indent=2), encoding="utf-8")

        orca_out = td / "orca_out"
        orca_out.mkdir(exist_ok=True)
        cmd = roo.orca_base_cmd(td) + [
            "--slice",
            "0",
            "--arrange",
            "1",
            "--load-settings",
            f"{m_path};{p_path}",
            "--load-filaments",
            str(f_path),
            "--export-statistics",
            "--outputdir",
            str(orca_out),
            str(stl_path),
        ]
        # Ensure --filesystem covers the input STL directory if external.
        if args.model and "flatpak" in cmd[0]:
            cmd.insert(2, f"--filesystem={stl_path.parent}")
        orca_res = subprocess.run(cmd, cwd=td_str, capture_output=True, text=True, timeout=120)
        if orca_res.returncode != 0:
            raise RuntimeError(
                f"OrcaSlicer failed ({orca_res.returncode}):\n{orca_res.stdout}\n{orca_res.stderr}"
            )

        orca_gcode = orca_out / "plate_1.gcode"
        orca_stats_file = orca_out / "plate_1.statistics.json"
        orca_stats = json.loads(orca_stats_file.read_text(encoding="utf-8")) if orca_stats_file.exists() else {}

        bead_r = 0.5 * cfg["process"]["outer_wall_width_mm"]
        cmp_proc = subprocess.run(
            [
                str(cli_bin),
                "compare",
                str(lc_gcode),
                str(orca_gcode),
                "--filament-diameter",
                str(fil_d),
                "--bed-width",
                str(bw),
                "--bed-depth",
                str(bd),
                "--pixel-mm",
                "0.25",
                "--bead-radius-mm",
                f"{bead_r:.3f}",
            ],
            check=True,
            capture_output=True,
            text=True,
            timeout=60,
        )
        cmp_data = json.loads(cmp_proc.stdout)
        rep_lc = cmp_data["a"]
        rep_orca = cmp_data["b"]
        raster = cmp_data["raster"]
        extra_lc = count_gcode_metrics(lc_gcode)
        extra_orca = count_gcode_metrics(orca_gcode)

        orca_summary = orca_stats.get("summary", {})
        orca_time_raw = orca_summary.get("time")
        orca_time_s = (
            float(orca_time_raw)
            if isinstance(orca_time_raw, (int, float))
            else parse_duration_str_s(orca_summary.get("estimated_time"))
        )
        lc_time_s = lc_stats.get("estimated_time_s")

        payload = {
            "model": model_label,
            "presets": {
                "machine": args.machine,
                "filament": args.filament,
                "process": args.process,
            },
            "layercraft": {"eval": rep_lc, "stats": lc_stats, "extra": extra_lc},
            "orca": {"eval": rep_orca, "statistics": orca_stats, "extra": extra_orca},
            "raster_iou": raster,
        }
        if args.out_dir:
            (td / "comparison.json").write_text(json.dumps(payload, indent=2), encoding="utf-8")

        vol_diff_pct = (
            100.0
            * (rep_lc["total_filament_volume_mm3"] - rep_orca["total_filament_volume_mm3"])
            / max(rep_orca["total_filament_volume_mm3"], 1e-6)
        )
        print(f"### Model: `{model_label}` | Preset: `{args.machine}` + `{args.process}`\n")
        print("| Metric | LayerCraft | OrcaSlicer (2.5.2 CLI) | Delta / Notes |")
        print("|---|---:|---:|---|")
        print(f"| Active layers | {rep_lc['active_layer_count']} | {rep_orca['active_layer_count']} | {rep_lc['active_layer_count'] - rep_orca['active_layer_count']:+d} |")
        print(f"| Max Z (mm) | {rep_lc['max_z_mm']:.3f} | {rep_orca['max_z_mm']:.3f} | {rep_lc['max_z_mm'] - rep_orca['max_z_mm']:+.3f} |")
        print(f"| Extruded volume (mm³) | {rep_lc['total_filament_volume_mm3']:.2f} | {rep_orca['total_filament_volume_mm3']:.2f} | {vol_diff_pct:+.1f}% |")
        print(f"| Travel distance (mm) | {rep_lc['total_travel_mm']:.1f} | {rep_orca['total_travel_mm']:.1f} | {rep_lc['total_travel_mm'] - rep_orca['total_travel_mm']:+.1f} |")
        print(f"| Retracts (E- lines) | {extra_lc['retract_lines']} | {extra_orca['retract_lines']} | {extra_lc['retract_lines'] - extra_orca['retract_lines']:+d} |")
        print(f"| Z-hops | {rep_lc['z_hop_count']} | {rep_orca['z_hop_count']} | {rep_lc['z_hop_count'] - rep_orca['z_hop_count']:+d} |")
        print(f"| Wipes | {rep_lc['wipe_moves_count']} | {rep_orca['wipe_moves_count']} | {rep_lc['wipe_moves_count'] - rep_orca['wipe_moves_count']:+d} |")
        print(f"| Peak vol. flow (mm³/s) | {rep_lc['max_volumetric_flow_mm3_s']:.2f} | {rep_orca['max_volumetric_flow_mm3_s']:.2f} | Cap: {fil_j['filament_max_volumetric_speed'][0]} mm³/s |")
        print(f"| Est. time | {fmt_time(lc_time_s)} | {fmt_time(orca_time_s)} | {'' if orca_time_s is None or lc_time_s is None else f'{lc_time_s - orca_time_s:+.0f}s'} |")
        print(f"| Mean / Min 2D Raster IoU | {raster['mean_layer_iou']:.3f} / {raster['min_layer_iou']:.3f} | (same) | {raster['compared_layers']} matched layers |")
        print(f"| Safety clean? | {rep_lc['safety']['non_finite_values']==0 and rep_lc['safety']['out_of_bounds_moves']==0 and rep_lc['safety']['z_regressions']==0} | {rep_orca['safety']['non_finite_values']==0 and rep_orca['safety']['out_of_bounds_moves']==0 and rep_orca['safety']['z_regressions']==0} | 0 OOB / 0 Z-regressions |")

        all_feats = sorted(set(rep_lc["feature_volumes_mm3"].keys()) | set(rep_orca["feature_volumes_mm3"].keys()))
        print("\n| Feature Volume (mm³) | LayerCraft | OrcaSlicer |")
        print("|---|---:|---:|")
        for feat in all_feats:
            vl = rep_lc["feature_volumes_mm3"].get(feat, 0.0)
            vo = rep_orca["feature_volumes_mm3"].get(feat, 0.0)
            print(f"| {feat} | {vl:.2f} | {vo:.2f} |")

        if args.json:
            print("\n" + json.dumps(payload, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
