use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use clap::Parser;
use color_eyre::eyre::{Context, Result, bail};
use serde::Serialize;
use tracing::{error, info};
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

const DOCKERFILE: &str = include_str!("../assets/Dockerfile");
const RAMEKIN_EXTENSION: &str = include_str!("../assets/ramekin.ts");

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

    let pi_config_dir = xdg
        .create_config_directory("")
        .wrap_err("failed to create pi config directory")?;

    // Seed empty config files if they don't exist
    for file in ["settings.json", "keybindings.json"] {
        let path = pi_config_dir.join(file);
        if !path.exists() {
            fs_err::write(&path, "{}")?;
        }
    }
    let agents_md = pi_config_dir.join("AGENTS.md");
    if !agents_md.exists() {
        fs_err::write(&agents_md, "")?;
    }

    info!(data = %pi_data_dir.display(), config = %pi_config_dir.display(), "pi directories");
    info!(workspace = %workspace.display(), "starting agent");

    // Write embedded files to XDG cache so docker compose can find them
    let cache_dir = xdg
        .create_cache_directory("")
        .wrap_err("failed to create cache directory")?;

    fs_err::write(cache_dir.join("Dockerfile"), DOCKERFILE)?;
    let extension_path = cache_dir.join("ramekin.ts");
    fs_err::write(&extension_path, RAMEKIN_EXTENSION)?;

    // Always build the base image first
    info!("building base image");
    let base_dockerfile = cache_dir.join("Dockerfile");
    let status = Command::new("docker")
        .args(["build", "-t", "ramekin-agent", "-f"])
        .arg(&base_dockerfile)
        .arg(&cache_dir)
        .status()
        .wrap_err("failed to build base image")?;

    if !status.success() {
        bail!("base image build failed ({})", status);
    }

    // If a project Dockerfile exists, build it on top of the base image
    let custom_dockerfile = workspace.join(".ramekin/Dockerfile");
    let (dockerfile, build_context) = if custom_dockerfile.exists() {
        info!("building project image from .ramekin/Dockerfile");
        (custom_dockerfile, workspace.clone())
    } else {
        (base_dockerfile, cache_dir.clone())
    };

    let compose = generate_compose(
        &workspace,
        &dockerfile,
        &build_context,
        &pi_data_dir,
        &pi_config_dir,
        &extension_path,
    );
    let compose_file = cache_dir.join("compose.yml");
    fs_err::write(&compose_file, &compose)?;

    let docker_compose = |args: &[&str]| -> Result<Command> {
        let mut cmd = Command::new("docker");
        cmd.args(["compose", "-f"])
            .arg(&compose_file)
            .args(args);
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

#[derive(Serialize)]
struct ComposeConfig {
    services: Services,
}

#[derive(Serialize)]
struct Services {
    agent: AgentService,
}

#[derive(Serialize)]
struct AgentService {
    build: BuildConfig,
    image: String,
    stdin_open: bool,
    tty: bool,
    volumes: Vec<String>,
}

#[derive(Serialize)]
struct BuildConfig {
    context: String,
    dockerfile: String,
}

/// Generate a Docker Compose config with all volume mounts baked in.
fn generate_compose(
    workspace: &Path,
    dockerfile: &Path,
    build_context: &Path,
    data_dir: &Path,
    config_dir: &Path,
    extension_path: &Path,
) -> String {
    let mut volumes = vec![
        format!("{}:/workspace", workspace.display()),
        format!("{}:/root/.pi", data_dir.display()),
        format!(
            "{}/settings.json:/root/.pi/agent/settings.json",
            config_dir.display()
        ),
        format!(
            "{}/keybindings.json:/root/.pi/agent/keybindings.json",
            config_dir.display()
        ),
        format!("{}/AGENTS.md:/root/.pi/agent/AGENTS.md", config_dir.display()),
        format!(
            "{}:/root/.pi/agent/extensions/ramekin.ts:ro",
            extension_path.display()
        ),
    ];

    if let Some(dir) = xdg::BaseDirectories::with_prefix("git").get_config_home() {
        info!(path = %dir.display(), "mounting git config dir");
        volumes.push(format!("{}:/root/.config/git:ro", dir.display()));
    }
    if let Some(dir) = xdg::BaseDirectories::with_prefix("jj").get_config_home() {
        info!(path = %dir.display(), "mounting jj config dir");
        volumes.push(format!("{}:/root/.config/jj:ro", dir.display()));
    }

    let config = ComposeConfig {
        services: Services {
            agent: AgentService {
                build: BuildConfig {
                    context: build_context.display().to_string(),
                    dockerfile: dockerfile.display().to_string(),
                },
                image: "ramekin-agent".into(),
                stdin_open: true,
                tty: true,
                volumes,
            },
        },
    };

    serde_yaml::to_string(&config).expect("failed to serialize compose config")
}
