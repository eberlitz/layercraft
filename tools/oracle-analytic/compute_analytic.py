#!/usr/bin/env python3
"""
Closed-form analytic ground truth calculator for LayerCraft oracle cases.

Computes exact expected layer counts, max Z, bed-centered XY bounding boxes,
and closed-form perimeter bead volumes for canonical geometries.
"""

from __future__ import annotations

import json
import math
import pathlib
from typing import Any


ROOT = pathlib.Path(__file__).resolve().parents[2]
ORACLE_DIR = ROOT / "tests" / "oracle"


def bead_area(w: float, h: float) -> float:
    if w >= h:
        return (w - h) * h + math.pi * (0.5 * h) ** 2
    return math.pi * (0.5 * w) ** 2


def compute_analytic(recipe: dict[str, Any]) -> dict[str, Any]:
    shape = recipe["shape"]
    cfg = recipe["config"]
    mach = cfg["machine"]
    proc = cfg["process"]

    h0 = proc["first_layer_height_mm"]
    h = proc["layer_height_mm"]
    bed_cx = 0.5 * mach["bed_width_mm"]
    bed_cy = 0.5 * mach["bed_depth_mm"]
    ow = proc["outer_wall_width_mm"]

    kind = shape["kind"]
    if kind == "box":
        w, d, zmax = shape["width_mm"], shape["depth_mm"], shape["height_mm"]
        layers = 1 + round((zmax - h0) / h)
        bbox = [bed_cx - 0.5 * w, bed_cy - 0.5 * d, bed_cx + 0.5 * w, bed_cy + 0.5 * d]
        # Outer wall centerline is inset by 0.5 * ow on all 4 sides -> side_len = dim - ow.
        perim_len = 2.0 * ((w - ow) + (d - ow))
        outer_vol = layers * perim_len * bead_area(ow, h)
    elif kind == "cylinder":
        r, zmax = shape["radius_mm"], shape["height_mm"]
        layers = 1 + round((zmax - h0) / h)
        bbox = [bed_cx - r, bed_cy - r, bed_cx + r, bed_cy + r]
        perim_len = 2.0 * math.pi * (r - 0.5 * ow)
        outer_vol = layers * perim_len * bead_area(ow, h)
    elif kind == "hollow_cylinder":
        orad, irad, zmax = shape["outer_radius_mm"], shape["inner_radius_mm"], shape["height_mm"]
        layers = 1 + round((zmax - h0) / h)
        bbox = [bed_cx - orad, bed_cy - orad, bed_cx + orad, bed_cy + orad]
        perim_len = 2.0 * math.pi * (orad - 0.5 * ow) + 2.0 * math.pi * (irad + 0.5 * ow)
        outer_vol = layers * perim_len * bead_area(ow, h)
    elif kind == "stepped_block":
        bw, bd, bh = shape["base_width_mm"], shape["base_depth_mm"], shape["base_height_mm"]
        tw, td, th = shape["top_width_mm"], shape["top_depth_mm"], shape["top_height_mm"]
        zmax = bh + th
        layers = 1 + round((zmax - h0) / h)
        base_layers = 1 + round((bh - h0) / h)
        top_layers = layers - base_layers
        bbox = [bed_cx - 0.5 * bw, bed_cy - 0.5 * bd, bed_cx + 0.5 * bw, bed_cy + 0.5 * bd]
        outer_vol = (
            base_layers * 2.0 * ((bw - ow) + (bd - ow))
            + top_layers * 2.0 * ((tw - ow) + (td - ow))
        ) * bead_area(ow, h)
    else:
        raise ValueError(f"Unsupported shape kind: {kind}")

    return {
        "layer_count": layers,
        "max_z_mm": round(zmax, 6),
        "expected_bbox_xy": [round(v, 4) for v in bbox],
        "outer_wall_volume_mm3": round(outer_vol, 3),
    }


def main() -> int:
    for cdir in sorted(ORACLE_DIR.iterdir()):
        rpath = cdir / "recipe.json"
        if not cdir.is_dir() or not rpath.exists():
            continue
        recipe = json.loads(rpath.read_text(encoding="utf-8"))
        epath = cdir / "expected.json"
        existing = json.loads(epath.read_text(encoding="utf-8")) if epath.exists() else {}
        existing.setdefault("source", "analytic")
        existing["analytic"] = compute_analytic(recipe)
        existing.setdefault(
            "tolerances",
            {
                "max_z_tol_mm": 0.005,
                "bbox_tol_mm": 0.6,
                "outer_wall_rel_tol": 0.05,
                "total_volume_rel_tol": 0.12,
            },
        )
        epath.write_text(json.dumps(existing, indent=2) + "\n", encoding="utf-8")
        print(f"[analytic] {cdir.name}: {existing['analytic']}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
