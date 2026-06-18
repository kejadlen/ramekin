mod config;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;
use miette::{Context, IntoDiagnostic, Result, bail, miette};
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

/// Resolved on-host paths and container layout for the chosen agent.
///
/// Two constructors ([`AgentLayout::for_pi`], [`AgentLayout::for_claude`])
/// produce a fully-populated layout; the rest of the program reads fields
/// directly. Anything genuinely agent-specific (e.g. Pi's per-run agent-dir
/// reassembly) lives in an `Option` field rather than a variant arm.
struct AgentLayout {
    agent: config::Agent,
    /// Embedded Dockerfile for this agent's base image.
    dockerfile: &'static str,
    /// Container path where the system-prompt file is mounted (passed to
    /// the agent via `prompt_flag`).
    prompt_path_in_container: &'static str,
    /// Flag the agent reads the prompt file with. Pi's
    /// `--append-system-prompt` accepts text or a file path; Claude
    /// distinguishes the two, so it needs `--append-system-prompt-file` to
    /// read the file rather than append the literal path string.
    prompt_flag: &'static str,
    /// Host path where the prompt file is written so it lands at
    /// `prompt_path_in_container` inside the container.
    prompt_host_path: PathBuf,
    /// Default flags ramekin always passes to the agent, before any
    /// user-supplied `agent_args`. Currently empty for both agents; Claude's
    /// yolo mode is now expressed via managed-settings baked into the image.
    default_args: &'static [&'static str],
    /// Container path the workspace is mounted at; the agent's working dir.
    /// Pi keeps the simple `/workspace`; Claude uses `/workspace/<repo_slug>`
    /// so its cwd-keyed `projects` map doesn't collide across host repos.
    workspace_target_in_container: String,
    /// All builtin mounts ramekin always injects: agent state plus the
    /// workspace mount.
    mounts: Vec<config::ResolvedMount>,
    /// Directories `prepare` must ensure exist.
    dirs_to_create: Vec<PathBuf>,
    /// Files that must exist before container start. Docker bind-mounts of
    /// files require the host file to exist; otherwise it creates a
    /// directory with that name. Each is created with `{}\n` if missing.
    bind_files_to_init: Vec<PathBuf>,
    /// Pi-specific: directory cleared and reassembled from KDL pi entries
    /// each run. `None` for agents that don't need this.
    pi_agent_dir: Option<PathBuf>,
    /// Labelled host paths to display in the `config` subcommand output.
    state_dirs: Vec<(&'static str, PathBuf)>,
}

impl AgentLayout {
    /// Resolve the layout for the chosen agent. Pure: no directories
    /// created, no files written. Use [`Self::prepare`] to materialize
    /// on-disk state before launching a container.
    fn for_agent(
        agent: config::Agent,
        xdg: &xdg::BaseDirectories,
        repo_slug: &str,
        workspace: &Path,
    ) -> Result<Self> {
        match agent {
            config::Agent::Pi => Self::for_pi(xdg, repo_slug, workspace),
            config::Agent::Claude => Self::for_claude(xdg, repo_slug, workspace),
        }
    }

    fn for_pi(xdg: &xdg::BaseDirectories, repo_slug: &str, workspace: &Path) -> Result<Self> {
        let data_home = xdg
            .get_data_home()
            .ok_or_else(|| miette!("could not determine XDG data home"))?;
        let config_home = xdg
            .get_config_home()
            .ok_or_else(|| miette!("could not determine XDG config home"))?;

        // `~/.pi/agent` inside the container; cleared and reassembled per run.
        let agent_dir = config_home.join("agent");
        // `~/.pi`; holds auth and global pi state.
        let pi_data_dir = data_home.clone();
        // `~/.pi/agent/sessions`; one per host repo.
        let repo_sessions_dir = data_home.join(format!("repos/{repo_slug}/sessions"));
        let workspace_target = "/workspace".to_string();

        let mounts = build_mounts([
            (pi_data_dir.clone(), "/root/.pi".to_string()),
            (agent_dir.clone(), "/root/.pi/agent".to_string()),
            (
                repo_sessions_dir.clone(),
                "/root/.pi/agent/sessions".to_string(),
            ),
            (workspace.to_path_buf(), workspace_target.clone()),
        ]);

        Ok(Self {
            agent: config::Agent::Pi,
            dockerfile: PI_DOCKERFILE,
            prompt_path_in_container: "/root/.pi/agent/ramekin-prompt.md",
            prompt_flag: "--append-system-prompt",
            prompt_host_path: agent_dir.join("ramekin-prompt.md"),
            default_args: &[],
            workspace_target_in_container: workspace_target,
            mounts,
            dirs_to_create: vec![
                agent_dir.clone(),
                pi_data_dir.clone(),
                repo_sessions_dir.clone(),
            ],
            bind_files_to_init: vec![],
            pi_agent_dir: Some(agent_dir.clone()),
            state_dirs: vec![
                ("agent   ", agent_dir),
                ("data    ", pi_data_dir),
                ("sessions", repo_sessions_dir),
            ],
        })
    }

    fn for_claude(xdg: &xdg::BaseDirectories, repo_slug: &str, workspace: &Path) -> Result<Self> {
        let data_home = xdg
            .get_data_home()
            .ok_or_else(|| miette!("could not determine XDG data home"))?;

        // `~/.claude` (dir) — persistent settings, auth, history. Global
        // across repos so OAuth tokens, account identity, and onboarding
        // state survive switching workspaces.
        let claude_data_dir = data_home.join("agents/claude");
        // `~/.claude.json` (file) — sibling to `~/.claude/`, not inside it.
        // Holds account identity (`oauthAccount`, `userID`), onboarding
        // flags, and a `projects` map keyed by absolute cwd. Per-repo
        // isolation comes from mounting each workspace at a distinct
        // `/workspace/<slug>` rather than splitting the file.
        let claude_state_file = data_home.join("agents/claude.json");
        let workspace_target = format!("/workspace/{repo_slug}");

        let mounts = build_mounts([
            (claude_data_dir.clone(), "/root/.claude".to_string()),
            (claude_state_file.clone(), "/root/.claude.json".to_string()),
            (workspace.to_path_buf(), workspace_target.clone()),
        ]);

        Ok(Self {
            agent: config::Agent::Claude,
            dockerfile: CLAUDE_DOCKERFILE,
            prompt_path_in_container: "/root/.claude/ramekin-prompt.md",
            prompt_flag: "--append-system-prompt-file",
            prompt_host_path: claude_data_dir.join("ramekin-prompt.md"),
            default_args: &[],
            workspace_target_in_container: workspace_target,
            mounts,
            dirs_to_create: vec![claude_data_dir.clone()],
            bind_files_to_init: vec![claude_state_file.clone()],
            pi_agent_dir: None,
            state_dirs: vec![
                ("claude  ", claude_data_dir),
                ("state   ", claude_state_file),
            ],
        })
    }

    /// Materialize on-disk state described by this layout: create
    /// directories and ensure bind-target files exist. Idempotent.
    fn prepare(&self) -> Result<()> {
        for dir in &self.dirs_to_create {
            fs_err::create_dir_all(dir).into_diagnostic()?;
        }
        for file in &self.bind_files_to_init {
            if let Some(parent) = file.parent() {
                fs_err::create_dir_all(parent).into_diagnostic()?;
            }
            // `create_new` is race-safe — concurrent runs in the same repo
            // can't overwrite an existing file with `{}\n`.
            match fs_err::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(file)
            {
                Ok(mut f) => f.write_all(b"{}\n").into_diagnostic()?,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e).into_diagnostic(),
            }
        }
        Ok(())
    }
}

/// Convenience: turn `(source, target)` pairs into writable bind mounts.
fn build_mounts<I>(entries: I) -> Vec<config::ResolvedMount>
where
    I: IntoIterator<Item = (PathBuf, String)>,
{
    entries
        .into_iter()
        .map(|(source, target)| config::ResolvedMount {
            source,
            target,
            writable: true,
        })
        .collect()
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
    /// Compute all paths and the merged config layers. Pure: no directories
    /// created, no files written, no agent state mutated. `ramekin config`
    /// reads from the resolved value to print state without touching it.
    /// `ramekin run` calls [`Self::prepare`] afterwards to materialize the
    /// on-disk state.
    fn resolve(workspace_arg: PathBuf) -> Result<Self> {
        let workspace = workspace_arg
            .canonicalize()
            .into_diagnostic()
            .wrap_err_with(|| {
                format!("workspace path does not exist: {}", workspace_arg.display())
            })?;

        let xdg = xdg::BaseDirectories::with_prefix("ramekin");

        let cache_dir = xdg
            .get_cache_home()
            .ok_or_else(|| miette!("could not determine XDG cache home"))?;

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
        let layout = AgentLayout::for_agent(agent, &xdg, &repo_slug, &workspace)?;

        // Expand `claude { ... }` entries into bind mounts before the builtin
        // layer is appended, so synthesized mounts inherit the user/project
        // scope they came from. (No-op for pi.)
        let mut pre_config = pre_config;
        if layout.agent == config::Agent::Claude {
            pre_config.expand_claude_mounts();
        }
        let config = pre_config.with_builtin(layout.mounts.clone());

        Ok(Self {
            workspace,
            xdg,
            cache_dir,
            custom_dockerfile,
            config,
            layout,
        })
    }

    /// Materialize all on-disk state needed for `run`. Creates XDG
    /// directories, ensures the Claude state file exists, clears and
    /// re-assembles the pi agent dir, and writes the system prompt.
    /// Idempotent.
    fn prepare(&self) -> Result<()> {
        fs_err::create_dir_all(&self.cache_dir).into_diagnostic()?;
        self.layout.prepare()?;

        // Pi assembles its agent config dir per run by clearing and copying
        // from KDL pi entries. Claude uses bind mounts.
        if let Some(agent_dir) = &self.layout.pi_agent_dir {
            config::clear_agent_dir(agent_dir).wrap_err("failed to clear agent directory")?;
            let resolved_pi: Vec<config::ResolvedPiEntry> = self
                .config
                .merged_pi()
                .iter()
                .map(|sv| sv.value.resolve())
                .collect();
            config::assemble_pi(agent_dir, &resolved_pi)
                .wrap_err("failed to assemble pi config")?;
        }

        // Write the system prompt where the agent will pick it up via
        // `prompt_flag`. Substitute the agent-specific workspace path so the
        // prompt tells the agent the right cwd.
        let prompt = RAMEKIN_PROMPT.replace(
            "{{WORKSPACE_PATH}}",
            &self.layout.workspace_target_in_container,
        );
        fs_err::write(&self.layout.prompt_host_path, prompt).into_diagnostic()?;
        Ok(())
    }

    fn config(&self) -> Result<()> {
        println!("Workspace");
        println!("  {}", self.workspace.display());

        println!();
        println!("Agent");
        println!("  {}", self.layout.agent);

        println!();
        println!("Ramekin directories");
        for (label, path) in &self.layout.state_dirs {
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
        if !merged_pi.is_empty() && self.layout.agent == config::Agent::Pi {
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
        if !merged_claude.is_empty() && self.layout.agent == config::Agent::Claude {
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
        let base_label = format!("embedded ({})", self.layout.agent);
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
        info!(agent = %self.layout.agent, workspace = %self.workspace.display(), "starting agent");

        // Materialize cache and agent-state directories, write prompt, etc.
        // Deferred from `resolve` so `ramekin config` stays read-only.
        self.prepare()?;

        // Write the embedded Dockerfile to the cache directory
        let base_dockerfile = self.cache_dir.join("Dockerfile");
        fs_err::write(&base_dockerfile, self.layout.dockerfile).into_diagnostic()?;

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
        let compose = generate_compose(ComposeParams {
            dockerfile: &dockerfile,
            build_context: &build_context,
            mounts: &all_mounts,
            env_vars: &env_vars,
            prompt_flag: self.layout.prompt_flag,
            prompt_path: self.layout.prompt_path_in_container,
            default_args: self.layout.default_args,
            agent_args,
            working_dir: &self.layout.workspace_target_in_container,
        });
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
    working_dir: String,
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

/// Inputs for [`generate_compose`], grouped so the container command,
/// mounts, environment, and build context travel together instead of as a
/// long positional argument list.
struct ComposeParams<'a> {
    dockerfile: &'a Path,
    build_context: &'a Path,
    mounts: &'a [&'a config::ResolvedMount],
    env_vars: &'a [config::ScopedValue<(&'a str, &'a str)>],
    prompt_flag: &'a str,
    prompt_path: &'a str,
    default_args: &'a [&'a str],
    agent_args: &'a [String],
    working_dir: &'a str,
}

/// Generate a Docker Compose config with all volume mounts.
fn generate_compose(params: ComposeParams) -> String {
    let ComposeParams {
        dockerfile,
        build_context,
        mounts,
        env_vars,
        prompt_flag,
        prompt_path,
        default_args,
        agent_args,
        working_dir,
    } = params;

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

    // Always pass the prompt flag for the ramekin container context,
    // preceded by any agent-specific defaults. User-supplied agent_args come
    // last so they can override.
    let command: Vec<String> = default_args
        .iter()
        .map(|s| (*s).to_string())
        .chain([prompt_flag.to_string(), prompt_path.to_string()])
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
                working_dir: working_dir.to_string(),
                environment,
                volumes,
                command,
            },
        },
    };

    serde_yaml::to_string(&config).expect("failed to serialize compose config")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_dirs() -> xdg::BaseDirectories {
        xdg::BaseDirectories::with_prefix("ramekin")
    }

    // Pi's `--append-system-prompt` reads text or a file path, so passing the
    // prompt file's path appends its contents.
    #[test]
    fn for_pi_appends_the_prompt_as_a_path() {
        let layout = AgentLayout::for_pi(&base_dirs(), "owner/repo", Path::new("/tmp/ws")).unwrap();
        assert_eq!(layout.prompt_flag, "--append-system-prompt");
    }

    // Claude's `--append-system-prompt` takes a literal string; only
    // `--append-system-prompt-file` reads the file. Passing the path to the
    // plain flag would append the literal path text and drop the prompt — the
    // regression this guards against.
    #[test]
    fn for_claude_appends_the_prompt_as_a_file() {
        let layout =
            AgentLayout::for_claude(&base_dirs(), "owner/repo", Path::new("/tmp/ws")).unwrap();
        assert_eq!(layout.prompt_flag, "--append-system-prompt-file");
    }

    // The flag and path are threaded into the agent command verbatim.
    #[test]
    fn generate_compose_uses_the_given_prompt_flag() {
        let compose = generate_compose(ComposeParams {
            dockerfile: Path::new("Dockerfile"),
            build_context: Path::new("."),
            mounts: &[],
            env_vars: &[],
            prompt_flag: "--append-system-prompt-file",
            prompt_path: "/root/.claude/ramekin-prompt.md",
            default_args: &[],
            agent_args: &[],
            working_dir: "/workspace/repo",
        });
        assert!(compose.contains("--append-system-prompt-file"));
        assert!(compose.contains("/root/.claude/ramekin-prompt.md"));
    }
}
