use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
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

    // User-scoped: config files shared across all sessions
    let config_dir = xdg
        .create_config_directory("")
        .wrap_err("failed to create config directory")?;

    for file in ["settings.json", "keybindings.json"] {
        let path = config_dir.join(file);
        if !path.is_file() {
            fs_err::write(&path, "{}")?;
        }
    }
    let agents_md = config_dir.join("AGENTS.md");
    if !agents_md.exists() {
        fs_err::write(&agents_md, "")?;
    }

    // User-scoped: auth shared across all sessions.
    // Seed from pi's own auth if available so the user doesn't re-authenticate.
    let auth_file = xdg
        .place_data_file("auth.json")
        .wrap_err("failed to create auth file path")?;
    if !auth_file.exists() {
        let pi_auth = home_dir().map(|h| h.join(".pi/agent/auth.json"));
        if let Some(src) = pi_auth.filter(|p| p.is_file()) {
            info!(src = %src.display(), "seeding auth from pi");
            fs_err::copy(&src, &auth_file)?;
        } else {
            fs_err::write(&auth_file, "{}")?;
        }
    }

    // Repo-scoped: per-workspace pi data dir for sessions
    let repo_slug = repo_slug(&workspace);
    let repo_data_dir = xdg
        .create_data_directory(format!("repos/{repo_slug}"))
        .wrap_err("failed to create repo data directory")?;

    info!(config = %config_dir.display(), repo = %repo_data_dir.display(), "directories");
    info!(workspace = %workspace.display(), "starting agent");

    // User-scoped: embedded assets
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

    // Session-scoped: unique compose file and project name
    let session_id = session_id();
    let session_dir = xdg
        .create_cache_directory(format!("sessions/{session_id}"))
        .wrap_err("failed to create session directory")?;

    let compose = generate_compose(
        &workspace,
        &dockerfile,
        &build_context,
        &repo_data_dir,
        &auth_file,
        &config_dir,
        &extension_path,
    );
    let compose_file = session_dir.join("compose.yml");
    fs_err::write(&compose_file, &compose)?;

    let project_name = format!("ramekin-{session_id}");
    let docker_compose = |args: &[&str]| -> Result<Command> {
        let mut cmd = Command::new("docker");
        cmd.args(["compose", "-f"])
            .arg(&compose_file)
            .args(["--project-name", &project_name])
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

    // Clean up session directory
    if let Err(e) = fs_err::remove_dir_all(&session_dir) {
        error!("failed to clean up session dir: {e}");
    }

    if !status.success() {
        bail!("agent exited with error ({})", status);
    }

    Ok(())
}

fn home_dir() -> Option<PathBuf> {
    std::env::var("HOME").ok().map(PathBuf::from)
}

/// Generate a short random session ID.
fn session_id() -> String {
    format!("{:x}", std::process::id())
}

/// Create a slug for a workspace path: `<dirname>-<hash>`.
fn repo_slug(workspace: &Path) -> String {
    let name = workspace
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "root".into());
    let mut hasher = DefaultHasher::new();
    workspace.hash(&mut hasher);
    let hash = hasher.finish();
    format!("{name}-{hash:08x}")
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
    repo_data_dir: &Path,
    auth_file: &Path,
    config_dir: &Path,
    extension_path: &Path,
) -> String {
    let mut volumes = vec![
        format!("{}:/workspace", workspace.display()),
        format!("{}:/root/.pi", repo_data_dir.display()),
        format!("{}:/root/.pi/auth.json", auth_file.display()),
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
    if let Some(dir) = xdg::BaseDirectories::with_prefix("ranger").get_data_home() {
        info!(path = %dir.display(), "mounting ranger data dir");
        volumes.push(format!("{}:/root/.local/share/ranger", dir.display()));
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
