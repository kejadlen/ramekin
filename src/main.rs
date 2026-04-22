mod config;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;
use miette::{Context, IntoDiagnostic, Result, bail};
use serde::Serialize;
use tracing::{error, info};
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

const DOCKERFILE: &str = include_str!("../assets/Dockerfile");
const RAMEKIN_PROMPT: &str = include_str!("../assets/ramekin-prompt.md");

const VERSION: &str = env!("RAMEKIN_VERSION");

#[derive(Parser)]
#[command(about = "Run a pi coding agent in a containerized environment", version = VERSION)]
struct Cli {
    /// Workspace directory to mount (defaults to current directory)
    #[arg(global = true, default_value = ".")]
    workspace: PathBuf,

    #[command(subcommand)]
    command: Option<Cmd>,

    /// Extra arguments forwarded to pi inside the container (after --)
    #[arg(last = true, global = true)]
    pi_args: Vec<String>,
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
    /// Generate shell completions
    Completions {
        /// Shell to generate completions for
        shell: Shell,
    },
}

fn main() -> Result<()> {
    miette::set_hook(Box::new(|_| {
        Box::new(miette::MietteHandlerOpts::new().build())
    }))?;
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();
    let command = cli.command.unwrap_or(Cmd::Run { rebuild: false });

    // Completions doesn't need workspace resolution.
    if let Cmd::Completions { shell } = command {
        clap_complete::generate(
            shell,
            &mut Cli::command(),
            "ramekin",
            &mut std::io::stdout(),
        );
        return Ok(());
    }

    let ramekin = Ramekin::resolve(cli.workspace)?;

    match command {
        Cmd::Run { rebuild } => ramekin.run(rebuild, &cli.pi_args),
        Cmd::Config => ramekin.config(),
        Cmd::Completions { .. } => unreachable!(),
    }
}

// ---------------------------------------------------------------------------
// Ramekin
// ---------------------------------------------------------------------------

struct Ramekin {
    workspace: PathBuf,
    xdg: xdg::BaseDirectories,
    agent_dir: PathBuf,
    pi_data_dir: PathBuf,
    repo_sessions_dir: PathBuf,
    cache_dir: PathBuf,
    custom_dockerfile: Option<PathBuf>,
    config: config::ScopedConfig,
}

impl Ramekin {
    /// Resolve all paths, create XDG directories, assemble pi config, and
    /// resolve mounts.
    fn resolve(workspace_arg: PathBuf) -> Result<Self> {
        let workspace = workspace_arg
            .canonicalize()
            .into_diagnostic()
            .wrap_err_with(|| {
                format!("workspace path does not exist: {}", workspace_arg.display())
            })?;

        let xdg = xdg::BaseDirectories::with_prefix("ramekin");

        // Agent directory mirrors /root/.pi/agent in the container.
        let agent_dir = xdg
            .create_config_directory("agent")
            .into_diagnostic()
            .wrap_err("failed to create agent config directory")?;

        let pi_data_dir = xdg
            .create_data_directory("")
            .into_diagnostic()
            .wrap_err("failed to create pi data directory")?;

        let repo_slug = repo_slug(&workspace);
        let repo_sessions_dir = xdg
            .create_data_directory(format!("repos/{repo_slug}/sessions"))
            .into_diagnostic()
            .wrap_err("failed to create repo sessions directory")?;

        let cache_dir = xdg
            .create_cache_directory("")
            .into_diagnostic()
            .wrap_err("failed to create cache directory")?;

        let custom_dockerfile_path = workspace.join(".ramekin/Dockerfile");
        let custom_dockerfile = custom_dockerfile_path
            .is_file()
            .then_some(custom_dockerfile_path);

        // Builtin mounts (always present, not overridable)
        let builtin_entries = [
            (&pi_data_dir, "/root/.pi"),
            (&agent_dir, "/root/.pi/agent"),
            (&repo_sessions_dir, "/root/.pi/agent/sessions"),
            (&workspace, "/workspace"),
        ];
        let builtin_mounts: Vec<config::ResolvedMount> = builtin_entries
            .into_iter()
            .map(|(source, target)| config::ResolvedMount {
                source: source.clone(),
                target: target.into(),
                writable: true,
            })
            .collect();

        let config = config::Config::load(&workspace, builtin_mounts)
            .wrap_err("failed to load ramekin configuration")?;

        // Clear and reassemble the agent dir from pi config.
        config::clear_agent_dir(&agent_dir).wrap_err("failed to clear agent directory")?;

        let resolved_pi: Vec<config::ResolvedPiEntry> = config
            .merged_pi()
            .iter()
            .map(|sv| sv.value.resolve())
            .collect();
        config::assemble_pi(&agent_dir, &resolved_pi).wrap_err("failed to assemble pi config")?;

        // Write the system prompt file so pi can read it via --append-system-prompt.
        let prompt_path = agent_dir.join("ramekin-prompt.md");
        fs_err::write(&prompt_path, RAMEKIN_PROMPT).into_diagnostic()?;

        Ok(Self {
            workspace,
            xdg,
            agent_dir,
            pi_data_dir,
            repo_sessions_dir,
            cache_dir,
            custom_dockerfile,
            config,
        })
    }

    fn config(&self) -> Result<()> {
        println!("Workspace");
        println!("  {}", self.workspace.display());

        println!();
        println!("Ramekin directories");
        println!("  agent    {}", self.agent_dir.display());
        println!("  data     {}", self.pi_data_dir.display());
        println!("  sessions {}", self.repo_sessions_dir.display());
        println!("  cache    {}", self.cache_dir.display());

        let merged_mounts = self.config.merged_mounts();
        let merged_pi = self.config.merged_pi();
        let merged_env = self.config.merged_env();

        let scope_label = |scope: config::Scope| -> String {
            self.config
                .layers
                .iter()
                .find(|l| l.scope == scope)
                .and_then(|l| l.path.as_ref())
                .map(|p| format!("{scope} ({})", p.display()))
                .unwrap_or_else(|| scope.to_string())
        };

        // Mounts
        if !merged_mounts.is_empty() {
            println!();
            println!("Mounts");
            let scopes: std::collections::BTreeSet<_> =
                merged_mounts.iter().map(|sv| sv.scope).collect();
            for scope in scopes {
                println!("  {}", scope_label(scope));
                for sv in merged_mounts.iter().filter(|sv| sv.scope == scope) {
                    println!(
                        "    {} → {}",
                        sv.value.source.display(),
                        sv.value.display_target()
                    );
                }
            }
        }

        // Environment
        if !merged_env.is_empty() {
            println!();
            println!("Environment");
            let scopes: std::collections::BTreeSet<_> =
                merged_env.iter().map(|sv| sv.scope).collect();
            for scope in scopes {
                println!("  {}", scope_label(scope));
                for sv in merged_env.iter().filter(|sv| sv.scope == scope) {
                    println!("    {}={}", sv.value.0, sv.value.1);
                }
            }
        }

        // Pi config
        if !merged_pi.is_empty() {
            println!();
            println!("Pi config");
            let scopes: std::collections::BTreeSet<_> =
                merged_pi.iter().map(|sv| sv.scope).collect();
            for scope in scopes {
                println!("  {}", scope_label(scope));
                for sv in merged_pi.iter().filter(|sv| sv.scope == scope) {
                    let resolved = sv.value.resolve();
                    let kind = if resolved.source.is_dir() {
                        "dir"
                    } else if resolved.source.is_file() {
                        "file"
                    } else {
                        "missing"
                    };
                    let marker = if resolved.source.exists() {
                        "✓"
                    } else {
                        "✗"
                    };
                    println!(
                        "    {marker} {} → {} ({kind})",
                        resolved.source.display(),
                        resolved.target
                    );
                }
            }
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

    fn run(&self, rebuild: bool, pi_args: &[String]) -> Result<()> {
        info!(agent = %self.agent_dir.display(), repo = %self.repo_sessions_dir.display(), "directories");
        info!(workspace = %self.workspace.display(), "starting agent");

        // Write the embedded Dockerfile to the cache directory
        let base_dockerfile = self.cache_dir.join("Dockerfile");
        fs_err::write(&base_dockerfile, DOCKERFILE).into_diagnostic()?;

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
        let status = build_cmd
            .status()
            .into_diagnostic()
            .wrap_err("failed to build base image")?;
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
        let session_id = session_id();
        let session_dir = self
            .xdg
            .create_cache_directory(format!("sessions/{session_id}"))
            .into_diagnostic()
            .wrap_err("failed to create session directory")?;

        let all_mounts: Vec<_> = self
            .config
            .merged_mounts()
            .into_iter()
            .map(|sv| sv.value)
            .collect();
        let env_vars = self.config.merged_env();
        let compose =
            generate_compose(&dockerfile, &build_context, &all_mounts, &env_vars, pi_args);
        let compose_file = session_dir.join("compose.yml");
        fs_err::write(&compose_file, &compose).into_diagnostic()?;

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
            .into_diagnostic()
            .wrap_err("failed to run docker compose up")?;
        if !status.success() {
            bail!("docker compose up failed ({})", status);
        }

        let status = docker_compose(&["attach", "agent"])?
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .into_diagnostic()
            .wrap_err("failed to attach to agent")?;

        // Always tear down, regardless of attach exit status
        let down_status = docker_compose(&["down"])?
            .status()
            .into_diagnostic()
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

/// Generate a session ID from the current process ID.
fn session_id() -> String {
    format!("{:08x}", fastrand::u32(..))
}

/// FNV-1a 64-bit hash. Deterministic across Rust toolchain versions, unlike DefaultHasher.
fn fnv1a_64(bytes: &[u8]) -> u64 {
    const BASIS: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x00000100000001B3;
    let mut hash = BASIS;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// Create a slug for a workspace path: `<dirname>-<hash>`.
fn repo_slug(workspace: &Path) -> String {
    let name = workspace
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "root".into());
    let hash = fnv1a_64(workspace.as_os_str().as_encoded_bytes());
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
    environment: Vec<String>,
    volumes: Vec<String>,
    command: Vec<String>,
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
    mounts: &[&config::ResolvedMount],
    env_vars: &[config::ScopedValue<(&str, &str)>],
    pi_args: &[String],
) -> String {
    let volumes: Vec<String> = mounts.iter().map(|m| m.to_volume_string()).collect();

    let environment: Vec<String> = env_vars
        .iter()
        .map(|sv| format!("{}={}", sv.value.0, sv.value.1))
        .collect();

    // Always pass --append-system-prompt for the ramekin container context.
    // The prompt file is written into the agent dir which is mounted at /root/.pi/agent.
    let prompt_path = "/root/.pi/agent/ramekin-prompt.md";
    let command: Vec<String> = [
        "--append-system-prompt".to_string(),
        prompt_path.to_string(),
    ]
    .into_iter()
    .chain(pi_args.iter().cloned())
    .collect();

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
                environment,
                volumes,
                command,
            },
        },
    };

    serde_yaml::to_string(&config).expect("failed to serialize compose config")
}
