mod config;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;
use miette::{Context, IntoDiagnostic, Result, bail};
use serde::Serialize;
use tracing::{error, info};
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

const PI_DOCKERFILE: &str = include_str!("../assets/Dockerfile");
const CLAUDE_DOCKERFILE: &str = include_str!("../assets/Dockerfile.claude");
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

    /// Extra arguments forwarded to the agent inside the container (after --)
    #[arg(last = true, global = true)]
    agent_args: Vec<String>,
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
        Cmd::Run { rebuild } => ramekin.run(rebuild, &cli.agent_args),
        Cmd::Config => ramekin.config(),
        Cmd::Completions { .. } => unreachable!(),
    }
}

// ---------------------------------------------------------------------------
// AgentLayout
// ---------------------------------------------------------------------------

/// Per-agent host paths and container layout.
///
/// Each variant carries everything ramekin needs to set up the chosen
/// agent's container: state directories on the host, embedded Dockerfile
/// content, and the path the system-prompt file should land at inside
/// the container.
enum AgentLayout {
    Pi {
        /// `~/.pi/agent` inside the container; cleared and reassembled per run.
        agent_dir: PathBuf,
        /// `~/.pi` inside the container; holds auth and global pi state.
        pi_data_dir: PathBuf,
        /// `~/.pi/agent/sessions` inside the container; one per host repo.
        repo_sessions_dir: PathBuf,
    },
    Claude {
        /// `~/.claude` inside the container; persistent settings, auth, history.
        claude_data_dir: PathBuf,
        /// `~/.claude/projects/-workspace` inside the container; one per host repo.
        ///
        /// Claude Code keys session history off the cwd; since every workspace
        /// mounts at `/workspace`, the encoded cwd path is always `-workspace`,
        /// and we have to split per-repo on the host side to avoid collisions.
        claude_projects_dir: PathBuf,
        /// `~/.claude.json` inside the container — sibling to `~/.claude/`,
        /// not inside it. One per host repo.
        ///
        /// Claude Code's global state file has a top-level `projects` map
        /// keyed by absolute cwd. Without a per-repo split every host repo
        /// would share the same `/workspace` entry — including granted
        /// permissions and prompt history. Each repo gets its own file.
        claude_state_file: PathBuf,
    },
}

impl AgentLayout {
    fn for_agent(
        agent: config::Agent,
        xdg: &xdg::BaseDirectories,
        repo_slug: &str,
    ) -> Result<Self> {
        match agent {
            config::Agent::Pi => Ok(Self::Pi {
                agent_dir: xdg
                    .create_config_directory("agent")
                    .into_diagnostic()
                    .wrap_err("failed to create agent config directory")?,
                pi_data_dir: xdg
                    .create_data_directory("")
                    .into_diagnostic()
                    .wrap_err("failed to create pi data directory")?,
                repo_sessions_dir: xdg
                    .create_data_directory(format!("repos/{repo_slug}/sessions"))
                    .into_diagnostic()
                    .wrap_err("failed to create repo sessions directory")?,
            }),
            config::Agent::Claude => {
                let claude_data_dir = xdg
                    .create_data_directory("agents/claude")
                    .into_diagnostic()
                    .wrap_err("failed to create claude data directory")?;
                let claude_projects_dir = xdg
                    .create_data_directory(format!("repos/{repo_slug}/claude-projects"))
                    .into_diagnostic()
                    .wrap_err("failed to create claude projects directory")?;
                let claude_state_file = xdg
                    .place_data_file(format!("repos/{repo_slug}/claude.json"))
                    .into_diagnostic()
                    .wrap_err("failed to determine claude state file path")?;
                // Docker bind-mounts of files require the host file to exist;
                // otherwise it creates a directory with that name.
                if !claude_state_file.exists() {
                    fs_err::write(&claude_state_file, "{}\n").into_diagnostic()?;
                }
                Ok(Self::Claude {
                    claude_data_dir,
                    claude_projects_dir,
                    claude_state_file,
                })
            }
        }
    }

    fn agent(&self) -> config::Agent {
        match self {
            Self::Pi { .. } => config::Agent::Pi,
            Self::Claude { .. } => config::Agent::Claude,
        }
    }

    fn dockerfile_content(&self) -> &'static str {
        match self {
            Self::Pi { .. } => PI_DOCKERFILE,
            Self::Claude { .. } => CLAUDE_DOCKERFILE,
        }
    }

    /// Container path the prompt file is mounted at (passed to the agent
    /// via `--append-system-prompt`).
    fn prompt_path_in_container(&self) -> &'static str {
        match self {
            Self::Pi { .. } => "/root/.pi/agent/ramekin-prompt.md",
            Self::Claude { .. } => "/root/.claude/ramekin-prompt.md",
        }
    }

    /// Host path where the prompt file should be written so it lands at
    /// `prompt_path_in_container` inside the container.
    fn prompt_host_path(&self) -> PathBuf {
        match self {
            Self::Pi { agent_dir, .. } => agent_dir.join("ramekin-prompt.md"),
            Self::Claude {
                claude_data_dir, ..
            } => claude_data_dir.join("ramekin-prompt.md"),
        }
    }

    /// Builtin mounts that ramekin always injects for this agent.
    fn builtin_mounts(&self, workspace: &Path) -> Vec<config::ResolvedMount> {
        let mut entries: Vec<(PathBuf, &str)> = match self {
            Self::Pi {
                agent_dir,
                pi_data_dir,
                repo_sessions_dir,
            } => vec![
                (pi_data_dir.clone(), "/root/.pi"),
                (agent_dir.clone(), "/root/.pi/agent"),
                (repo_sessions_dir.clone(), "/root/.pi/agent/sessions"),
            ],
            Self::Claude {
                claude_data_dir,
                claude_projects_dir,
                claude_state_file,
            } => vec![
                (claude_data_dir.clone(), "/root/.claude"),
                (
                    claude_projects_dir.clone(),
                    "/root/.claude/projects/-workspace",
                ),
                (claude_state_file.clone(), "/root/.claude.json"),
            ],
        };
        entries.push((workspace.to_path_buf(), "/workspace"));
        entries
            .into_iter()
            .map(|(source, target)| config::ResolvedMount {
                source,
                target: target.into(),
                writable: true,
            })
            .collect()
    }

    /// Labelled host paths to display in the `config` subcommand output.
    fn state_dirs(&self) -> Vec<(&'static str, &Path)> {
        match self {
            Self::Pi {
                agent_dir,
                pi_data_dir,
                repo_sessions_dir,
            } => vec![
                ("agent   ", agent_dir),
                ("data    ", pi_data_dir),
                ("sessions", repo_sessions_dir),
            ],
            Self::Claude {
                claude_data_dir,
                claude_projects_dir,
                claude_state_file,
            } => vec![
                ("claude  ", claude_data_dir),
                ("projects", claude_projects_dir),
                ("state   ", claude_state_file),
            ],
        }
    }
}

// ---------------------------------------------------------------------------
// Ramekin
// ---------------------------------------------------------------------------

struct Ramekin {
    workspace: PathBuf,
    xdg: xdg::BaseDirectories,
    cache_dir: PathBuf,
    custom_dockerfile: Option<PathBuf>,
    config: config::ScopedConfig,
    layout: AgentLayout,
}

impl Ramekin {
    /// Resolve all paths, create XDG directories, assemble agent config,
    /// and resolve mounts.
    fn resolve(workspace_arg: PathBuf) -> Result<Self> {
        let workspace = workspace_arg
            .canonicalize()
            .into_diagnostic()
            .wrap_err_with(|| {
                format!("workspace path does not exist: {}", workspace_arg.display())
            })?;

        let xdg = xdg::BaseDirectories::with_prefix("ramekin");

        let cache_dir = xdg
            .create_cache_directory("")
            .into_diagnostic()
            .wrap_err("failed to create cache directory")?;

        let custom_dockerfile_path = workspace.join(".ramekin/Dockerfile");
        let custom_dockerfile = custom_dockerfile_path
            .is_file()
            .then_some(custom_dockerfile_path);

        // Load user/project layers first so we can read the effective agent
        // before deciding which builtin mounts to inject.
        let pre_config =
            config::Config::load(&workspace).wrap_err("failed to load ramekin configuration")?;
        let agent = pre_config.effective_agent();

        let repo_slug = repo_slug(&workspace);
        let layout = AgentLayout::for_agent(agent, &xdg, &repo_slug)?;

        // Expand `claude { ... }` entries into bind mounts before the builtin
        // layer is appended, so synthesized mounts inherit the user/project
        // scope they came from. (No-op for pi.)
        let mut pre_config = pre_config;
        if matches!(layout, AgentLayout::Claude { .. }) {
            pre_config.expand_claude_mounts();
        }
        let config = pre_config.with_builtin(layout.builtin_mounts(&workspace));

        // Pi assembles its agent config dir per run by clearing and copying
        // from KDL pi entries. Claude uses bind mounts (handled above).
        if let AgentLayout::Pi { agent_dir, .. } = &layout {
            config::clear_agent_dir(agent_dir).wrap_err("failed to clear agent directory")?;
            let resolved_pi: Vec<config::ResolvedPiEntry> = config
                .merged_pi()
                .iter()
                .map(|sv| sv.value.resolve())
                .collect();
            config::assemble_pi(agent_dir, &resolved_pi)
                .wrap_err("failed to assemble pi config")?;
        }

        // Write the system prompt where the agent will pick it up via
        // --append-system-prompt.
        fs_err::write(layout.prompt_host_path(), RAMEKIN_PROMPT).into_diagnostic()?;

        Ok(Self {
            workspace,
            xdg,
            cache_dir,
            custom_dockerfile,
            config,
            layout,
        })
    }

    fn config(&self) -> Result<()> {
        println!("Workspace");
        println!("  {}", self.workspace.display());

        println!();
        println!("Agent");
        println!("  {}", self.layout.agent());

        println!();
        println!("Ramekin directories");
        for (label, path) in self.layout.state_dirs() {
            println!("  {label} {}", path.display());
        }
        println!("  cache    {}", self.cache_dir.display());

        let merged_mounts = self.config.merged_mounts();
        let merged_pi = self.config.merged_pi();
        let merged_claude = self.config.merged_claude();
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

        // Pi config (shown only when pi is the active agent; entries are inert otherwise)
        if !merged_pi.is_empty() && self.layout.agent() == config::Agent::Pi {
            println!();
            println!("Pi config");
            let scopes: std::collections::BTreeSet<_> =
                merged_pi.iter().map(|sv| sv.scope).collect();
            for scope in scopes {
                println!("  {}", scope_label(scope));
                for sv in merged_pi.iter().filter(|sv| sv.scope == scope) {
                    let resolved = sv.value.resolve();
                    let kind = entry_kind(&resolved.source);
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

        // Claude config (shown only when claude is the active agent)
        if !merged_claude.is_empty() && self.layout.agent() == config::Agent::Claude {
            println!();
            println!("Claude config");
            let scopes: std::collections::BTreeSet<_> =
                merged_claude.iter().map(|sv| sv.scope).collect();
            for scope in scopes {
                println!("  {}", scope_label(scope));
                for sv in merged_claude.iter().filter(|sv| sv.scope == scope) {
                    let host = PathBuf::from(shellexpand::tilde(&sv.value.source).as_ref());
                    let kind = entry_kind(&host);
                    let marker = if host.exists() { "✓" } else { "✗" };
                    let suffix = if sv.value.writable { "" } else { " (ro)" };
                    println!(
                        "    {marker} {} → {}{suffix} ({kind})",
                        host.display(),
                        sv.value.target_in_claude_dir(),
                    );
                }
            }
        }

        println!();
        println!("Dockerfile");
        let base_label = format!("embedded ({})", self.layout.agent());
        match &self.custom_dockerfile {
            Some(path) => {
                println!("  ✓ {} (FROM ramekin-agent: {base_label})", path.display());
            }
            None => {
                println!("  {base_label}");
                println!(
                    "  ✗ {} (not found)",
                    self.workspace.join(".ramekin/Dockerfile").display()
                );
            }
        }

        Ok(())
    }

    fn run(&self, rebuild: bool, agent_args: &[String]) -> Result<()> {
        info!(agent = %self.layout.agent(), workspace = %self.workspace.display(), "starting agent");

        // Write the embedded Dockerfile to the cache directory
        let base_dockerfile = self.cache_dir.join("Dockerfile");
        fs_err::write(&base_dockerfile, self.layout.dockerfile_content()).into_diagnostic()?;

        // The base image fetches release metadata from the GitHub API at build
        // time. Pass a host token so the build doesn't get rate-limited.
        let gh_token = host_github_token();
        if gh_token.is_some() {
            info!("authenticated GitHub API for image build");
        }

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
        if let Some(token) = &gh_token {
            build_cmd
                .env("RAMEKIN_GH_TOKEN", token)
                .args(["--secret", "id=github-token,env=RAMEKIN_GH_TOKEN"]);
        }
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
        let compose = generate_compose(
            &dockerfile,
            &build_context,
            &all_mounts,
            &env_vars,
            self.layout.prompt_path_in_container(),
            agent_args,
        );
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

/// Classify a host path for `ramekin config` display.
fn entry_kind(path: &Path) -> &'static str {
    if path.is_dir() {
        "dir"
    } else if path.is_file() {
        "file"
    } else {
        "missing"
    }
}

/// Look up a GitHub token from the host environment for build-time API calls.
///
/// Tries env vars first, then falls back to `gh auth token`. Returns `None`
/// if no token is available; the build degrades to anonymous API access.
fn host_github_token() -> Option<String> {
    for var in ["RAMEKIN_GH_TOKEN", "GITHUB_TOKEN", "GH_TOKEN"] {
        if let Ok(v) = std::env::var(var)
            && !v.is_empty()
        {
            return Some(v);
        }
    }
    let output = Command::new("gh").args(["auth", "token"]).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let token = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (!token.is_empty()).then_some(token)
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
    volumes: Vec<VolumeBind>,
    command: Vec<String>,
}

#[derive(Serialize)]
struct BuildConfig {
    context: String,
    dockerfile: String,
}

/// Long-form compose bind mount. Avoids the `source:target[:ro]` short form,
/// which can't represent paths containing colons.
#[derive(Serialize)]
struct VolumeBind {
    #[serde(rename = "type")]
    kind: &'static str,
    source: String,
    target: String,
    read_only: bool,
}

/// Generate a Docker Compose config with all volume mounts.
fn generate_compose(
    dockerfile: &Path,
    build_context: &Path,
    mounts: &[&config::ResolvedMount],
    env_vars: &[config::ScopedValue<(&str, &str)>],
    prompt_path: &str,
    agent_args: &[String],
) -> String {
    let volumes: Vec<VolumeBind> = mounts
        .iter()
        .map(|m| VolumeBind {
            kind: "bind",
            source: m.source.display().to_string(),
            target: m.target.clone(),
            read_only: !m.writable,
        })
        .collect();

    let environment: Vec<String> = env_vars
        .iter()
        .map(|sv| format!("{}={}", sv.value.0, sv.value.1))
        .collect();

    // Always pass --append-system-prompt for the ramekin container context.
    let command: Vec<String> = [
        "--append-system-prompt".to_string(),
        prompt_path.to_string(),
    ]
    .into_iter()
    .chain(agent_args.iter().cloned())
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
