use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};

use clap::Parser;
use color_eyre::eyre::{Context, Result, bail};
use tracing::{error, info};
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

const COMPOSE_YML: &str = include_str!("../assets/compose.yml");
const DOCKERFILE: &str = include_str!("../assets/Dockerfile");

#[derive(Parser)]
#[command(about = "Run a pi coding agent in a containerized environment")]
struct Cli {
    /// Workspace directory to mount (defaults to current directory)
    #[arg(default_value = ".")]
    workspace: PathBuf,
}

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
    info!(workspace = %workspace.display(), "starting agent");

    // Write embedded files to XDG cache so docker compose can find them
    let cache_dir = xdg
        .create_cache_directory("")
        .wrap_err("failed to create cache directory")?;
    fs_err::write(cache_dir.join("compose.yml"), COMPOSE_YML)?;
    fs_err::write(cache_dir.join("Dockerfile"), DOCKERFILE)?;

    let compose_file = cache_dir.join("compose.yml");

    let docker_compose = |args: &[&str]| -> Result<Command> {
        let mut cmd = Command::new("docker");
        cmd.args(["compose", "-f"])
            .arg(&compose_file)
            .args(args)
            .env("RAMEKIN_WORKSPACE", &workspace)
            .env("RAMEKIN_DATA_DIR", &pi_data_dir);
        Ok(cmd)
    };

    let status = docker_compose(&["up", "-d", "--build"])?
        .status()
        .wrap_err("failed to run docker compose up")?;

    if !status.success() {
        bail!("docker compose up failed ({})", status);
    }

    let status = docker_compose(&["attach", "agent"])?
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .wrap_err("failed to attach to agent")?;

    // Always tear down, regardless of attach exit status
    let down_status = docker_compose(&["down"])?
        .status()
        .wrap_err("failed to run docker compose down")?;

    if !down_status.success() {
        error!("docker compose down failed ({})", down_status);
    }

    if !status.success() {
        bail!("agent exited with error ({})", status);
    }

    Ok(())
}
