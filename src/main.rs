use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use clap::{Parser, Subcommand};
use color_eyre::eyre::{Context, Result, bail};
use serde::Serialize;
use tracing::{error, info};
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

const DOCKERFILE: &str = include_str!("../assets/Dockerfile");
const RAMEKIN_EXTENSION: &str = include_str!("../assets/ramekin.ts");

#[derive(Parser)]
#[command(about = "Run a pi coding agent in a containerized environment")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start a containerized pi agent session
    Run {
        /// Workspace directory to mount (defaults to current directory)
        #[arg(default_value = ".")]
        workspace: PathBuf,
    },
    /// Show resolved paths and mount configuration
    Config {
        /// Workspace directory to resolve (defaults to current directory)
        #[arg(default_value = ".")]
        workspace: PathBuf,
    },
}

fn main() -> ExitCode {
    color_eyre::install().ok();
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();
    let result = match cli.command {
        Cmd::Run { workspace } => cmd_run(workspace),
        Cmd::Config { workspace } => cmd_config(workspace),
    };

    if let Err(e) = result {
        error!("{e:?}");
        return ExitCode::FAILURE;
    }

    ExitCode::SUCCESS
}

// ---------------------------------------------------------------------------
// Resolved paths shared by run and config
// ---------------------------------------------------------------------------

struct Dirs {
    workspace: PathBuf,
    agent_dir: PathBuf,
    pi_data_dir: PathBuf,
    repo_sessions_dir: PathBuf,
    cache_dir: PathBuf,
    custom_dockerfile: PathBuf,
}

/// Resolve all XDG directories and ensure the agent directory structure exists.
fn resolve_dirs(workspace_arg: PathBuf) -> Result<Dirs> {
    let workspace = workspace_arg
        .canonicalize()
        .wrap_err_with(|| format!("workspace path does not exist: {}", workspace_arg.display()))?;

    let xdg = xdg::BaseDirectories::with_prefix("ramekin");

    // User-scoped: agent directory mirrors /root/.pi/agent in the container.
    // Skills, extensions, settings, keybindings, and AGENTS.md all live here.
    let agent_dir = xdg
        .create_config_directory("agent")
        .wrap_err("failed to create agent config directory")?;

    for file in ["settings.json", "keybindings.json"] {
        let path = agent_dir.join(file);
        if !path.is_file() {
            fs_err::write(&path, "{}")?;
        }
    }
    let agents_md = agent_dir.join("AGENTS.md");
    if !agents_md.exists() {
        fs_err::write(&agents_md, "")?;
    }

    let extensions_dir = xdg
        .create_config_directory("agent/extensions")
        .wrap_err("failed to create extensions directory")?;
    fs_err::write(extensions_dir.join("ramekin.ts"), RAMEKIN_EXTENSION)?;

    xdg.create_config_directory("agent/skills")
        .wrap_err("failed to create skills directory")?;

    let pi_data_dir = xdg
        .create_data_directory("")
        .wrap_err("failed to create pi data directory")?;

    let repo_slug = repo_slug(&workspace);
    let repo_sessions_dir = xdg
        .create_data_directory(format!("repos/{repo_slug}/sessions"))
        .wrap_err("failed to create repo sessions directory")?;

    let cache_dir = xdg
        .create_cache_directory("")
        .wrap_err("failed to create cache directory")?;

    let custom_dockerfile = workspace.join(".ramekin/Dockerfile");

    Ok(Dirs {
        workspace,
        agent_dir,
        pi_data_dir,
        repo_sessions_dir,
        cache_dir,
        custom_dockerfile,
    })
}

// ---------------------------------------------------------------------------
// config subcommand
// ---------------------------------------------------------------------------

fn cmd_config(workspace: PathBuf) -> Result<()> {
    let dirs = resolve_dirs(workspace)?;

    let check = |path: &Path| if path.exists() { "✓" } else { "✗" };

    println!("Workspace");
    println!("  {} {}", check(&dirs.workspace), dirs.workspace.display());

    println!();
    println!("Ramekin directories");
    println!(
        "  {} agent    {}",
        check(&dirs.agent_dir),
        dirs.agent_dir.display()
    );
    println!(
        "  {} data     {}",
        check(&dirs.pi_data_dir),
        dirs.pi_data_dir.display()
    );
    println!(
        "  {} sessions {}",
        check(&dirs.repo_sessions_dir),
        dirs.repo_sessions_dir.display()
    );
    println!(
        "  {} cache    {}",
        check(&dirs.cache_dir),
        dirs.cache_dir.display()
    );

    println!();
    println!("Volume mounts");
    let mut mounts: Vec<(&Path, &str)> = vec![
        (&dirs.pi_data_dir, "/root/.pi"),
        (&dirs.agent_dir, "/root/.pi/agent"),
        (&dirs.repo_sessions_dir, "/root/.pi/agent/sessions"),
        (&dirs.workspace, "/workspace"),
    ];

    let git_config = xdg::BaseDirectories::with_prefix("git").get_config_home();
    let jj_config = xdg::BaseDirectories::with_prefix("jj").get_config_home();
    let ranger_data = xdg::BaseDirectories::with_prefix("ranger").get_data_home();

    if let Some(ref dir) = git_config {
        if dir.is_dir() {
            mounts.push((dir, "/root/.config/git (ro)"));
        }
    }
    if let Some(ref dir) = jj_config {
        if dir.is_dir() {
            mounts.push((dir, "/root/.config/jj (ro)"));
        }
    }
    if let Some(ref dir) = ranger_data {
        if dir.is_dir() {
            mounts.push((dir, "/root/.local/share/ranger"));
        }
    }

    for (source, target) in mounts {
        println!("  {} {} → {}", check(source), source.display(), target);
    }

    println!();
    println!("Dockerfile");
    if dirs.custom_dockerfile.is_file() {
        println!("  ✓ {}", dirs.custom_dockerfile.display());
    } else {
        println!("  embedded (default)");
        println!(
            "  ✗ {} (not found)",
            dirs.custom_dockerfile.display()
        );
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// run subcommand
// ---------------------------------------------------------------------------

fn cmd_run(workspace: PathBuf) -> Result<()> {
    let dirs = resolve_dirs(workspace)?;

    info!(agent = %dirs.agent_dir.display(), repo = %dirs.repo_sessions_dir.display(), "directories");
    info!(workspace = %dirs.workspace.display(), "starting agent");

    fs_err::write(dirs.cache_dir.join("Dockerfile"), DOCKERFILE)?;

    // Always build the base image first
    info!("building base image");
    let base_dockerfile = dirs.cache_dir.join("Dockerfile");
    let status = Command::new("docker")
        .args(["build", "-t", "ramekin-agent", "-f"])
        .arg(&base_dockerfile)
        .arg(&dirs.cache_dir)
        .status()
        .wrap_err("failed to build base image")?;

    if !status.success() {
        bail!("base image build failed ({})", status);
    }

    // If a project Dockerfile exists, build it on top of the base image
    let (dockerfile, build_context) = if dirs.custom_dockerfile.exists() {
        info!("building project image from .ramekin/Dockerfile");
        (dirs.custom_dockerfile.clone(), dirs.workspace.clone())
    } else {
        (base_dockerfile, dirs.cache_dir.clone())
    };

    // Session-scoped: unique compose file and project name
    let xdg = xdg::BaseDirectories::with_prefix("ramekin");
    let session_id = session_id();
    let session_dir = xdg
        .create_cache_directory(format!("sessions/{session_id}"))
        .wrap_err("failed to create session directory")?;

    let compose = generate_compose(
        &dirs.workspace,
        &dockerfile,
        &build_context,
        &dirs.pi_data_dir,
        &dirs.agent_dir,
        &dirs.repo_sessions_dir,
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

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

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

/// Generate a Docker Compose config with all volume mounts.
fn generate_compose(
    workspace: &Path,
    dockerfile: &Path,
    build_context: &Path,
    pi_data_dir: &Path,
    agent_dir: &Path,
    repo_sessions_dir: &Path,
) -> String {
    let mut volumes = vec![
        format!("{}:/root/.pi", pi_data_dir.display()),
        format!("{}:/root/.pi/agent", agent_dir.display()),
        format!("{}:/root/.pi/agent/sessions", repo_sessions_dir.display()),
        format!("{}:/workspace", workspace.display()),
    ];

    if let Some(dir) = xdg::BaseDirectories::with_prefix("git").get_config_home() {
        if dir.is_dir() {
            volumes.push(format!("{}:/root/.config/git:ro", dir.display()));
        }
    }
    if let Some(dir) = xdg::BaseDirectories::with_prefix("jj").get_config_home() {
        if dir.is_dir() {
            volumes.push(format!("{}:/root/.config/jj:ro", dir.display()));
        }
    }
    if let Some(dir) = xdg::BaseDirectories::with_prefix("ranger").get_data_home() {
        if dir.is_dir() {
            volumes.push(format!("{}:/root/.local/share/ranger", dir.display()));
        }
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
