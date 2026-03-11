use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};

use clap::Parser;
use color_eyre::eyre::{Context, Result, bail};
use tracing::{error, info};
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

const COMPOSE_YML: &str = include_str!("../assets/compose.yml");
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

    // Inject host git/jj config mounts into compose template
    let compose = inject_vcs_config_mounts(COMPOSE_YML);
    fs_err::write(cache_dir.join("compose.yml"), &compose)?;
    fs_err::write(cache_dir.join("Dockerfile"), DOCKERFILE)?;
    fs_err::write(cache_dir.join("ramekin.ts"), RAMEKIN_EXTENSION)?;

    let compose_file = cache_dir.join("compose.yml");

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

    let docker_compose = |args: &[&str]| -> Result<Command> {
        let mut cmd = Command::new("docker");
        cmd.args(["compose", "-f"])
            .arg(&compose_file)
            .args(args)
            .env("RAMEKIN_WORKSPACE", &workspace)
            .env("RAMEKIN_DATA_DIR", &pi_data_dir)
            .env("RAMEKIN_DOCKERFILE", &dockerfile)
            .env("RAMEKIN_BUILD_CONTEXT", &build_context)
            .env("RAMEKIN_CONFIG_DIR", &pi_config_dir)
            .env("RAMEKIN_EXTENSION", cache_dir.join("ramekin.ts"));
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

/// Insert read-only volume mounts for host git/jj config into the compose YAML.
fn inject_vcs_config_mounts(template: &str) -> String {
    let git_config_dir = xdg::BaseDirectories::with_prefix("git").get_config_home();
    let jj_config_dir = xdg::BaseDirectories::with_prefix("jj").get_config_home();

    if git_config_dir.is_none() && jj_config_dir.is_none() {
        return template.to_string();
    }

    let mut extra = String::new();
    if let Some(ref path) = git_config_dir {
        info!(path = %path.display(), "mounting git config dir");
        extra.push_str(&format!(
            "      - \"{}:/root/.config/git:ro\"\n",
            path.display()
        ));
    }
    if let Some(ref path) = jj_config_dir {
        info!(path = %path.display(), "mounting jj config dir");
        extra.push_str(&format!(
            "      - \"{}:/root/.config/jj:ro\"\n",
            path.display()
        ));
    }

    // Insert before the extension bind mount
    template.replace("      - type: bind", &format!("{extra}      - type: bind"))
}
