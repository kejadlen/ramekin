mod config;
mod outbox;

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;
use miette::{Context, IntoDiagnostic, Result, bail, miette};
use serde::Serialize;
use tracing::{error, info, warn};
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

const PI_DOCKERFILE: &str = include_str!("../assets/Dockerfile");
const CLAUDE_DOCKERFILE: &str = include_str!("../assets/Dockerfile.claude");
const RAMEKIN_PROMPT: &str = include_str!("../assets/ramekin-prompt.md");

const VERSION: &str = env!("RAMEKIN_VERSION");

/// Container path of the rendered per-session system prompt.
const PROMPT_TARGET: &str = "/root/.ramekin/ramekin-prompt.md";

/// The `~/.claude` subdirectories that are caches and scratch, bound to
/// fresh session-scoped dirs so they don't accumulate in the persistent
/// claude state. A best-current-guess denylist; everything else persists
/// (worst case: rot) rather than vanishing (worst case: lost auth).
const CLAUDE_EPHEMERAL: &[&str] = &["statsig", "todos", "shell-snapshots", "debug"];

#[derive(Parser)]
#[command(about = "Run a coding agent (pi or Claude Code) in a containerized environment", version = VERSION)]
struct Cli {
    /// Workspace directory to mount (defaults to current directory)
    #[arg(global = true, default_value = ".")]
    workspace: PathBuf,

    /// Profile to run (a named agent + provider bundle)
    #[arg(short, long, global = true)]
    profile: Option<String>,

    #[command(subcommand)]
    command: Option<Cmd>,

    /// Extra arguments forwarded to the agent inside the container (after --)
    #[arg(last = true, global = true)]
    agent_args: Vec<String>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start a containerized agent session
    Run {
        /// Force a full image rebuild (ignores Docker layer cache)
        #[arg(long)]
        rebuild: bool,
    },
    /// Show resolved paths and mount configuration
    Config,
    /// Review config changes proposed by agents
    Outbox {
        #[command(subcommand)]
        command: OutboxCmd,
    },
    /// Generate shell completions
    Completions {
        /// Shell to generate completions for
        shell: Shell,
    },
}

#[derive(Subcommand)]
enum OutboxCmd {
    /// List pending proposals across all repos and sessions
    List,
    /// Diff proposals against the host config they were mounted from
    Diff {
        /// A single proposal (`<slug>/<session>/<path>`) or session
        /// (`<slug>/<session>`); all proposals when omitted
        entry: Option<String>,
    },
    /// Copy a proposal over its host source, after confirmation
    Apply {
        /// The proposal to apply (`<slug>/<session>/<path>`)
        entry: String,
        /// Destination for proposals that don't map back to an allowlisted
        /// agent-config entry
        #[arg(long)]
        to: Option<PathBuf>,
    },
    /// Drop proposals without applying them
    Discard {
        /// A single proposal (`<slug>/<session>/<path>`) or a whole session
        /// (`<slug>/<session>`)
        entry: String,
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

    // Outbox review is host-global: it spans repos and needs no workspace,
    // profile, or agent resolution.
    if let Cmd::Outbox { command } = command {
        return run_outbox(command);
    }

    let ramekin = Ramekin::resolve(cli.workspace, cli.profile.as_deref())?;

    match command {
        Cmd::Run { rebuild } => ramekin.run(rebuild, &cli.agent_args),
        Cmd::Config => ramekin.config(),
        Cmd::Completions { .. } | Cmd::Outbox { .. } => unreachable!(),
    }
}

// ---------------------------------------------------------------------------
// Outbox commands
// ---------------------------------------------------------------------------

fn run_outbox(command: OutboxCmd) -> Result<()> {
    let data_home = xdg::BaseDirectories::with_prefix("ramekin")
        .get_data_home()
        .ok_or_else(|| miette!("could not determine XDG data home"))?;

    match command {
        OutboxCmd::List => {
            let proposals = outbox::scan(&data_home)?;
            if proposals.is_empty() {
                println!("no pending proposals");
                return Ok(());
            }
            for p in proposals {
                match p.host_target() {
                    Some(target) => println!("{} → {}", p.entry(), target.display()),
                    None => println!("{} (no mapped target; apply needs --to)", p.entry()),
                }
            }
        }
        OutboxCmd::Diff { entry } => {
            let proposals = match entry {
                Some(entry) => outbox::find(&data_home, &entry)?,
                None => outbox::scan(&data_home)?,
            };
            for p in proposals {
                diff_proposal(&p, p.host_target().as_deref())?;
            }
        }
        OutboxCmd::Apply { entry, to } => {
            let proposals = outbox::find(&data_home, &entry)?;
            let [proposal] = proposals.as_slice() else {
                bail!(
                    "`{entry}` matches {} proposals; apply one file at a time",
                    proposals.len()
                );
            };
            let target = to.or_else(|| proposal.host_target()).ok_or_else(|| {
                miette!(
                    "`{entry}` doesn't map back to an allowlisted agent-config entry; \
                     pass an explicit destination with --to"
                )
            })?;

            diff_proposal(proposal, Some(&target))?;
            if !confirm(&format!("apply to {}?", target.display()))? {
                println!("not applied");
                return Ok(());
            }

            // Write through a symlinked host source (dotfiles), so the
            // change lands in the dotfiles working copy, not over the link.
            let dest = if target.exists() {
                target.canonicalize().into_diagnostic()?
            } else {
                if let Some(parent) = target.parent() {
                    fs_err::create_dir_all(parent).into_diagnostic()?;
                }
                target
            };
            fs_err::copy(&proposal.file, &dest).into_diagnostic()?;
            outbox::remove(&data_home, proposal)?;
            println!("applied to {}", dest.display());
        }
        OutboxCmd::Discard { entry } => {
            for proposal in outbox::find(&data_home, &entry)? {
                outbox::remove(&data_home, &proposal)?;
                println!("discarded {}", proposal.entry());
            }
        }
    }
    Ok(())
}

/// Show a proposal's diff against its host source (difftastic when
/// available, `diff -u` otherwise). A missing host source diffs against
/// /dev/null, i.e. shows the whole proposal as new.
fn diff_proposal(proposal: &outbox::Proposal, target: Option<&Path>) -> Result<()> {
    println!("--- {}", proposal.entry());
    let host: &Path = match target {
        Some(t) if t.exists() => t,
        _ => Path::new("/dev/null"),
    };
    let difft = Command::new("difft").arg(host).arg(&proposal.file).status();
    if difft.is_err() {
        // difftastic not installed; plain diff. Exit code 1 just means the
        // files differ.
        Command::new("diff")
            .arg("-u")
            .arg(host)
            .arg(&proposal.file)
            .status()
            .into_diagnostic()
            .wrap_err("failed to run diff")?;
    }
    Ok(())
}

/// Ask the user to confirm on stdin. Anything but `y`/`yes` is a no.
fn confirm(prompt: &str) -> Result<bool> {
    print!("{prompt} [y/N] ");
    std::io::stdout().flush().into_diagnostic()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer).into_diagnostic()?;
    let answer = answer.trim().to_ascii_lowercase();
    Ok(answer == "y" || answer == "yes")
}

// ---------------------------------------------------------------------------
// AgentState
// ---------------------------------------------------------------------------

/// Host-side persistent state for the active agent, and how it mounts.
///
/// The two agents get opposite persistence policies, chosen by failure mode:
/// pi is ephemeral by default with an allowlist of what persists (its
/// persistent surface is small and stable); claude persists by default with
/// a denylist of known junk (an unclassified new state file should rot, not
/// vanish along with auth or onboarding state).
enum AgentState {
    Pi {
        /// `$XDG_DATA_HOME/ramekin/agents/pi/`; holds `auth.json`, the one
        /// global file that survives across sessions.
        state_dir: PathBuf,
        /// `$XDG_DATA_HOME/ramekin/repos/<slug>/sessions/`.
        repo_sessions_dir: PathBuf,
    },
    Claude {
        /// `$XDG_DATA_HOME/ramekin/agents/claude/` → `/root/.claude`.
        /// Global across repos so OAuth tokens, account identity, and
        /// onboarding state survive switching workspaces.
        data_dir: PathBuf,
        /// `$XDG_DATA_HOME/ramekin/agents/claude.json` → `/root/.claude.json`
        /// (sibling to `~/.claude/`, not inside it). Its cwd-keyed `projects`
        /// map partitions per repo via the `/workspace/<slug>` mount, so the
        /// file itself stays global.
        state_file: PathBuf,
    },
}

impl AgentState {
    fn for_agent(agent: config::Agent, data_home: &Path, repo_slug: &str) -> Self {
        match agent {
            config::Agent::Pi => Self::Pi {
                state_dir: data_home.join("agents/pi"),
                repo_sessions_dir: data_home.join(format!("repos/{repo_slug}/sessions")),
            },
            config::Agent::Claude => Self::Claude {
                data_dir: data_home.join("agents/claude"),
                state_file: data_home.join("agents/claude.json"),
            },
        }
    }

    /// Materialize persistent host-side state. Idempotent.
    fn prepare(&self, xdg: &xdg::BaseDirectories) -> Result<()> {
        match self {
            Self::Pi {
                state_dir,
                repo_sessions_dir,
            } => {
                fs_err::create_dir_all(state_dir).into_diagnostic()?;
                fs_err::create_dir_all(repo_sessions_dir).into_diagnostic()?;

                // auth.json bind-mounts as a file, so it has to exist before
                // the container starts. Migrate from the pre-redesign
                // location (~/.config/ramekin/agent/auth.json) or start it
                // empty.
                let auth_file = state_dir.join("auth.json");
                if !auth_file.exists() {
                    let old_auth = xdg
                        .get_config_home()
                        .map(|config_home| config_home.join("agent/auth.json"))
                        .filter(|p| p.exists());
                    match old_auth {
                        Some(old) => {
                            info!(from = %old.display(), to = %auth_file.display(), "migrating pi auth");
                            fs_err::copy(&old, &auth_file).into_diagnostic()?;
                        }
                        None => init_json_file(&auth_file)?,
                    }
                }
            }
            Self::Claude {
                data_dir,
                state_file,
            } => {
                fs_err::create_dir_all(data_dir).into_diagnostic()?;
                init_json_file(state_file)?;
            }
        }
        Ok(())
    }

    /// Create the session-scoped directories this agent's mounts need.
    fn prepare_session(&self, session_dir: &Path) -> Result<()> {
        match self {
            Self::Pi { .. } => {
                fs_err::create_dir_all(session_dir.join("agent")).into_diagnostic()?;
            }
            Self::Claude { .. } => {
                for name in CLAUDE_EPHEMERAL {
                    fs_err::create_dir_all(session_dir.join("claude").join(name))
                        .into_diagnostic()?;
                }
            }
        }
        Ok(())
    }

    /// Agent-state mounts for one session. Read-only host-config mounts
    /// from the binary layer sit above these.
    fn mounts(&self, session_dir: &Path) -> Vec<config::ResolvedMount> {
        let rw = |source: PathBuf, target: String| config::ResolvedMount {
            source,
            target,
            writable: true,
        };
        match self {
            // Fresh empty writable dir per session, with the allowlisted
            // persistent pieces (auth.json, per-repo sessions/) bound on top.
            Self::Pi {
                state_dir,
                repo_sessions_dir,
            } => vec![
                rw(session_dir.join("agent"), config::PI_AGENT_DIR.into()),
                rw(
                    state_dir.join("auth.json"),
                    format!("{}/auth.json", config::PI_AGENT_DIR),
                ),
                rw(
                    repo_sessions_dir.clone(),
                    format!("{}/sessions", config::PI_AGENT_DIR),
                ),
            ],
            // Persistent state dir and state file, with fresh session-scoped
            // dirs bound over the known ephemeral subdirs.
            Self::Claude {
                data_dir,
                state_file,
            } => {
                let mut mounts = vec![
                    rw(data_dir.clone(), "/root/.claude".into()),
                    rw(state_file.clone(), "/root/.claude.json".into()),
                ];
                mounts.extend(CLAUDE_EPHEMERAL.iter().map(|name| {
                    rw(
                        session_dir.join("claude").join(name),
                        format!("/root/.claude/{name}"),
                    )
                }));
                mounts
            }
        }
    }

    /// The session-scoped dir whose discarded writes the teardown report
    /// inspects. Only pi has one: its whole agent dir is ephemeral, so a
    /// novel write there is a candidate for the persistent allowlist.
    /// Claude's session-scoped dirs are the ephemeral denylist — already
    /// classified junk, not worth reporting every run.
    fn report_dir(&self, session_dir: &Path) -> Option<PathBuf> {
        match self {
            Self::Pi { .. } => Some(session_dir.join("agent")),
            Self::Claude { .. } => None,
        }
    }

    /// Labelled host paths for `ramekin config` output.
    fn state_labels(&self) -> Vec<(&'static str, &Path)> {
        match self {
            Self::Pi {
                state_dir,
                repo_sessions_dir,
            } => vec![("pi state", state_dir), ("sessions", repo_sessions_dir)],
            Self::Claude {
                data_dir,
                state_file,
            } => vec![("claude  ", data_dir), ("state   ", state_file)],
        }
    }
}

/// Create a file containing `{}\n` unless it already exists. `create_new`
/// is race-safe: two concurrent first runs can't clobber each other, and
/// losing the race is fine — the file exists.
fn init_json_file(file: &Path) -> Result<()> {
    match fs_err::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(file)
    {
        Ok(mut f) => f.write_all(b"{}\n").into_diagnostic()?,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e).into_diagnostic(),
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Ramekin
// ---------------------------------------------------------------------------

struct Ramekin {
    workspace: PathBuf,
    /// Container path of the workspace mount: `/workspace/<slug>`. Per-repo
    /// so anything the agent keys by cwd (pi's session grouping, claude's
    /// `projects` map and transcripts) gets a distinct path per repo instead
    /// of every repo looking like the same `/workspace` project.
    workspace_target: String,
    repo_slug: String,
    xdg: xdg::BaseDirectories,
    data_home: PathBuf,
    cache_dir: PathBuf,
    custom_dockerfile: Option<PathBuf>,
    config: config::ScopedConfig,
    agent_state: AgentState,
}

impl Ramekin {
    /// Resolve all paths and load config layers. Side-effect free: nothing
    /// is created or written until `run` calls `prepare`, so `ramekin
    /// config` can inspect state without mutating it.
    fn resolve(workspace_arg: PathBuf, cli_profile: Option<&str>) -> Result<Self> {
        let workspace = workspace_arg
            .canonicalize()
            .into_diagnostic()
            .wrap_err_with(|| {
                format!("workspace path does not exist: {}", workspace_arg.display())
            })?;

        let xdg = xdg::BaseDirectories::with_prefix("ramekin");
        let data_home = xdg
            .get_data_home()
            .ok_or_else(|| miette!("could not determine XDG data home"))?;
        let cache_dir = xdg
            .get_cache_home()
            .ok_or_else(|| miette!("could not determine XDG cache home"))?;

        let repo_slug = repo_slug(&workspace);
        let workspace_target = format!("/workspace/{repo_slug}");

        let custom_dockerfile_path = workspace.join(".ramekin/Dockerfile");
        let custom_dockerfile = custom_dockerfile_path
            .is_file()
            .then_some(custom_dockerfile_path);

        let config = config::ScopedConfig::load(&workspace, &workspace_target, cli_profile)
            .wrap_err("failed to load ramekin configuration")?;

        let agent_state = AgentState::for_agent(config.agent(), &data_home, &repo_slug);

        Ok(Self {
            workspace,
            workspace_target,
            repo_slug,
            xdg,
            data_home,
            cache_dir,
            custom_dockerfile,
            config,
            agent_state,
        })
    }

    /// Session plumbing mounts shared by both agents: the rendered prompt,
    /// the outbox, and the workspace.
    fn session_mounts(&self, session_dir: &Path, outbox_dir: &Path) -> Vec<config::ResolvedMount> {
        let mut mounts = self.agent_state.mounts(session_dir);
        mounts.push(config::ResolvedMount {
            source: session_dir.join("ramekin-prompt.md"),
            target: PROMPT_TARGET.into(),
            writable: false,
        });
        mounts.push(config::ResolvedMount {
            source: outbox_dir.to_path_buf(),
            target: outbox::OUTBOX_TARGET.into(),
            writable: true,
        });
        mounts.push(config::ResolvedMount {
            source: self.workspace.clone(),
            target: self.workspace_target.clone(),
            writable: true,
        });
        mounts
    }

    /// Merge config mounts with the forced session mounts, ordered
    /// lexicographically by target so parents precede children.
    fn final_mounts<'a>(
        &'a self,
        session_mounts: &'a [config::ResolvedMount],
    ) -> Vec<&'a config::ResolvedMount> {
        let mut by_target: BTreeMap<&str, &config::ResolvedMount> = self
            .config
            .merged_mounts()
            .into_iter()
            .map(|sv| (sv.value.target.as_str(), sv.value))
            .collect();
        for mount in session_mounts {
            by_target.insert(mount.target.as_str(), mount);
        }
        by_target.into_values().collect()
    }

    /// Tag of the active agent's base image.
    fn base_image(&self) -> String {
        format!("ramekin-{}", self.config.agent())
    }

    fn config(&self) -> Result<()> {
        println!("Workspace");
        println!("  {} → {}", self.workspace.display(), self.workspace_target);

        println!();
        println!("Profile");
        println!(
            "  {} (agent {}, selected by {})",
            self.config.profile.name,
            self.config.agent(),
            self.config.selection.scope,
        );
        for (name, sv) in &self.config.profiles {
            let marker = if *name == self.config.profile.name {
                "*"
            } else {
                " "
            };
            println!("  {marker} {name} ({}, agent {})", sv.scope, sv.value.agent);
        }

        println!();
        println!("Ramekin directories");
        for (label, path) in self.agent_state.state_labels() {
            println!("  {label} {}", path.display());
        }
        println!("  cache    {}", self.cache_dir.display());

        let merged_mounts = self.config.merged_mounts();
        let merged_env = self.config.merged_env();

        let scope_label = |scope: config::Scope| -> String {
            if scope == config::Scope::Profile {
                return format!("profile ({})", self.config.profile.name);
            }
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
            let scopes: BTreeSet<_> = merged_mounts.iter().map(|sv| sv.scope).collect();
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

        // Session mounts (sources materialize per run; shown with a placeholder)
        let placeholder = self.cache_dir.join("sessions/<session>");
        let outbox_placeholder = self
            .data_home
            .join(format!("repos/{}/outbox/<session>", self.repo_slug));
        println!();
        println!("Session mounts");
        for mount in self.session_mounts(&placeholder, &outbox_placeholder) {
            println!(
                "    {} → {}",
                mount.source.display(),
                mount.display_target()
            );
        }

        // Environment
        if !merged_env.is_empty() {
            println!();
            println!("Environment");
            let scopes: BTreeSet<_> = merged_env.iter().map(|sv| sv.scope).collect();
            for scope in scopes {
                println!("  {}", scope_label(scope));
                for sv in merged_env.iter().filter(|sv| sv.scope == scope) {
                    match &sv.value.value {
                        Some(value) => println!("    {}={value}", sv.value.name),
                        None => println!("    {} (passed through from host)", sv.value.name),
                    }
                }
            }
        }

        println!();
        println!("Dockerfile");
        match &self.custom_dockerfile {
            Some(path) => println!("  ✓ {} (BASE={})", path.display(), self.base_image()),
            None => {
                println!("  embedded ({})", self.base_image());
                println!(
                    "  ✗ {} (not found)",
                    self.workspace.join(".ramekin/Dockerfile").display()
                );
            }
        }

        Ok(())
    }

    fn run(&self, rebuild: bool, agent_args: &[String]) -> Result<()> {
        let agent = self.config.agent();
        info!(
            profile = %self.config.profile.name,
            agent = %agent,
            workspace = %self.workspace.display(),
            target = %self.workspace_target,
            "starting agent"
        );

        fs_err::create_dir_all(&self.cache_dir).into_diagnostic()?;
        self.agent_state.prepare(&self.xdg)?;

        // Write the embedded Dockerfile to the cache directory, one file per
        // agent so concurrent sessions of different agents don't race.
        let dockerfile_source = match agent {
            config::Agent::Pi => PI_DOCKERFILE,
            config::Agent::Claude => CLAUDE_DOCKERFILE,
        };
        let base_dockerfile = self.cache_dir.join(format!("Dockerfile.{agent}"));
        fs_err::write(&base_dockerfile, dockerfile_source).into_diagnostic()?;

        // The base image fetches release metadata from the GitHub API at
        // build time. Pass a host token so the build doesn't get rate-limited.
        let gh_token = host_github_token();
        if gh_token.is_some() {
            info!("authenticated GitHub API for image build");
        }

        let base_image = self.base_image();
        if rebuild {
            info!("rebuilding base image (no cache)");
        } else {
            info!("building base image");
        }
        let mut build_cmd = Command::new("docker");
        build_cmd
            .args(["build", "-t", &base_image, "-f"])
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

        // Determine the final dockerfile, build context, and image tag. A
        // custom Dockerfile declares `ARG BASE` / `FROM ${BASE}` and gets the
        // active agent's base tag passed in, plus a repo- and agent-specific
        // image tag so the two never collide.
        let (dockerfile, build_context, image, build_args) = match &self.custom_dockerfile {
            Some(custom) => {
                info!("building project image from .ramekin/Dockerfile");
                (
                    custom.clone(),
                    self.workspace.clone(),
                    project_image_name(&self.repo_slug, agent),
                    BTreeMap::from([("BASE", base_image.clone())]),
                )
            }
            None => (
                base_dockerfile,
                self.cache_dir.clone(),
                base_image.clone(),
                BTreeMap::new(),
            ),
        };

        // Session-scoped: compose file, rendered prompt, and fresh agent
        // dirs, all under a random session id so concurrent runs don't
        // interfere.
        let session_id = session_id();
        let session_dir = self
            .xdg
            .create_cache_directory(format!("sessions/{session_id}"))
            .into_diagnostic()
            .wrap_err("failed to create session directory")?;
        self.agent_state.prepare_session(&session_dir)?;

        let prompt = RAMEKIN_PROMPT.replace("{{WORKSPACE_PATH}}", &self.workspace_target);
        fs_err::write(session_dir.join("ramekin-prompt.md"), prompt).into_diagnostic()?;

        let outbox_dir =
            outbox::create_session(&self.data_home, &self.repo_slug, &session_id, agent)
                .wrap_err("failed to create session outbox")?;

        let session_mounts = self.session_mounts(&session_dir, &outbox_dir);
        let all_mounts = self.final_mounts(&session_mounts);
        let env_vars = self.config.merged_env();
        let compose = generate_compose(ComposeParams {
            dockerfile: &dockerfile,
            build_context: &build_context,
            build_args,
            mounts: &all_mounts,
            env_vars: &env_vars,
            image: &image,
            working_dir: &self.workspace_target,
            prompt_flag: match agent {
                // Pi's --append-system-prompt accepts a file path; Claude's
                // takes a literal string, so it needs the -file variant to
                // read the file rather than append the literal path.
                config::Agent::Pi => "--append-system-prompt",
                config::Agent::Claude => "--append-system-prompt-file",
            },
            agent_args,
        });
        let compose_file = session_dir.join("compose.yml");
        fs_err::write(&compose_file, &compose).into_diagnostic()?;

        // Mount targets inside the agent dir show up on the host as empty
        // artifacts Docker creates to serve as mount points; the teardown
        // report has to know to skip them.
        let agent_dir_mountpoints: BTreeSet<PathBuf> = all_mounts
            .iter()
            .filter_map(|m| {
                m.target
                    .strip_prefix(&format!("{}/", config::PI_AGENT_DIR))
                    .map(PathBuf::from)
            })
            .collect();

        let project_name = format!("ramekin-{session_id}");
        let docker_compose = |args: &[&str]| -> Result<Command> {
            let mut cmd = Command::new("docker");
            cmd.args(["compose", "-f"])
                .arg(&compose_file)
                .args(["--project-name", &project_name])
                .args(args);
            Ok(cmd)
        };

        // Build the project image when a custom Dockerfile is present. `up`
        // alone only builds when the image is missing, which would serve a stale
        // layer after the Dockerfile changes. Layer caching keeps this cheap
        // unless `--rebuild` forces a clean build.
        if self.custom_dockerfile.is_some() || rebuild {
            let mut args = vec!["build"];
            if rebuild {
                args.push("--no-cache");
            }
            let status = docker_compose(&args)?
                .status()
                .into_diagnostic()
                .wrap_err("failed to run docker compose build")?;
            if !status.success() {
                bail!("docker compose build failed ({})", status);
            }
        }

        let status = docker_compose(&["up", "-d"])?
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

        // Anything the agent wrote to its session-scoped dir is about to be
        // discarded; log it so a path that deserves persistence gets noticed
        // instead of silently vanishing.
        if let Some(report_dir) = self.agent_state.report_dir(&session_dir) {
            match discarded_writes(&report_dir, &agent_dir_mountpoints) {
                Ok(paths) => {
                    for path in paths {
                        warn!(path = %path.display(), "discarding session-scoped agent write");
                    }
                }
                Err(e) => error!("failed to inspect session agent dir: {e}"),
            }
        }

        // A non-empty outbox survives teardown as pending proposals.
        match outbox::finish_session(&self.data_home, &self.repo_slug, &session_id) {
            Ok(0) => {}
            Ok(pending) => {
                info!("{pending} config proposal(s) pending — review with `ramekin outbox list`");
            }
            Err(e) => error!("failed to finalize session outbox: {e}"),
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

/// Collect every file the agent wrote into its session-scoped agent dir.
///
/// The dir starts empty, so anything found here (other than the empty
/// artifacts Docker created as mount points, listed in `mountpoints` as
/// agent-dir-relative paths) is durable-looking state the agent produced
/// that ramekin is about to throw away. Logging these is the learning loop
/// for promoting a path into the persistent set — or confirming it's junk.
fn discarded_writes(agent_dir: &Path, mountpoints: &BTreeSet<PathBuf>) -> Result<Vec<PathBuf>> {
    fn walk(
        dir: &Path,
        root: &Path,
        skip: &BTreeSet<PathBuf>,
        found: &mut Vec<PathBuf>,
    ) -> Result<()> {
        for entry in fs_err::read_dir(dir).into_diagnostic()? {
            let entry = entry.into_diagnostic()?;
            let path = entry.path();
            let rel = path
                .strip_prefix(root)
                .expect("walk stays under root")
                .to_path_buf();
            // A mount point (and everything a mount put under it) is not an
            // agent write; skip the whole subtree.
            if skip.contains(&rel) {
                continue;
            }
            if entry.file_type().into_diagnostic()?.is_dir() {
                walk(&path, root, skip, found)?;
            } else {
                found.push(rel);
            }
        }
        Ok(())
    }

    let mut found = Vec::new();
    walk(agent_dir, agent_dir, mountpoints, &mut found)?;
    found.sort();
    Ok(found)
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

/// Generate a random session ID for scoping the compose project and cache dir.
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

/// Docker image tag for a workspace's project image, built from its
/// `.ramekin/Dockerfile`. Repo- and agent-specific so it neither collides
/// with the per-agent base tags nor lets one agent's project layer shadow
/// the other's. Lowercased because Docker repository names must be lowercase.
fn project_image_name(repo_slug: &str, agent: config::Agent) -> String {
    format!("ramekin-{repo_slug}-{agent}").to_lowercase()
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
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    args: BTreeMap<&'static str, String>,
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
    build_args: BTreeMap<&'static str, String>,
    mounts: &'a [&'a config::ResolvedMount],
    env_vars: &'a [config::ScopedValue<&'a config::EnvVar>],
    image: &'a str,
    working_dir: &'a str,
    prompt_flag: &'a str,
    agent_args: &'a [String],
}

/// Generate a Docker Compose config with all volume mounts.
fn generate_compose(params: ComposeParams) -> String {
    let ComposeParams {
        dockerfile,
        build_context,
        build_args,
        mounts,
        env_vars,
        image,
        working_dir,
        prompt_flag,
        agent_args,
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

    // A bare name (no value) is compose's passthrough form: the variable is
    // forwarded from the environment ramekin runs in, and stays unset in the
    // container when the host doesn't have it either.
    let environment: Vec<String> = env_vars
        .iter()
        .map(|sv| match &sv.value.value {
            Some(value) => format!("{}={value}", sv.value.name),
            None => sv.value.name.clone(),
        })
        .collect();

    // Always pass the prompt flag for the ramekin container context.
    // User-supplied agent args come last so they can override.
    let command: Vec<String> = [prompt_flag.to_string(), PROMPT_TARGET.to_string()]
        .into_iter()
        .chain(agent_args.iter().cloned())
        .collect();

    let config = ComposeConfig {
        services: Services {
            agent: AgentService {
                build: BuildConfig {
                    context: build_context.display().to_string(),
                    dockerfile: dockerfile.display().to_string(),
                    args: build_args,
                },
                image: image.to_string(),
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

    #[test]
    fn project_image_name_is_repo_and_agent_specific() {
        let pi = project_image_name("lit-rs-deadbeef", config::Agent::Pi);
        let claude = project_image_name("lit-rs-deadbeef", config::Agent::Claude);
        // Must not collide with the per-agent base tags, or `docker compose
        // up` reuses the base instead of building the project Dockerfile.
        assert_ne!(pi, "ramekin-pi");
        assert_ne!(claude, "ramekin-claude");
        // One project Dockerfile serves both agents; the tags must differ or
        // one agent's project layer shadows the other's.
        assert_ne!(pi, claude);
        // Docker repository names must be lowercase.
        assert_eq!(pi, pi.to_lowercase(), "got: {pi}");
    }

    fn compose_params<'a>(
        mounts: &'a [&'a config::ResolvedMount],
        env_vars: &'a [config::ScopedValue<&'a config::EnvVar>],
    ) -> ComposeParams<'a> {
        ComposeParams {
            dockerfile: Path::new("/cache/Dockerfile.pi"),
            build_context: Path::new("/cache"),
            build_args: BTreeMap::new(),
            mounts,
            env_vars,
            image: "ramekin-pi",
            working_dir: "/workspace/x-1",
            prompt_flag: "--append-system-prompt",
            agent_args: &[],
        }
    }

    #[test]
    fn generate_compose_long_form_binds() {
        let mount = config::ResolvedMount {
            source: PathBuf::from("/host/.config/git"),
            target: "/root/.config/git".into(),
            writable: false,
        };
        let yaml = generate_compose(compose_params(&[&mount], &[]));
        assert!(yaml.contains("type: bind"), "{yaml}");
        assert!(yaml.contains("source: /host/.config/git"), "{yaml}");
        assert!(yaml.contains("target: /root/.config/git"), "{yaml}");
        assert!(yaml.contains("read_only: true"), "{yaml}");
        assert!(yaml.contains("working_dir: /workspace/x-1"), "{yaml}");
        // No build args → no args key at all.
        assert!(!yaml.contains("args:"), "{yaml}");
    }

    #[test]
    fn generate_compose_env_passthrough_is_a_bare_name() {
        let with_value = config::EnvVar {
            name: "FOO".into(),
            value: Some("bar".into()),
        };
        let passthrough = config::EnvVar {
            name: "GITHUB_TOKEN".into(),
            value: None,
        };
        let env = [
            config::ScopedValue {
                scope: config::Scope::User,
                value: &with_value,
            },
            config::ScopedValue {
                scope: config::Scope::Profile,
                value: &passthrough,
            },
        ];
        let yaml = generate_compose(compose_params(&[], &env));
        assert!(yaml.contains("- FOO=bar"), "{yaml}");
        assert!(yaml.contains("- GITHUB_TOKEN\n"), "{yaml}");
        assert!(!yaml.contains("GITHUB_TOKEN="), "{yaml}");
    }

    #[test]
    fn generate_compose_carries_base_build_arg() {
        let mut params = compose_params(&[], &[]);
        params.build_args = BTreeMap::from([("BASE", "ramekin-claude".to_string())]);
        params.prompt_flag = "--append-system-prompt-file";
        let yaml = generate_compose(params);
        assert!(yaml.contains("BASE: ramekin-claude"), "{yaml}");
        // Claude needs the -file variant: the plain flag would append the
        // literal path string instead of the prompt contents.
        assert!(yaml.contains("--append-system-prompt-file"), "{yaml}");
        assert!(yaml.contains(PROMPT_TARGET), "{yaml}");
    }

    #[test]
    fn discarded_writes_skips_mountpoint_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        // Mount point artifacts docker would leave behind.
        fs_err::write(dir.path().join("auth.json"), "").unwrap();
        fs_err::create_dir_all(dir.path().join("sessions")).unwrap();
        // Genuine agent writes.
        fs_err::write(dir.path().join("scratch.txt"), "x").unwrap();
        fs_err::create_dir_all(dir.path().join("cache")).unwrap();
        fs_err::write(dir.path().join("cache/blob"), "y").unwrap();

        let mountpoints: BTreeSet<PathBuf> =
            [PathBuf::from("auth.json"), PathBuf::from("sessions")]
                .into_iter()
                .collect();
        let found = discarded_writes(dir.path(), &mountpoints).unwrap();
        assert_eq!(
            found,
            vec![PathBuf::from("cache/blob"), PathBuf::from("scratch.txt")]
        );
    }

    #[test]
    fn discarded_writes_empty_dir_reports_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let found = discarded_writes(dir.path(), &BTreeSet::new()).unwrap();
        assert!(found.is_empty());
    }

    #[test]
    fn claude_state_mounts_partition_persistent_and_ephemeral() {
        let state = AgentState::Claude {
            data_dir: PathBuf::from("/data/agents/claude"),
            state_file: PathBuf::from("/data/agents/claude.json"),
        };
        let mounts = state.mounts(Path::new("/cache/sessions/abc"));
        let target = |t: &str| mounts.iter().find(|m| m.target == t);

        let data = target("/root/.claude").expect("claude data dir mount");
        assert_eq!(data.source, PathBuf::from("/data/agents/claude"));
        assert!(data.writable);

        let state_file = target("/root/.claude.json").expect("claude state file mount");
        assert_eq!(state_file.source, PathBuf::from("/data/agents/claude.json"));

        // Ephemeral denylist dirs bind session-scoped dirs over the junk.
        for name in CLAUDE_EPHEMERAL {
            let m = target(&format!("/root/.claude/{name}"))
                .unwrap_or_else(|| panic!("missing ephemeral mount for {name}"));
            assert_eq!(m.source, Path::new("/cache/sessions/abc/claude").join(name));
            assert!(m.writable);
        }
    }

    #[test]
    fn pi_state_mounts_allowlist_persistence() {
        let state = AgentState::Pi {
            state_dir: PathBuf::from("/data/agents/pi"),
            repo_sessions_dir: PathBuf::from("/data/repos/x-1/sessions"),
        };
        let mounts = state.mounts(Path::new("/cache/sessions/abc"));
        let target = |t: &str| mounts.iter().find(|m| m.target == t);

        let agent_dir = target("/root/.pi/agent").expect("session agent dir mount");
        assert_eq!(agent_dir.source, PathBuf::from("/cache/sessions/abc/agent"));

        let auth = target("/root/.pi/agent/auth.json").expect("auth mount");
        assert_eq!(auth.source, PathBuf::from("/data/agents/pi/auth.json"));

        let sessions = target("/root/.pi/agent/sessions").expect("sessions mount");
        assert_eq!(sessions.source, PathBuf::from("/data/repos/x-1/sessions"));
    }
}
