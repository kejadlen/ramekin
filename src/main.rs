mod config;

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use clap::{Parser, Subcommand};
use color_eyre::eyre::{Context, Result, bail};
use serde::Serialize;
use tracing::{error, info};
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

const DOCKERFILE: &str = include_str!("../assets/Dockerfile");
const RAMEKIN_EXTENSION: &str = include_str!("../assets/ramekin.ts");

const VERSION: &str = env!("RAMEKIN_VERSION");

#[derive(Parser)]
#[command(about = "Run a pi coding agent in a containerized environment", version = VERSION)]
struct Cli {
    /// Workspace directory to mount (defaults to current directory)
    #[arg(global = true, default_value = ".")]
    workspace: PathBuf,

    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start a containerized pi agent session
    Run {
        /// Force a full image rebuild (ignores Docker layer cache)
        #[arg(long)]
        rebuild: bool,
    },
    /// Show resolved paths and mount configuration
    Config,
}

fn main() -> Result<()> {
    color_eyre::install()?;
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();
    let command = cli.command.unwrap_or(Cmd::Run { rebuild: false });
    let ramekin = Ramekin::resolve(cli.workspace)?;

    match command {
        Cmd::Run { rebuild } => ramekin.run(rebuild),
        Cmd::Config => ramekin.config(),
    }
}

// ---------------------------------------------------------------------------
// Ramekin
// ---------------------------------------------------------------------------

struct Ramekin {
    workspace: PathBuf,
    agent_dir: PathBuf,
    pi_data_dir: PathBuf,
    repo_sessions_dir: PathBuf,
    cache_dir: PathBuf,
    custom_dockerfile: Option<PathBuf>,
    mounts: Vec<config::ResolvedMount>,
}

impl Ramekin {
    /// Resolve all paths, create XDG directories, seed default files, and
    /// resolve mounts.
    fn resolve(workspace_arg: PathBuf) -> Result<Self> {
        let workspace = workspace_arg.canonicalize().wrap_err_with(|| {
            format!("workspace path does not exist: {}", workspace_arg.display())
        })?;

        let xdg = xdg::BaseDirectories::with_prefix("ramekin");

        // Agent directory mirrors /root/.pi/agent in the container.
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

        let custom_dockerfile_path = workspace.join(".ramekin/Dockerfile");
        let custom_dockerfile = custom_dockerfile_path
            .is_file()
            .then_some(custom_dockerfile_path);

        // Builtin mounts (always present)
        let builtin = [
            (&pi_data_dir, "/root/.pi"),
            (&agent_dir, "/root/.pi/agent"),
            (&repo_sessions_dir, "/root/.pi/agent/sessions"),
            (&workspace, "/workspace"),
        ];
        let mut mounts: Vec<config::ResolvedMount> = builtin
            .into_iter()
            .map(|(source, target)| config::ResolvedMount {
                source: source.clone(),
                target: target.into(),
                read_only: false,
            })
            .collect();

        // Config mounts (user-configurable, skipped when source doesn't exist)
        mounts.extend(config::Config::default().resolve_mounts());

        Ok(Self {
            workspace,
            agent_dir,
            pi_data_dir,
            repo_sessions_dir,
            cache_dir,
            custom_dockerfile,
            mounts,
        })
    }

    fn config(&self) -> Result<()> {
        let check = |path: &Path| if path.exists() { "✓" } else { "✗" };

        println!("Workspace");
        println!("  {} {}", check(&self.workspace), self.workspace.display());

        println!();
        println!("Ramekin directories");
        println!(
            "  {} agent    {}",
            check(&self.agent_dir),
            self.agent_dir.display()
        );
        println!(
            "  {} data     {}",
            check(&self.pi_data_dir),
            self.pi_data_dir.display()
        );
        println!(
            "  {} sessions {}",
            check(&self.repo_sessions_dir),
            self.repo_sessions_dir.display()
        );
        println!(
            "  {} cache    {}",
            check(&self.cache_dir),
            self.cache_dir.display()
        );

        println!();
        println!("Volume mounts");
        for m in &self.mounts {
            println!(
                "  {} {} → {}",
                check(&m.source),
                m.source.display(),
                m.display_target()
            );
        }

        println!();
        println!("Dockerfile");
        match &self.custom_dockerfile {
            Some(path) => println!("  ✓ {}", path.display()),
            None => {
                println!("  embedded (default)");
                println!(
                    "  ✗ {} (not found)",
                    self.workspace.join(".ramekin/Dockerfile").display()
                );
            }
        }

        Ok(())
    }

    fn run(&self, rebuild: bool) -> Result<()> {
        info!(agent = %self.agent_dir.display(), repo = %self.repo_sessions_dir.display(), "directories");
        info!(workspace = %self.workspace.display(), "starting agent");

        // Write the embedded Dockerfile to the cache directory
        let base_dockerfile = self.cache_dir.join("Dockerfile");
        fs_err::write(&base_dockerfile, DOCKERFILE)?;

        // Build the base image
        if rebuild {
            info!("rebuilding base image (no cache)");
        } else {
            info!("building base image");
        }
        let mut build_cmd = Command::new("docker");
        build_cmd
            .args(["build", "-t", "ramekin-agent", "-f"])
            .arg(&base_dockerfile);
        if rebuild {
            build_cmd.args(["--no-cache", "--pull"]);
        }
        build_cmd.arg(&self.cache_dir);
        let status = build_cmd.status().wrap_err("failed to build base image")?;
        if !status.success() {
            bail!("base image build failed ({})", status);
        }

        // Determine the final dockerfile and build context
        let (dockerfile, build_context) = match &self.custom_dockerfile {
            Some(custom) => {
                info!("building project image from .ramekin/Dockerfile");
                (custom.clone(), self.workspace.clone())
            }
            None => (base_dockerfile, self.cache_dir.clone()),
        };

        // Session-scoped: unique compose file and project name
        let xdg = xdg::BaseDirectories::with_prefix("ramekin");
        let session_id = session_id();
        let session_dir = xdg
            .create_cache_directory(format!("sessions/{session_id}"))
            .wrap_err("failed to create session directory")?;

        let compose = generate_compose(&dockerfile, &build_context, &self.mounts);
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

        if let Err(e) = fs_err::remove_dir_all(&session_dir) {
            error!("failed to clean up session dir: {e}");
        }

        if !status.success() {
            bail!("agent exited with error ({})", status);
        }

        Ok(())
    }
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
    dockerfile: &Path,
    build_context: &Path,
    mounts: &[config::ResolvedMount],
) -> String {
    let volumes: Vec<String> = mounts.iter().map(|m| m.to_volume_string()).collect();

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
