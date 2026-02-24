use std::process::{Command, ExitCode, Stdio};

use color_eyre::eyre::{Context, Result};
use tracing::info;

fn main() -> ExitCode {
    color_eyre::install().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    if let Err(e) = run() {
        eprintln!("{e:?}");
        return ExitCode::FAILURE;
    }

    ExitCode::SUCCESS
}

fn run() -> Result<()> {
    let compose_dir = env!("CARGO_MANIFEST_DIR");

    info!("starting containers");
    let status = Command::new("docker")
        .args(["compose", "up", "--build", "-d"])
        .current_dir(compose_dir)
        .status()
        .wrap_err("failed to run docker compose")?;

    if !status.success() {
        color_eyre::eyre::bail!("docker compose up failed ({})", status);
    }

    info!("attaching to agent");
    let status = Command::new("docker")
        .args(["compose", "attach", "agent"])
        .current_dir(compose_dir)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .wrap_err("failed to attach to agent")?;

    if !status.success() {
        color_eyre::eyre::bail!("docker compose attach failed ({})", status);
    }

    Ok(())
}
