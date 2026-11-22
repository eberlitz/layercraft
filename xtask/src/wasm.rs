//! `cargo xtask wasm`: verifies L0-L5 library crates and `layercraft-web` compile for `wasm32-unknown-unknown`.

use std::path::Path;
use std::process::Command;

const WASM_CRATES: &[&str] = &[
    "layercraft-geom",
    "layercraft-clip",
    "layercraft-mesh",
    "layercraft-config",
    "layercraft-slice",
    "layercraft-gcode",
    "layercraft-io",
    "layercraft-engine",
    "layercraft-viewport",
    "layercraft-viewport-gpu",
    "layercraft-ui-egui",
    "layercraft-web",
];

pub fn run(root: &Path) -> Result<(), String> {
    let mut cmd = Command::new("cargo");
    cmd.current_dir(root)
        .arg("check")
        .arg("--target")
        .arg("wasm32-unknown-unknown");
    for c in WASM_CRATES {
        cmd.arg("-p").arg(c);
    }
    let status = cmd
        .status()
        .map_err(|e| format!("failed to launch cargo check for wasm32: {e}"))?;
    if !status.success() {
        return Err("wasm32-unknown-unknown check failed".to_owned());
    }
    println!(
        "wasm: OK ({} crates built for wasm32-unknown-unknown)",
        WASM_CRATES.len()
    );
    Ok(())
}
