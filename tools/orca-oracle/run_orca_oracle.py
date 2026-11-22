#!/usr/bin/env python3
"""
Black-box OrcaSlicer CLI oracle runner for LayerCraft.

CLEAN-ROOM BOUNDARY:
  This script invokes an externally installed OrcaSlicer binary out-of-process
  solely as a black-box behavioral oracle to measure physical print statistics,
  bounding boxes, and evaluated G-code metrics on synthetic test meshes.
  No OrcaSlicer source code is read, copied, or linked.
"""

from __future__ import annotations

import json
import math
import os
import pathlib
import shlex
import shutil
import subprocess
import sys
import tempfile
from typing import Any


ROOT = pathlib.Path(__file__).resolve().parents[2]
ORACLE_DIR = ROOT / "tests" / "oracle"


def write_ascii_stl(path: pathlib.Path, tris: list[tuple[tuple[float, float, float], tuple[float, float, float], tuple[float, float, float]]]) -> None:
    with path.open("w", encoding="utf-8") as f:
        f.write("solid synthetic\n")
        for a, b, c in tris:
            ux, uy, uz = b[0] - a[0], b[1] - a[1], b[2] - a[2]
            vx, vy, vz = c[0] - a[0], c[1] - a[1], c[2] - a[2]
            nx = uy * vz - uz * vy
            ny = uz * vx - ux * vz
            nz = ux * vy - uy * vx
            mag = math.sqrt(nx * nx + ny * ny + nz * nz) or 1.0
            f.write(f"  facet normal {nx/mag:.6g} {ny/mag:.6g} {nz/mag:.6g}\n")
            f.write("    outer loop\n")
            for p in (a, b, c):
                f.write(f"      vertex {p[0]:.6g} {p[1]:.6g} {p[2]:.6g}\n")
            f.write("    endloop\n  endfacet\n")
        f.write("endsolid synthetic\n")


def box_tris(w: float, d: float, h: float, offset: tuple[float, float, float] = (0.0, 0.0, 0.0)) -> list[Any]:
    ox, oy, oz = offset
    p000 = (ox, oy, oz)
    p100 = (ox + w, oy, oz)
    p110 = (ox + w, oy + d, oz)
    p010 = (ox, oy + d, oz)
    p001 = (ox, oy, oz + h)
    p101 = (ox + w, oy, oz + h)
    p111 = (ox + w, oy + d, oz + h)
    p011 = (ox, oy + d, oz + h)
    quads = [
        (p000, p010, p110, p100),
        (p001, p101, p111, p011),
        (p000, p100, p101, p001),
        (p010, p011, p111, p110),
        (p000, p001, p011, p010),
        (p100, p110, p111, p101),
    ]
    tris = []
    for a, b, c, d_pt in quads:
        tris.append((a, b, c))
        tris.append((a, c, d_pt))
    return tris


def cylinder_tris(r: float, h: float, segs: int = 64) -> list[Any]:
    tris = []
    cb = (0.0, 0.0, 0.0)
    ct = (0.0, 0.0, h)
    for i in range(segs):
        a0 = 2.0 * math.pi * i / segs
        a1 = 2.0 * math.pi * ((i + 1) % segs) / segs
        b0 = (r * math.cos(a0), r * math.sin(a0), 0.0)
        b1 = (r * math.cos(a1), r * math.sin(a1), 0.0)
        t0 = (r * math.cos(a0), r * math.sin(a0), h)
        t1 = (r * math.cos(a1), r * math.sin(a1), h)
        tris.append((cb, b1, b0))
        tris.append((ct, t0, t1))
        tris.append((b0, b1, t1))
        tris.append((b0, t1, t0))
    return tris


def hollow_cylinder_tris(orad: float, irad: float, h: float, segs: int = 64) -> list[Any]:
    tris = []
    for i in range(segs):
        a0 = 2.0 * math.pi * i / segs
        a1 = 2.0 * math.pi * ((i + 1) % segs) / segs
        ob0 = (orad * math.cos(a0), orad * math.sin(a0), 0.0)
        ob1 = (orad * math.cos(a1), orad * math.sin(a1), 0.0)
        ot0 = (orad * math.cos(a0), orad * math.sin(a0), h)
        ot1 = (orad * math.cos(a1), orad * math.sin(a1), h)
        ib0 = (irad * math.cos(a0), irad * math.sin(a0), 0.0)
        ib1 = (irad * math.cos(a1), irad * math.sin(a1), 0.0)
        it0 = (irad * math.cos(a0), irad * math.sin(a0), h)
        it1 = (irad * math.cos(a1), irad * math.sin(a1), h)
        for a, b, c, d_pt in [
            (ob0, ob1, ot1, ot0),
            (ib1, ib0, it0, it1),
            (ib0, ib1, ob1, ob0),
            (ot0, ot1, it1, it0),
        ]:
            tris.append((a, b, c))
            tris.append((a, c, d_pt))
    return tris


def build_tris_from_shape(shape: dict[str, Any]) -> list[Any]:
    kind = shape["kind"]
    if kind == "box":
        off = tuple(shape.get("offset_xyz", [0.0, 0.0, 0.0]))
        return box_tris(shape["width_mm"], shape["depth_mm"], shape["height_mm"], off)
    if kind == "cylinder":
        return cylinder_tris(shape["radius_mm"], shape["height_mm"], shape.get("segments", 64))
    if kind == "hollow_cylinder":
        return hollow_cylinder_tris(
            shape["outer_radius_mm"], shape["inner_radius_mm"], shape["height_mm"], shape.get("segments", 64)
        )
    if kind == "stepped_block":
        bw, bd, bh = shape["base_width_mm"], shape["base_depth_mm"], shape["base_height_mm"]
        tw, td, th = shape["top_width_mm"], shape["top_depth_mm"], shape["top_height_mm"]
        tris = box_tris(bw, bd, bh)
        tris.extend(box_tris(tw, td, th, (0.5 * (bw - tw), 0.5 * (bd - td), bh)))
        return tris
    raise ValueError(f"Unsupported shape kind: {kind}")


def orca_base_cmd(tmpdir: pathlib.Path) -> list[str]:
    override = os.environ.get("ORCA_ORACLE_CMD")
    if override:
        return shlex.split(override)
    if shutil.which("flatpak"):
        return [
            "flatpak",
            "run",
            f"--filesystem={tmpdir}",
            "com.orcaslicer.OrcaSlicer",
        ]
    if shutil.which("orca-slicer"):
        return ["orca-slicer"]
    raise RuntimeError("Neither flatpak (com.orcaslicer.OrcaSlicer) nor orca-slicer found in PATH")


def run_case(case_dir: pathlib.Path, cli_bin: pathlib.Path | None) -> dict[str, Any]:
    recipe = json.loads((case_dir / "recipe.json").read_text(encoding="utf-8"))
    cfg = recipe["config"]
    mach = cfg["machine"]
    fil = cfg["filament"]
    proc = cfg["process"]

    with tempfile.TemporaryDirectory(prefix="orca_oracle_") as td_str:
        td = pathlib.Path(td_str)
        stl_path = td / "part.stl"
        write_ascii_stl(stl_path, build_tris_from_shape(recipe["shape"]))

        w, d = mach["bed_width_mm"], mach["bed_depth_mm"]
        machine_json = {
            "type": "machine",
            "name": "layercraft_oracle_machine",
            "from": "User",
            "gcode_flavor": mach.get("flavor", "klipper"),
            "nozzle_diameter": [str(mach["nozzle_diameter_mm"])],
            "printable_area": ["0x0", f"{w:g}x0", f"{w:g}x{d:g}", f"0x{d:g}"],
            "printable_height": str(mach["bed_height_mm"]),
            "use_relative_e_distances": "1" if mach.get("relative_extrusion") else "0",
            "retraction_length": [str(mach.get("retraction_length_mm", 0.8))],
            "retraction_speed": [str(mach.get("retraction_speed_mm_s", 35.0))],
            "travel_speed": str(mach.get("travel_speed_mm_s", 150.0)),
            "layer_change_gcode": "",
            "machine_start_gcode": "G28\nG90\nM82",
            "machine_end_gcode": "M104 S0\nM140 S0\nM84",
        }
        process_json = {
            "type": "process",
            "name": "layercraft_oracle_process",
            "from": "User",
            "layer_height": str(proc["layer_height_mm"]),
            "initial_layer_print_height": str(proc["first_layer_height_mm"]),
            "wall_loops": str(proc["wall_loops"]),
            "top_shell_layers": str(proc["top_shell_layers"]),
            "bottom_shell_layers": str(proc["bottom_shell_layers"]),
            "sparse_infill_density": f"{proc['sparse_infill_density'] * 100.0:g}%",
            "sparse_infill_pattern": proc.get("sparse_infill_pattern", "grid"),
            "wall_generator": "classic",
            "skirt_loops": str(proc.get("skirt_loops", 0)),
            "brim_type": "no_brim" if proc.get("brim_width_mm", 0.0) <= 0 else "outer_only",
            "brim_width": str(proc.get("brim_width_mm", 0.0)),
            "enable_support": "0",
            "line_width": str(proc["outer_wall_width_mm"]),
            "outer_wall_line_width": str(proc["outer_wall_width_mm"]),
            "inner_wall_line_width": str(proc["inner_wall_width_mm"]),
            "sparse_infill_line_width": str(proc["sparse_infill_width_mm"]),
            "internal_solid_infill_line_width": str(proc["solid_infill_width_mm"]),
            "top_surface_line_width": str(proc["solid_infill_width_mm"]),
            "outer_wall_speed": str(proc["outer_wall_speed_mm_s"]),
            "inner_wall_speed": str(proc["inner_wall_speed_mm_s"]),
            "sparse_infill_speed": str(proc["infill_speed_mm_s"]),
            "internal_solid_infill_speed": str(proc["infill_speed_mm_s"]),
            "top_surface_speed": str(proc["infill_speed_mm_s"]),
            "initial_layer_speed": str(proc["first_layer_speed_mm_s"]),
            "initial_layer_infill_speed": str(proc["first_layer_speed_mm_s"]),
        }
        filament_json = {
            "type": "filament",
            "name": "layercraft_oracle_filament",
            "from": "User",
            "filament_type": ["PLA"],
            "filament_diameter": [str(fil["diameter_mm"])],
            "filament_density": [str(fil["density_g_cm3"])],
            "nozzle_temperature": [str(fil["nozzle_temp_c"])],
            "nozzle_temperature_initial_layer": [str(fil["nozzle_temp_c"])],
            "hot_plate_temp": [str(fil["bed_temp_c"])],
            "hot_plate_temp_initial_layer": [str(fil["bed_temp_c"])],
        }

        m_path = td / "machine.json"
        p_path = td / "process.json"
        f_path = td / "filament.json"
        out_dir = td / "out"
        out_dir.mkdir()
        m_path.write_text(json.dumps(machine_json, indent=2), encoding="utf-8")
        p_path.write_text(json.dumps(process_json, indent=2), encoding="utf-8")
        f_path.write_text(json.dumps(filament_json, indent=2), encoding="utf-8")

        cmd = orca_base_cmd(td) + [
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
            str(out_dir),
            str(stl_path),
        ]
        res = subprocess.run(cmd, cwd=td_str, capture_output=True, text=True, timeout=120)
        if res.returncode != 0:
            raise RuntimeError(
                f"OrcaSlicer oracle failed ({res.returncode}) for {case_dir.name}:\n{res.stdout}\n{res.stderr}"
            )

        stats = json.loads((out_dir / "plate_1.statistics.json").read_text(encoding="utf-8"))
        gcode_path = out_dir / "plate_1.gcode"

        orca_eval = None
        if cli_bin and cli_bin.exists():
            ev = subprocess.run(
                [
                    str(cli_bin),
                    "eval",
                    str(gcode_path),
                    "--filament-diameter",
                    str(fil["diameter_mm"]),
                    "--bed-width",
                    str(w),
                    "--bed-depth",
                    str(d),
                ],
                capture_output=True,
                text=True,
                timeout=30,
            )
            if ev.returncode == 0:
                orca_eval = json.loads(ev.stdout)

        # Save a local reference G-code if requested (gitignored? No, keep expected.json lightweight!).
        summary = stats["summary"]
        features_vol = {k: round(v["volume"], 3) for k, v in summary.get("features", {}).items()}
        expected_path = case_dir / "expected.json"
        existing = json.loads(expected_path.read_text(encoding="utf-8")) if expected_path.exists() else {}
        existing["source"] = f"analytic+{stats.get('generator', 'OrcaSlicer')}"
        existing["orca_oracle"] = {
            "generator": stats.get("generator"),
            "layer_count": summary.get("layer_count"),
            "total_volume_mm3": round(summary.get("volume", 0.0), 3),
            "feature_volumes_mm3": features_vol,
            "eval": orca_eval,
        }
        expected_path.write_text(json.dumps(existing, indent=2) + "\n", encoding="utf-8")
        return existing


def main() -> int:
    cli_bin = ROOT / "target" / "debug" / "layercraft-cli"
    if not cli_bin.exists():
        cli_bin = None

    case_dirs = sorted(p for p in ORACLE_DIR.iterdir() if p.is_dir() and (p / "recipe.json").exists())
    if not case_dirs:
        print("No oracle cases found.", file=sys.stderr)
        return 1

    for cdir in case_dirs:
        recipe = json.loads((cdir / "recipe.json").read_text(encoding="utf-8"))
        if not recipe.get("run_orca_oracle", True):
            continue
        out = run_case(cdir, cli_bin)
        oo = out["orca_oracle"]
        print(
            f"[orca-oracle] {cdir.name}: layers={oo['layer_count']}, "
            f"vol={oo['total_volume_mm3']} mm3"
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
