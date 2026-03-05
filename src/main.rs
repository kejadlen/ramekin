use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};

use clap::Parser;
use color_eyre::eyre::{Context, Result, bail};
use tracing::{error, info};
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

#[derive(Parser)]
#[command(about = "Run a pi coding agent in a containerized environment")]
struct Cli {
    /// Workspace directory to mount (defaults to current directory)
    #[arg(default_value = ".")]
    workspace: PathBuf,
}

const IMAGE_NAME: &str = "ramekin-agent";

fn main() -> ExitCode {
    color_eyre::install().ok();
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env())
        .init();

    if let Err(e) = run() {
        error!("{e:?}");
        return ExitCode::FAILURE;
    }

    ExitCode::SUCCESS
}

fn run() -> Result<()> {
    let cli = Cli::parse();

    let workspace = cli
        .workspace
        .canonicalize()
        .wrap_err_with(|| format!("workspace path does not exist: {}", cli.workspace.display()))?;

    let xdg = xdg::BaseDirectories::with_prefix("ramekin");
    let pi_data_dir = xdg
        .create_data_directory("")
        .wrap_err("failed to create pi data directory")?;
    info!(data = %pi_data_dir.display(), "pi data directory");

    let dockerfile = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Dockerfile");

    info!("building agent image");
    let status = Command::new("docker")
        .args(["build", "-t", IMAGE_NAME, "-f"])
        .arg(&dockerfile)
        .arg(dockerfile.parent().unwrap())
        .status()
        .wrap_err("failed to run docker build")?;

    if !status.success() {
        bail!("docker build failed ({})", status);
    }

    info!(?workspace, "starting agent");
    let status = Command::new("docker")
        .args(["run", "--rm", "-it"])
        .arg("-v")
        .arg(format!("{}:/workspace", workspace.display()))
        .arg("-v")
        .arg(format!("{}:/root/.pi", pi_data_dir.display()))
        .arg(IMAGE_NAME)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .wrap_err("failed to run docker")?;

    if !status.success() {
        bail!("docker run failed ({})", status);
    }

    Ok(())
}
