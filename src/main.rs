use std::path::{Path, PathBuf};
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

/// Resolve the agent Dockerfile and build context for the given workspace.
///
/// If `<workspace>/.ramekin/Dockerfile` exists, the agent is built from the
/// user-supplied Dockerfile with the workspace as the build context.
/// Otherwise we fall back to the default Dockerfile shipped with ramekin.
fn resolve_agent_dockerfile(workspace: &Path) -> (PathBuf, PathBuf) {
    let custom = workspace.join(".ramekin/Dockerfile");
    if custom.is_file() {
        info!(?custom, "using custom agent Dockerfile");
        (custom, workspace.to_path_buf())
    } else {
        let default = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Dockerfile");
        info!("no .ramekin/Dockerfile found, using default");
        (default, PathBuf::from(env!("CARGO_MANIFEST_DIR")))
    }
}

fn run() -> Result<()> {
    let compose_dir = env!("CARGO_MANIFEST_DIR");

    let workspace = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().expect("cannot determine current directory"));

    let workspace = workspace
        .canonicalize()
        .wrap_err_with(|| format!("workspace path does not exist: {}", workspace.display()))?;

    let (agent_dockerfile, agent_context) = resolve_agent_dockerfile(&workspace);

    info!(?workspace, "starting containers");
    let status = Command::new("docker")
        .args(["compose", "up", "--build", "-d"])
        .env("AGENT_DOCKERFILE", &agent_dockerfile)
        .env("AGENT_CONTEXT", &agent_context)
        .env("WORKSPACE_DIR", &workspace)
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
