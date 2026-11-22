//! `cargo xtask fast`: pre-commit verification gate.

use std::path::Path;
use std::process::Command;

use crate::{layers, licences, wasm};

fn run_cmd(root: &Path, args: &[&str]) -> Result<(), String> {
    let status = Command::new("cargo")
        .current_dir(root)
        .args(args)
        .status()
        .map_err(|e| format!("failed to launch cargo {}: {e}", args.join(" ")))?;
    if !status.success() {
        return Err(format!("cargo {} failed", args.join(" ")));
    }
    Ok(())
}

pub fn run(root: &Path) -> Result<(), String> {
    run_cmd(root, &["fmt", "--all", "--", "--check"])?;
    run_cmd(
        root,
        &[
            "clippy",
            "--workspace",
            "--all-targets",
            "--",
            "-D",
            "warnings",
        ],
    )?;
    layers::run(root)?;
    licences::run(root, true)?;
    wasm::run(root)?;
    run_cmd(root, &["test", "--workspace"])?;
    println!("fast: ALL CHECKS PASSED");
    Ok(())
}
