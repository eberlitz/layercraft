//! Workspace verification tasks (`cargo xtask <command>`).

mod fast;
mod ladder;
mod layers;
mod licences;
mod oracle;
mod wasm;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "xtask",
    about = "LayerCraft build, lint, layering, license, and oracle runner"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Enforce crate layering, UI isolation below L5, and the 2D kernel seam.
    Layers,
    /// Audit transitive dependencies for copyleft licenses and generate THIRD-PARTY-LICENSES.txt.
    Licences {
        #[arg(long)]
        check: bool,
    },
    /// Verify L0-L4 library crates compile for wasm32-unknown-unknown.
    Wasm,
    /// Run all analytic and OrcaSlicer black-box oracle tests and regenerate docs/oracle.md.
    Oracle {
        #[arg(long)]
        update_orca: bool,
    },
    /// Run tiered complexity ladder regression tests and regenerate docs/ladder.md.
    Ladder,
    /// Run fmt, clippy (-D warnings), layers, licences --check, wasm, and workspace unit tests.
    Fast,
    /// Run the full gate: fast + oracle + ladder.
    Ci,
}

fn workspace_root() -> Result<PathBuf, String> {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| "cannot locate workspace root".to_owned())
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let root = match workspace_root() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("xtask: {e}");
            return ExitCode::FAILURE;
        }
    };

    let res = match cli.cmd {
        Cmd::Layers => layers::run(&root),
        Cmd::Licences { check } => licences::run(&root, check),
        Cmd::Wasm => wasm::run(&root),
        Cmd::Oracle { update_orca } => oracle::run(&root, update_orca),
        Cmd::Ladder => ladder::run(&root),
        Cmd::Fast => fast::run(&root),
        Cmd::Ci => fast::run(&root)
            .and_then(|()| oracle::run(&root, false))
            .and_then(|()| ladder::run(&root)),
    };

    if let Err(e) = res {
        eprintln!("xtask error: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
