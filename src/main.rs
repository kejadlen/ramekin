mod config;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;
use miette::{Context, IntoDiagnostic, Result, bail, miette};
use serde::Serialize;
use tracing::{error, info, warn};
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
    /// Container path of the workspace mount: `/workspace/<slug>`. Per-repo
    /// so anything the agent keys by cwd (pi's session grouping) gets a
    /// distinct path per repo instead of every repo looking like the same
    /// `/workspace` project.
    workspace_target: String,
    xdg: xdg::BaseDirectories,
    /// Persistent pi state: `$XDG_DATA_HOME/ramekin/agents/pi/`. Holds
    /// `auth.json`, the one global file that survives across sessions.
    pi_state_dir: PathBuf,
    /// Per-repo session history: `$XDG_DATA_HOME/ramekin/repos/<slug>/sessions/`.
    repo_sessions_dir: PathBuf,
    cache_dir: PathBuf,
    custom_dockerfile: Option<PathBuf>,
    config: config::ScopedConfig,
}

impl Ramekin {
    /// Resolve all paths and load config layers. Side-effect free: nothing
    /// is created or written until `run` calls `prepare`, so `ramekin
    /// config` can inspect state without mutating it.
    fn resolve(workspace_arg: PathBuf) -> Result<Self> {
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
        let pi_state_dir = data_home.join("agents/pi");
        let repo_sessions_dir = data_home.join(format!("repos/{repo_slug}/sessions"));

        let custom_dockerfile_path = workspace.join(".ramekin/Dockerfile");
        let custom_dockerfile = custom_dockerfile_path
            .is_file()
            .then_some(custom_dockerfile_path);

        let config = config::ScopedConfig::load(&workspace, &workspace_target)
            .wrap_err("failed to load ramekin configuration")?;

        Ok(Self {
            workspace,
            workspace_target,
            xdg,
            pi_state_dir,
            repo_sessions_dir,
            cache_dir,
            custom_dockerfile,
            config,
        })
    }

    /// Materialize host-side state for a run: XDG directories and the
    /// persistent pi auth file.
    fn prepare(&self) -> Result<()> {
        fs_err::create_dir_all(&self.pi_state_dir).into_diagnostic()?;
        fs_err::create_dir_all(&self.repo_sessions_dir).into_diagnostic()?;
        fs_err::create_dir_all(&self.cache_dir).into_diagnostic()?;

        // auth.json bind-mounts as a file, so it has to exist before the
        // container starts. Migrate from the pre-redesign location
        // (~/.config/ramekin/agent/auth.json) or start it empty.
        let auth_file = self.pi_state_dir.join("auth.json");
        if !auth_file.exists() {
            let old_auth = self
                .xdg
                .get_config_home()
                .map(|config_home| config_home.join("agent/auth.json"));
            match old_auth.filter(|p| p.exists()) {
                Some(old) => {
                    info!(from = %old.display(), to = %auth_file.display(), "migrating pi auth");
                    fs_err::copy(&old, &auth_file).into_diagnostic()?;
                }
                None => {
                    // create_new so two concurrent first runs can't clobber
                    // each other; losing the race is fine, the file exists.
                    use std::io::Write;
                    match fs_err::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&auth_file)
                    {
                        Ok(mut f) => f.write_all(b"{}").into_diagnostic()?,
                        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                        Err(e) => return Err(e).into_diagnostic(),
                    }
                }
            }
        }

        Ok(())
    }

    /// Session plumbing and agent-state mounts. Not a config layer: these
    /// are forced, overriding any config mount at the same target.
    ///
    /// Pi state is ephemeral by default — a fresh empty writable dir per
    /// session at the agent dir, with the allowlisted persistent pieces
    /// (`auth.json`, per-repo `sessions/`) bind-mounted on top. Read-only
    /// host-config mounts from the binary layer sit above that.
    fn session_mounts(&self, session_dir: &Path) -> Vec<config::ResolvedMount> {
        let agent = |rest: &str| format!("{}/{rest}", config::PI_AGENT_DIR);
        vec![
            config::ResolvedMount {
                source: session_dir.join("agent"),
                target: config::PI_AGENT_DIR.into(),
                writable: true,
            },
            config::ResolvedMount {
                source: self.pi_state_dir.join("auth.json"),
                target: agent("auth.json"),
                writable: true,
            },
            config::ResolvedMount {
                source: self.repo_sessions_dir.clone(),
                target: agent("sessions"),
                writable: true,
            },
            config::ResolvedMount {
                source: session_dir.join("ramekin-prompt.md"),
                target: agent("ramekin-prompt.md"),
                writable: false,
            },
            config::ResolvedMount {
                source: self.workspace.clone(),
                target: self.workspace_target.clone(),
                writable: true,
            },
        ]
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

    fn config(&self) -> Result<()> {
        println!("Workspace");
        println!("  {} → {}", self.workspace.display(), self.workspace_target);

        println!();
        println!("Ramekin directories");
        println!("  pi state {}", self.pi_state_dir.display());
        println!("  sessions {}", self.repo_sessions_dir.display());
        println!("  cache    {}", self.cache_dir.display());

        let merged_mounts = self.config.merged_mounts();
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
        println!();
        println!("Session mounts");
        for mount in self.session_mounts(&placeholder) {
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
        info!(workspace = %self.workspace.display(), target = %self.workspace_target, "starting agent");

        self.prepare()?;

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

        // Determine the final dockerfile, build context, and image tag. A
        // custom Dockerfile gets a repo-specific tag so it doesn't collide with
        // the base `ramekin-agent` image it builds `FROM`; sharing the tag would
        // make `docker compose up` reuse the base instead of the project layer.
        let (dockerfile, build_context, image) = match &self.custom_dockerfile {
            Some(custom) => {
                info!("building project image from .ramekin/Dockerfile");
                (
                    custom.clone(),
                    self.workspace.clone(),
                    project_image_name(&self.workspace),
                )
            }
            None => (
                base_dockerfile,
                self.cache_dir.clone(),
                "ramekin-agent".to_string(),
            ),
        };

        // Session-scoped: compose file, rendered prompt, and a fresh empty
        // agent dir, all under a random session id so concurrent runs don't
        // interfere.
        let session_id = session_id();
        let session_dir = self
            .xdg
            .create_cache_directory(format!("sessions/{session_id}"))
            .into_diagnostic()
            .wrap_err("failed to create session directory")?;
        let session_agent_dir = session_dir.join("agent");
        fs_err::create_dir_all(&session_agent_dir).into_diagnostic()?;

        let prompt = RAMEKIN_PROMPT.replace("{{WORKSPACE_PATH}}", &self.workspace_target);
        fs_err::write(session_dir.join("ramekin-prompt.md"), prompt).into_diagnostic()?;

        let session_mounts = self.session_mounts(&session_dir);
        let all_mounts = self.final_mounts(&session_mounts);
        let env_vars = self.config.merged_env();
        let compose = generate_compose(
            &dockerfile,
            &build_context,
            &all_mounts,
            &env_vars,
            &image,
            &self.workspace_target,
            pi_args,
        );
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
        match discarded_writes(&session_agent_dir, &agent_dir_mountpoints) {
            Ok(paths) => {
                for path in paths {
                    warn!(path = %path.display(), "discarding session-scoped agent write");
                }
            }
            Err(e) => error!("failed to inspect session agent dir: {e}"),
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
/// `.ramekin/Dockerfile`. Kept distinct from the base `ramekin-agent` tag so
/// `docker compose up` builds the project layer instead of reusing the base
/// image that shares the tag. Lowercased because Docker repository names must
/// be lowercase.
fn project_image_name(workspace: &Path) -> String {
    format!("ramekin-{}", repo_slug(workspace)).to_lowercase()
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

/// Generate a Docker Compose config with all volume mounts.
fn generate_compose(
    dockerfile: &Path,
    build_context: &Path,
    mounts: &[&config::ResolvedMount],
    env_vars: &[config::ScopedValue<&config::EnvVar>],
    image: &str,
    working_dir: &str,
    pi_args: &[String],
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

    // Always pass --append-system-prompt for the ramekin container context.
    // The rendered prompt is mounted read-only into the agent dir.
    let prompt_path = format!("{}/ramekin-prompt.md", config::PI_AGENT_DIR);
    let command: Vec<String> = ["--append-system-prompt".to_string(), prompt_path]
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
    fn project_image_name_is_repo_specific_and_distinct_from_base() {
        let name = project_image_name(Path::new("/Users/alpha/src/lit-rs"));
        // Must not collide with the base image tag, or `docker compose up`
        // reuses the base instead of building the project Dockerfile.
        assert_ne!(name, "ramekin-agent");
        assert!(name.contains("lit-rs"), "got: {name}");
        // Docker repository names must be lowercase.
        assert_eq!(name, name.to_lowercase(), "got: {name}");
    }

    #[test]
    fn project_image_name_differs_per_workspace() {
        let a = project_image_name(Path::new("/Users/alpha/src/lit-rs"));
        let b = project_image_name(Path::new("/Users/alpha/src/ramekin"));
        assert_ne!(a, b);
    }

    #[test]
    fn generate_compose_uses_supplied_image_tag() {
        let yaml = generate_compose(
            Path::new("/ws/.ramekin/Dockerfile"),
            Path::new("/ws"),
            &[],
            &[],
            "ramekin-agent-lit-rs-deadbeef",
            "/workspace/lit-rs-deadbeef",
            &[],
        );
        assert!(
            yaml.contains("image: ramekin-agent-lit-rs-deadbeef"),
            "compose did not carry the supplied image tag:\n{yaml}"
        );
        assert!(
            yaml.contains("working_dir: /workspace/lit-rs-deadbeef"),
            "compose did not set working_dir:\n{yaml}"
        );
    }

    #[test]
    fn generate_compose_long_form_binds() {
        let mount = config::ResolvedMount {
            source: PathBuf::from("/host/.config/git"),
            target: "/root/.config/git".into(),
            writable: false,
        };
        let yaml = generate_compose(
            Path::new("/cache/Dockerfile"),
            Path::new("/cache"),
            &[&mount],
            &[],
            "ramekin-agent",
            "/workspace/x-1",
            &[],
        );
        assert!(yaml.contains("type: bind"), "{yaml}");
        assert!(yaml.contains("source: /host/.config/git"), "{yaml}");
        assert!(yaml.contains("target: /root/.config/git"), "{yaml}");
        assert!(yaml.contains("read_only: true"), "{yaml}");
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
}
