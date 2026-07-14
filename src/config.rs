use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

use kdl::{KdlDocument, KdlNode};
use miette::{Context, IntoDiagnostic, Result, bail, miette};

/// Configuration scope, ordered from lowest to highest precedence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Scope {
    /// Compiled into the binary: staples, host agent-config mounts, and the
    /// trivial profiles.
    Binary,
    /// The active profile's own env and mounts, overlaid by every file layer.
    Profile,
    /// The user layer: every `*.kdl` in `~/.config/ramekin/`, merged.
    User,
    /// Project-level `<workspace>/.ramekin/config.kdl`, committed.
    Project,
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Binary => write!(f, "binary"),
            Self::Profile => write!(f, "profile"),
            Self::User => write!(f, "user"),
            Self::Project => write!(f, "project"),
        }
    }
}

/// The coding agent a session runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agent {
    Pi,
    Claude,
}

impl Agent {
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "pi" => Ok(Self::Pi),
            "claude" => Ok(Self::Claude),
            other => bail!("unknown agent `{other}` (expected `pi` or `claude`)"),
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::Pi => "pi",
            Self::Claude => "claude",
        }
    }

    /// Host directory where this agent keeps its config (and, mixed in with
    /// it, runtime state that must not enter the container).
    pub fn host_config_dir(&self) -> &'static str {
        match self {
            Self::Pi => "~/.pi/agent",
            Self::Claude => "~/.claude",
        }
    }

    /// The config-shaped entries of the host agent dir. Only these mount
    /// into the container (read-only); the rest is host runtime state —
    /// credentials, transcripts, caches. Skip-if-missing makes
    /// over-inclusion cheap.
    pub fn config_allowlist(&self) -> &'static [&'static str] {
        match self {
            Self::Pi => &["AGENTS.md", "skills"],
            Self::Claude => &[
                "CLAUDE.md",
                "settings.json",
                "skills",
                "agents",
                "commands",
                "hooks",
            ],
        }
    }

    /// The agent's config dir inside the container.
    pub fn container_config_dir(&self) -> &'static str {
        match self {
            Self::Pi => PI_AGENT_DIR,
            Self::Claude => "/root/.claude",
        }
    }
}

impl fmt::Display for Agent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name())
    }
}

/// A named bundle of agent + provider plumbing: env vars and extra mounts.
/// Profiles merge by name across layers, last writer takes the whole
/// definition; fine-grained tweaks go through the ordinary layered `env`.
#[derive(Debug, Clone, PartialEq)]
pub struct Profile {
    pub name: String,
    pub agent: Agent,
    pub env: Vec<EnvVar>,
    pub mounts: Vec<Mount>,
    pub args: Vec<String>,
}

impl Profile {
    /// The trivial profiles shipped in the binary: bare agents with no
    /// provider plumbing, so ramekin runs with zero config. Everything
    /// richer is defined in KDL.
    fn builtin() -> Vec<Self> {
        [Agent::Pi, Agent::Claude]
            .into_iter()
            .map(|agent| Self {
                name: agent.name().to_string(),
                agent,
                env: Vec::new(),
                mounts: Vec::new(),
                args: Vec::new(),
            })
            .collect()
    }
}

/// The profile selected in the binary when no layer or flag picks one.
const DEFAULT_PROFILE: &str = "pi";

/// A mount as written in config: unexpanded paths, optional target.
#[derive(Debug, PartialEq, Clone)]
pub struct Mount {
    pub source: String,
    pub target: Option<String>,
    pub writable: bool,
}

/// A mount with tilde-expanded paths ready for Docker.
#[derive(Debug, PartialEq, Clone)]
pub struct ResolvedMount {
    pub source: PathBuf,
    pub target: String,
    pub writable: bool,
}

impl ResolvedMount {
    /// Label for display in `config` output (target, with ` (ro)` suffix when read-only).
    pub fn display_target(&self) -> String {
        if self.writable {
            self.target.clone()
        } else {
            format!("{} (ro)", self.target)
        }
    }
}

/// An environment variable for the container. `value: None` means the
/// variable passes through from the host environment at run time.
#[derive(Debug, PartialEq, Clone)]
pub struct EnvVar {
    pub name: String,
    pub value: Option<String>,
}

/// A single configuration layer.
#[derive(Debug)]
pub struct ConfigLayer {
    pub scope: Scope,
    /// `None` for the binary scope; the file (or directory, for the user
    /// layer) the config came from otherwise.
    pub path: Option<PathBuf>,
    pub mounts: Vec<ResolvedMount>,
    pub env: Vec<EnvVar>,
}

/// A value tagged with the config scope it came from.
#[derive(Debug, Clone, PartialEq)]
pub struct ScopedValue<T> {
    pub scope: Scope,
    pub value: T,
}

/// All configuration layers, ordered from lowest to highest precedence,
/// plus the resolved profile.
#[derive(Debug)]
pub struct ScopedConfig {
    pub layers: Vec<ConfigLayer>,
    /// Every known profile (builtin trivial ones plus KDL definitions),
    /// merged by name — later layers take the whole definition.
    pub profiles: BTreeMap<String, ScopedValue<Profile>>,
    /// The layer that selected the active profile, or `None` for `-p`.
    pub selected_by: Option<Scope>,
    /// The active profile.
    pub profile: Profile,
}

impl ScopedConfig {
    pub fn agent(&self) -> Agent {
        self.profile.agent
    }
}

/// Mask source: a mount whose source is `/dev/null` *removes* an inherited
/// mount at the same target instead of binding anything. With no inherited
/// mount to remove, it stays a real `/dev/null` bind, which blanks a file
/// that exists in the image or workspace.
const MASK_SOURCE: &str = "/dev/null";

impl ScopedConfig {
    /// Load all configuration layers for the given workspace.
    ///
    /// `workspace_target` is the container path of the workspace mount;
    /// relative mount targets resolve against it.
    ///
    /// Layers are returned in precedence order (lowest first):
    /// 1. Binary (staples, host agent-config mounts, trivial profiles)
    /// 2. Profile (the active profile's env and mounts)
    /// 3. User (every `*.kdl` in `~/.config/ramekin/`, merged as one layer)
    /// 4. Project (`<workspace>/.ramekin/config.kdl`)
    ///
    /// `cli_profile` is the `-p` selection, which beats any layer's.
    ///
    /// Returns an error if a config file can't be parsed, if two files
    /// within the user layer define the same key, or if the selected
    /// profile isn't defined anywhere.
    pub fn load(
        workspace: &Path,
        workspace_target: &str,
        cli_profile: Option<&str>,
    ) -> Result<Self> {
        // Resolve the user config home here rather than in `load_from` so
        // tests can inject an isolated directory and stay independent of
        // whatever `~/.config/ramekin/` holds on the developer's machine.
        let xdg = xdg::BaseDirectories::with_prefix("ramekin");
        Self::load_from(
            xdg.get_config_home().as_deref(),
            workspace,
            workspace_target,
            cli_profile,
        )
    }

    /// The layered load with the user config directory injected. `config_home`
    /// is where the user layer's `*.kdl` files live; `None`, or a path that
    /// isn't a directory, skips the user layer.
    fn load_from(
        config_home: Option<&Path>,
        workspace: &Path,
        workspace_target: &str,
        cli_profile: Option<&str>,
    ) -> Result<Self> {
        let mut builders = Vec::new();

        // User layer: every *.kdl in the config dir, sorted by name for
        // deterministic merging. Which files exist (and which are symlinks
        // into dotfiles) is a dotfiles decision, not a ramekin one.
        if let Some(config_dir) = config_home.filter(|d| d.is_dir()) {
            let mut files: Vec<PathBuf> = fs_err::read_dir(config_dir)
                .into_diagnostic()?
                .filter_map(|entry| entry.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|ext| ext == "kdl") && p.is_file())
                .collect();
            files.sort();
            if !files.is_empty() {
                let mut builder = LayerBuilder::new(Scope::User, Some(config_dir.to_path_buf()));
                for file in &files {
                    builder.add_file(file, workspace_target)?;
                }
                builders.push(builder);
            }
        }

        // Project layer
        let project_path = workspace.join(".ramekin/config.kdl");
        if project_path.exists() {
            let mut builder = LayerBuilder::new(Scope::Project, Some(project_path.clone()));
            builder.add_file(&project_path, workspace_target)?;
            builders.push(builder);
        }

        // Profiles merge by name across layers, last writer takes the whole
        // definition. Selection: highest layer wins, CLI beats all, binary
        // default when nothing selects.
        let mut profiles: BTreeMap<String, ScopedValue<Profile>> = Profile::builtin()
            .into_iter()
            .map(|p| {
                (
                    p.name.clone(),
                    ScopedValue {
                        scope: Scope::Binary,
                        value: p,
                    },
                )
            })
            .collect();
        let mut selection = (DEFAULT_PROFILE.to_string(), Some(Scope::Binary));
        for builder in &builders {
            for profile in &builder.profiles {
                profiles.insert(
                    profile.name.clone(),
                    ScopedValue {
                        scope: builder.scope,
                        value: profile.clone(),
                    },
                );
            }
            if let Some(name) = &builder.selection {
                selection = (name.clone(), Some(builder.scope));
            }
        }
        if let Some(name) = cli_profile {
            selection = (name.to_string(), None);
        }
        let (selected_name, selected_by) = selection;

        let profile = profiles
            .get(&selected_name)
            .map(|sv| sv.value.clone())
            .ok_or_else(|| {
                let known = profiles.keys().cloned().collect::<Vec<_>>().join(", ");
                let by = selected_by.map_or("-p".to_string(), |s| s.to_string());
                miette!(
                    "profile `{selected_name}` (selected by {by}) is not defined; \
                     known profiles: {known}",
                )
            })?;

        // The binary layer's agent-config mounts depend on the resolved
        // agent, so the layer is assembled only now.
        let mut layers = vec![ConfigLayer {
            scope: Scope::Binary,
            path: None,
            mounts: binary_mounts(profile.agent),
            env: Vec::new(),
        }];
        layers.push(ConfigLayer {
            scope: Scope::Profile,
            path: None,
            mounts: profile
                .mounts
                .iter()
                .filter_map(|m| m.resolve(workspace_target))
                .collect(),
            env: profile.env.clone(),
        });
        layers.extend(builders.into_iter().map(LayerBuilder::build));

        Ok(Self {
            layers,
            profiles,
            selected_by,
            profile,
        })
    }

    /// Return merged mounts from all layers, de-duplicated by container target.
    ///
    /// Higher-precedence layers override mounts with the same target, and a
    /// `/dev/null` source masks (removes) a mount inherited from a lower
    /// layer. Output order is lexicographic by target, which puts parent
    /// paths before any child path (`/root/.pi/agent` precedes
    /// `/root/.pi/agent/AGENTS.md`). Docker processes mount declarations in
    /// order; a parent declared after its child would shadow the child. The
    /// deterministic ordering also keeps repeat runs identical.
    pub fn merged_mounts(&self) -> Vec<ScopedValue<&ResolvedMount>> {
        let mut by_target: BTreeMap<&str, (ScopedValue<&ResolvedMount>, bool)> = BTreeMap::new();
        for layer in &self.layers {
            for mount in &layer.mounts {
                let inherited = by_target.contains_key(mount.target.as_str());
                by_target.insert(
                    mount.target.as_str(),
                    (
                        ScopedValue {
                            scope: layer.scope,
                            value: mount,
                        },
                        inherited,
                    ),
                );
            }
        }
        by_target
            .into_values()
            .filter(|(sv, inherited)| !(*inherited && sv.value.source == Path::new(MASK_SOURCE)))
            .map(|(sv, _)| sv)
            .collect()
    }

    /// Merge environment variables from all layers, de-duplicated by name.
    ///
    /// Higher-precedence layers override variables with the same name.
    /// Output order is lexicographic by name for reproducibility.
    pub fn merged_env(&self) -> Vec<ScopedValue<&EnvVar>> {
        let mut by_name: BTreeMap<&str, ScopedValue<&EnvVar>> = BTreeMap::new();
        for layer in &self.layers {
            for var in &layer.env {
                by_name.insert(
                    var.name.as_str(),
                    ScopedValue {
                        scope: layer.scope,
                        value: var,
                    },
                );
            }
        }
        by_name.into_values().collect()
    }
}

// ---------------------------------------------------------------------------
// Layer assembly
// ---------------------------------------------------------------------------

/// Accumulates parsed config into one layer, erroring on keys defined twice
/// within the layer — which matters for the user layer, where multiple files
/// merge and a silent last-writer-wins would depend on filename order.
struct LayerBuilder {
    scope: Scope,
    path: Option<PathBuf>,
    mounts: Vec<ResolvedMount>,
    mount_targets: BTreeSet<String>,
    env: Vec<EnvVar>,
    env_names: BTreeSet<String>,
    profiles: Vec<Profile>,
    profile_names: BTreeSet<String>,
    selection: Option<String>,
}

impl LayerBuilder {
    fn new(scope: Scope, path: Option<PathBuf>) -> Self {
        Self {
            scope,
            path,
            mounts: Vec::new(),
            mount_targets: BTreeSet::new(),
            env: Vec::new(),
            env_names: BTreeSet::new(),
            profiles: Vec::new(),
            profile_names: BTreeSet::new(),
            selection: None,
        }
    }

    fn add_file(&mut self, file: &Path, workspace_target: &str) -> Result<()> {
        let raw = parse_file(file)?;
        self.add(raw, file, workspace_target)
    }

    fn add(&mut self, raw: RawConfig, file: &Path, workspace_target: &str) -> Result<()> {
        for mount in &raw.mounts {
            // Mounts with a missing host source are skipped entirely, so
            // they don't participate in duplicate detection either.
            let Some(resolved) = mount.resolve(workspace_target) else {
                continue;
            };
            if !self.mount_targets.insert(resolved.target.clone()) {
                bail!(
                    "{}: mount target {} is defined twice in the {} layer",
                    file.display(),
                    resolved.target,
                    self.scope,
                );
            }
            self.mounts.push(resolved);
        }
        for var in raw.env {
            if !self.env_names.insert(var.name.clone()) {
                bail!(
                    "{}: env variable {} is defined twice in the {} layer",
                    file.display(),
                    var.name,
                    self.scope,
                );
            }
            self.env.push(var);
        }
        for profile in raw.profiles {
            if !self.profile_names.insert(profile.name.clone()) {
                bail!(
                    "{}: profile `{}` is defined twice in the {} layer",
                    file.display(),
                    profile.name,
                    self.scope,
                );
            }
            self.profiles.push(profile);
        }
        for name in raw.selections {
            if self.selection.is_some() {
                bail!(
                    "{}: the {} layer selects a profile twice",
                    file.display(),
                    self.scope,
                );
            }
            self.selection = Some(name);
        }
        Ok(())
    }

    fn build(self) -> ConfigLayer {
        ConfigLayer {
            scope: self.scope,
            path: self.path,
            mounts: self.mounts,
            env: self.env,
        }
    }
}

// ---------------------------------------------------------------------------
// KDL parsing
// ---------------------------------------------------------------------------

/// The parsed contents of a single config file.
#[derive(Debug, Default, PartialEq)]
struct RawConfig {
    mounts: Vec<Mount>,
    env: Vec<EnvVar>,
    profiles: Vec<Profile>,
    selections: Vec<String>,
}

fn parse_file(path: &Path) -> Result<RawConfig> {
    let content = fs_err::read_to_string(path)
        .into_diagnostic()
        .wrap_err("failed to read config file")?;
    parse_config(&content).wrap_err_with(|| format!("failed to parse {}", path.display()))
}

fn parse_config(content: &str) -> Result<RawConfig> {
    let doc: KdlDocument = content.parse().into_diagnostic()?;
    let mut raw = RawConfig::default();
    for node in doc.nodes() {
        match node.name().value() {
            "mounts" => raw.mounts.extend(parse_mounts(node)?),
            "env" => raw.env.extend(parse_env(node)?),
            // `profile "name" { ... }` defines; `profile "name"` selects.
            "profile" => match parse_profile(node)? {
                ProfileNode::Definition(profile) => raw.profiles.push(profile),
                ProfileNode::Selection(name) => raw.selections.push(name),
            },
            other => bail!("unknown config node `{other}`"),
        }
    }
    Ok(raw)
}

enum ProfileNode {
    Definition(Profile),
    Selection(String),
}

fn parse_profile(node: &KdlNode) -> Result<ProfileNode> {
    let name = match node.entries() {
        [entry] if entry.name().is_none() => entry
            .value()
            .as_string()
            .ok_or_else(|| miette!("`profile` takes a string name, got {}", entry.value()))?
            .to_string(),
        _ => bail!("`profile` takes exactly one string name"),
    };

    let Some(children) = node.children() else {
        return Ok(ProfileNode::Selection(name));
    };

    let mut agent = None;
    let mut env = Vec::new();
    let mut mounts = Vec::new();
    let mut args = Vec::new();
    for child in children.nodes() {
        match child.name().value() {
            "agent" => agent = Some(Agent::parse(&single_string_arg(child)?)?),
            "env" => env.extend(parse_env(child)?),
            "mounts" => mounts.extend(parse_mounts(child)?),
            "args" => args.extend(parse_args(child)?),
            other => bail!("unknown `profile` field `{other}`"),
        }
    }

    Ok(ProfileNode::Definition(Profile {
        agent: agent
            .ok_or_else(|| miette!("profile `{name}` is missing `agent` (`pi` or `claude`)"))?,
        name,
        env,
        mounts,
        args,
    }))
}

/// Parse an `args` node: bare string entries passed verbatim to the agent
/// binary (`args "--provider" "amazon-bedrock"`). No properties, no block.
fn parse_args(node: &KdlNode) -> Result<Vec<String>> {
    if node.children().is_some() {
        bail!("`args` takes inline string values (args \"--flag\" \"value\"), not a block");
    }
    node.entries()
        .iter()
        .map(|entry| {
            if entry.name().is_some() {
                bail!("`args` takes bare string values, not properties");
            }
            entry
                .value()
                .as_string()
                .map(str::to_string)
                .ok_or_else(|| miette!("`args` takes string values, got {}", entry.value()))
        })
        .collect()
}

/// Parse a `mounts` block. Exactly one syntax, mirroring `env`: a block with
/// one child node per mount, whose name is the host source path, with
/// optional `target` and `writable` properties.
fn parse_mounts(node: &KdlNode) -> Result<Vec<Mount>> {
    if !node.entries().is_empty() {
        bail!("`mounts` takes a block (mounts {{ \"~/path\" }}), not inline values");
    }
    let Some(children) = node.children() else {
        return Ok(Vec::new());
    };
    children.nodes().iter().map(parse_mount).collect()
}

fn parse_mount(node: &KdlNode) -> Result<Mount> {
    let source = node.name().value().to_string();
    // Catch the retired one-block-per-mount form (`mounts { source "..." }`)
    // and point at the current shape instead of misparsing its field names
    // as source paths.
    if matches!(source.as_str(), "source" | "target" | "writable") {
        bail!(
            "`{source}` is not a mount source: each mount is one node, \
             e.g. mounts {{ \"~/path\" target=\"/container/path\" writable=#true }}"
        );
    }
    if node.children().is_some() {
        bail!("mount \"{source}\" takes properties (target=\"...\", writable=#true), not a block");
    }

    let mut target = None;
    let mut writable = false;
    for entry in node.entries() {
        let Some(name) = entry.name() else {
            if entry.value().as_string() == Some("writable") {
                bail!("mount \"{source}\": write writable=#true to allow writes");
            }
            bail!(
                "mount \"{source}\": unexpected argument {}; the container path \
                 goes in target=\"...\"",
                entry.value()
            );
        };
        match name.value() {
            "target" => match entry.value().as_string() {
                Some(s) => target = Some(s.to_string()),
                None => bail!(
                    "mount \"{source}\": `target` takes a string, got {}",
                    entry.value()
                ),
            },
            "writable" => match entry.value().as_bool() {
                Some(b) => writable = b,
                None => bail!(
                    "mount \"{source}\": `writable` takes #true or #false, got {}",
                    entry.value()
                ),
            },
            other => bail!(
                "mount \"{source}\": unknown property `{other}` (expected `target` or `writable`)"
            ),
        }
    }

    Ok(Mount {
        source,
        target,
        writable,
    })
}

/// Parse an `env` block. Exactly one syntax: a block with one child node per
/// variable, whose single argument is the value. A bare child (no argument)
/// passes the host's value through at run time.
fn parse_env(node: &KdlNode) -> Result<Vec<EnvVar>> {
    if !node.entries().is_empty() {
        bail!("`env` takes a block (env {{ NAME \"value\" }}), not inline values");
    }
    let Some(children) = node.children() else {
        return Ok(Vec::new());
    };
    children
        .nodes()
        .iter()
        .map(|child| {
            let value = optional_string_arg(child)?;
            Ok(EnvVar {
                name: child.name().value().to_string(),
                value,
            })
        })
        .collect()
}

/// The node's single string argument, required.
fn single_string_arg(node: &KdlNode) -> Result<String> {
    optional_string_arg(node)?
        .ok_or_else(|| miette!("`{}` requires a string value", node.name().value()))
}

/// The node's single string argument, if present. Errors on properties,
/// extra arguments, or non-string values.
fn optional_string_arg(node: &KdlNode) -> Result<Option<String>> {
    let entries = node.entries();
    match entries {
        [] => Ok(None),
        [entry] if entry.name().is_none() => match entry.value().as_string() {
            Some(s) => Ok(Some(s.to_string())),
            None => bail!(
                "`{}` takes a string value, got {}",
                node.name().value(),
                entry.value()
            ),
        },
        _ => bail!("`{}` takes at most one string value", node.name().value()),
    }
}

// ---------------------------------------------------------------------------
// Builtin mounts and target resolution
// ---------------------------------------------------------------------------

/// Staple mounts every machine gets: read-only, skipped when missing on the
/// host, overridable (or maskable) by any config layer. The bar for a staple
/// is "true on every machine".
const STAPLES: &[&str] = &["~/.config/git", "~/.config/jj"];

/// Pi's agent dir inside the container.
pub const PI_AGENT_DIR: &str = "/root/.pi/agent";

/// Mounts compiled into the binary: staples plus the host's agent config for
/// the active agent.
///
/// Sources are canonicalized because agent dirs and staples commonly symlink
/// into dotfiles, and bind sources need real paths. Missing entries are
/// skipped.
fn binary_mounts(agent: Agent) -> Vec<ResolvedMount> {
    let staples = STAPLES
        .iter()
        .map(|source| ((*source).to_string(), resolve_container_target(source, "")));
    let agent_config = agent.config_allowlist().iter().map(move |entry| {
        (
            format!("{}/{entry}", agent.host_config_dir()),
            format!("{}/{entry}", agent.container_config_dir()),
        )
    });

    staples
        .chain(agent_config)
        .filter_map(|(source, target)| {
            let expanded = PathBuf::from(shellexpand::tilde(&source).as_ref());
            let canonical = expanded.canonicalize().ok()?;
            Some(ResolvedMount {
                source: canonical,
                target,
                writable: false,
            })
        })
        .collect()
}

impl Mount {
    /// Expand tildes and derive the container target path.
    ///
    /// Returns `None` if the source does not exist on the host. Files and
    /// devices (such as `/dev/null`) resolve like directories — Docker binds
    /// them all the same way.
    pub fn resolve(&self, workspace_target: &str) -> Option<ResolvedMount> {
        let expanded = PathBuf::from(shellexpand::tilde(&self.source).as_ref());
        if !expanded.exists() {
            return None;
        }

        let target = match &self.target {
            Some(t) => resolve_container_target(t, workspace_target),
            None => resolve_container_target(&self.source, workspace_target),
        };

        Some(ResolvedMount {
            source: expanded,
            target,
            writable: self.writable,
        })
    }
}

/// Home directory inside the agent container. The ramekin Dockerfile runs
/// everything as root, so `~` in container target paths maps here. If the
/// image ever switches to a non-root user, update this constant.
const CONTAINER_HOME: &str = "/root";

/// Resolve a configured target into an absolute container path.
///
/// A leading `~` expands to the container home directory. A relative path is
/// resolved against the workspace mount. Absolute paths pass through unchanged.
fn resolve_container_target(path: &str, workspace_target: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        format!("{CONTAINER_HOME}/{rest}")
    } else if path == "~" {
        CONTAINER_HOME.to_string()
    } else if path.starts_with('/') {
        path.to_string()
    } else {
        format!("{workspace_target}/{path}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WS: &str = "/workspace/test-slug";

    /// A ScopedConfig with the given layers and an inert trivial profile,
    /// for tests that only exercise merging.
    fn scoped(layers: Vec<ConfigLayer>) -> ScopedConfig {
        ScopedConfig {
            layers,
            profiles: BTreeMap::new(),
            selected_by: Some(Scope::Binary),
            profile: Profile {
                name: DEFAULT_PROFILE.into(),
                agent: Agent::Pi,
                env: Vec::new(),
                mounts: Vec::new(),
                args: Vec::new(),
            },
        }
    }

    #[test]
    fn parse_mounts_block() {
        let raw = parse_config(
            r#"
            mounts {
                "~/.config/git"
                "~/.local/share/ranger" writable=#true
                "~/Downloads" target="/root/downloads"
                "/x" writable=#false
            }
            "#,
        )
        .unwrap();
        assert_eq!(raw.mounts.len(), 4);
        assert_eq!(raw.mounts[0].source, "~/.config/git");
        assert!(!raw.mounts[0].writable);
        assert!(raw.mounts[1].writable);
        assert_eq!(raw.mounts[2].target, Some("/root/downloads".into()));
        assert!(!raw.mounts[2].writable);
        assert!(!raw.mounts[3].writable);
    }

    #[test]
    fn parse_env_block() {
        let raw = parse_config(
            r#"
            env {
                API_KEY "secret123"
                NODE_ENV "production"
                GITHUB_TOKEN
            }
            "#,
        )
        .unwrap();
        assert_eq!(raw.env.len(), 3);
        assert_eq!(raw.env[0].name, "API_KEY");
        assert_eq!(raw.env[0].value.as_deref(), Some("secret123"));
        // Bare variable: pass the host's value through at run time.
        assert_eq!(raw.env[2].name, "GITHUB_TOKEN");
        assert_eq!(raw.env[2].value, None);
    }

    #[test]
    fn env_flat_props_form_is_rejected() {
        // The old `env FOO="bar"` inline form is gone: one shape, no synonyms.
        let result = parse_config(r#"env FOO="bar""#);
        assert!(result.is_err());
    }

    #[test]
    fn unknown_node_is_rejected() {
        let result = parse_config(r#"pi { source "~/x" }"#);
        assert!(result.is_err());
    }

    #[test]
    fn unknown_mount_property_is_rejected() {
        let result = parse_config(r#"mounts { "/x" bogus="y" }"#);
        assert!(result.is_err());
    }

    #[test]
    fn mount_positional_argument_is_rejected() {
        // The target goes in a property, not a bare argument.
        let result = parse_config(r#"mounts { "/x" "/y" }"#);
        assert!(result.is_err());
    }

    #[test]
    fn mount_block_form_is_rejected() {
        // The old one-block-per-mount form is gone: one shape, no synonyms.
        let err = parse_config(r#"mounts { source "/x" target "/y" }"#).unwrap_err();
        assert!(err.to_string().contains("not a mount source"), "{err}");
    }

    #[test]
    fn mount_bare_writable_suggests_the_property() {
        let err = parse_config(r#"mounts { "/x" writable }"#).unwrap_err();
        assert!(err.to_string().contains("writable=#true"), "{err}");
    }

    #[test]
    fn invalid_syntax_is_rejected() {
        let result = parse_config("mounts { missing closing brace");
        assert!(result.is_err());
    }

    #[test]
    fn duplicate_env_within_layer_is_an_error() {
        let mut builder = LayerBuilder::new(Scope::User, None);
        let a = parse_config(r#"env { FOO "1" }"#).unwrap();
        let b = parse_config(r#"env { FOO "2" }"#).unwrap();
        builder.add(a, Path::new("a.kdl"), WS).unwrap();
        let err = builder.add(b, Path::new("b.kdl"), WS).unwrap_err();
        assert!(err.to_string().contains("FOO"), "{err}");
    }

    #[test]
    fn duplicate_mount_target_within_layer_is_an_error() {
        let mut builder = LayerBuilder::new(Scope::User, None);
        let a = parse_config(r#"mounts { "/tmp" target="/t" }"#).unwrap();
        let b = parse_config(r#"mounts { "/dev/null" target="/t" }"#).unwrap();
        builder.add(a, Path::new("a.kdl"), WS).unwrap();
        let err = builder.add(b, Path::new("b.kdl"), WS).unwrap_err();
        assert!(err.to_string().contains("/t"), "{err}");
    }

    #[test]
    fn missing_source_skips_duplicate_detection() {
        let mut builder = LayerBuilder::new(Scope::User, None);
        let a = parse_config(r#"mounts { "/nonexistent-a" target="/t" }"#).unwrap();
        let b = parse_config(r#"mounts { "/tmp" target="/t" }"#).unwrap();
        builder.add(a, Path::new("a.kdl"), WS).unwrap();
        builder.add(b, Path::new("b.kdl"), WS).unwrap();
        let layer = builder.build();
        assert_eq!(layer.mounts.len(), 1);
        assert_eq!(layer.mounts[0].source, PathBuf::from("/tmp"));
    }

    #[test]
    fn resolve_container_target_with_subpath() {
        assert_eq!(
            resolve_container_target("~/.config/git", WS),
            "/root/.config/git"
        );
    }

    #[test]
    fn resolve_container_target_bare() {
        assert_eq!(resolve_container_target("~", WS), "/root");
    }

    #[test]
    fn resolve_container_target_absolute_unchanged() {
        assert_eq!(resolve_container_target("/some/path", WS), "/some/path");
    }

    #[test]
    fn resolve_container_target_relative_uses_workspace() {
        assert_eq!(
            resolve_container_target(".envrc", WS),
            "/workspace/test-slug/.envrc"
        );
    }

    #[test]
    fn resolve_skips_nonexistent_source() {
        let mount = Mount {
            source: "/nonexistent/path/that/does/not/exist".into(),
            target: None,
            writable: false,
        };
        assert!(mount.resolve(WS).is_none());
    }

    #[test]
    fn resolve_with_file_source_and_relative_target() {
        let mount = Mount {
            source: "/dev/null".into(),
            target: Some(".envrc".into()),
            writable: false,
        };
        let resolved = mount.resolve(WS).unwrap();
        assert_eq!(resolved.source, PathBuf::from("/dev/null"));
        assert_eq!(resolved.target, "/workspace/test-slug/.envrc");
    }

    #[test]
    fn resolve_expands_tilde_in_explicit_target() {
        let mount = Mount {
            source: "/tmp".into(),
            target: Some("~/downloads".into()),
            writable: true,
        };
        let resolved = mount.resolve(WS).unwrap();
        assert_eq!(resolved.target, "/root/downloads");
    }

    #[test]
    fn resolve_derives_target_from_source_when_no_tilde() {
        let mount = Mount {
            source: "/tmp".into(),
            target: None,
            writable: true,
        };
        let resolved = mount.resolve(WS).unwrap();
        assert_eq!(resolved.target, "/tmp");
    }

    #[test]
    fn scope_display() {
        assert_eq!(Scope::Binary.to_string(), "binary");
        assert_eq!(Scope::User.to_string(), "user");
        assert_eq!(Scope::Project.to_string(), "project");
    }

    fn layer(scope: Scope, mounts: Vec<ResolvedMount>) -> ConfigLayer {
        ConfigLayer {
            scope,
            path: None,
            mounts,
            env: Vec::new(),
        }
    }

    fn mount(source: &str, target: &str, writable: bool) -> ResolvedMount {
        ResolvedMount {
            source: PathBuf::from(source),
            target: target.into(),
            writable,
        }
    }

    #[test]
    fn merged_mounts_accumulates_across_layers() {
        let config = scoped(vec![
            layer(Scope::Binary, vec![mount("/a", "/a", false)]),
            layer(Scope::User, vec![mount("/b", "/b", true)]),
            layer(Scope::Project, vec![mount("/c", "/c", true)]),
        ]);
        let merged = config.merged_mounts();
        assert_eq!(merged.len(), 3);
        let a = merged.iter().find(|sv| sv.value.target == "/a").unwrap();
        assert_eq!(a.scope, Scope::Binary);
        let b = merged.iter().find(|sv| sv.value.target == "/b").unwrap();
        assert_eq!(b.scope, Scope::User);
        let c = merged.iter().find(|sv| sv.value.target == "/c").unwrap();
        assert_eq!(c.scope, Scope::Project);
    }

    #[test]
    fn merged_mounts_deduplicates_by_target() {
        let config = scoped(vec![
            layer(
                Scope::User,
                vec![
                    mount("/user/git", "/root/.config/git", false),
                    mount("/user/jj", "/root/.config/jj", false),
                ],
            ),
            layer(
                Scope::Project,
                // Override the user git mount with a different source
                vec![mount("/project/git", "/root/.config/git", true)],
            ),
        ]);
        let merged = config.merged_mounts();
        // /root/.config/git appears in both layers; project layer wins
        assert_eq!(merged.len(), 2);
        let jj = merged
            .iter()
            .find(|sv| sv.value.target == "/root/.config/jj")
            .unwrap();
        assert_eq!(jj.scope, Scope::User);
        let git = merged
            .iter()
            .find(|sv| sv.value.target == "/root/.config/git")
            .unwrap();
        assert_eq!(git.scope, Scope::Project);
        assert_eq!(git.value.source, PathBuf::from("/project/git"));
        assert!(git.value.writable);
    }

    #[test]
    fn merged_mounts_orders_parents_before_children() {
        let config = scoped(vec![
            layer(
                Scope::Binary,
                vec![mount("/host/agents-md", "/root/.pi/agent/AGENTS.md", false)],
            ),
            layer(
                Scope::User,
                vec![mount("/host/agent-dir", "/root/.pi/agent", true)],
            ),
        ]);
        let merged = config.merged_mounts();
        let targets: Vec<&str> = merged.iter().map(|sv| sv.value.target.as_str()).collect();
        assert_eq!(
            targets,
            vec!["/root/.pi/agent", "/root/.pi/agent/AGENTS.md"]
        );
    }

    #[test]
    fn dev_null_mask_removes_inherited_mount() {
        let config = scoped(vec![
            layer(
                Scope::Binary,
                vec![mount("/host/skills", "/root/.pi/agent/skills", false)],
            ),
            layer(
                Scope::Project,
                vec![mount("/dev/null", "/root/.pi/agent/skills", false)],
            ),
        ]);
        let merged = config.merged_mounts();
        assert!(merged.is_empty(), "mask should remove the inherited mount");
    }

    #[test]
    fn dev_null_without_inherited_mount_stays_a_bind() {
        let config = scoped(vec![layer(
            Scope::Project,
            vec![mount("/dev/null", "/workspace/test-slug/.envrc", false)],
        )]);
        let merged = config.merged_mounts();
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].value.source, PathBuf::from("/dev/null"));
    }

    #[test]
    fn mount_overriding_mask_survives() {
        let config = scoped(vec![
            layer(Scope::Binary, vec![mount("/host/skills", "/s", false)]),
            layer(Scope::User, vec![mount("/dev/null", "/s", false)]),
            layer(Scope::Project, vec![mount("/project/skills", "/s", false)]),
        ]);
        let merged = config.merged_mounts();
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].value.source, PathBuf::from("/project/skills"));
        assert_eq!(merged[0].scope, Scope::Project);
    }

    #[test]
    fn load_with_no_project_config() {
        // Use a workspace with no .ramekin/ config files
        let config = ScopedConfig::load_from(None, Path::new("/tmp"), WS, None).unwrap();
        // Binary layer is always first (lowest precedence)
        assert_eq!(config.layers.first().unwrap().scope, Scope::Binary);
        // No project layer
        assert!(!config.layers.iter().any(|l| l.scope == Scope::Project));
    }

    #[test]
    fn load_with_project_config() {
        let dir = tempfile::tempdir().unwrap();
        let ramekin_dir = dir.path().join(".ramekin");
        fs_err::create_dir_all(&ramekin_dir).unwrap();
        fs_err::write(
            ramekin_dir.join("config.kdl"),
            "mounts {\n\"/tmp\" target=\"/container/tmp\"\n}\nenv {\nFOO \"bar\"\n}\n",
        )
        .unwrap();

        let config = ScopedConfig::load_from(None, dir.path(), WS, None).unwrap();

        let project = config
            .layers
            .iter()
            .find(|l| l.scope == Scope::Project)
            .expect("project layer missing");
        assert_eq!(project.mounts.len(), 1);
        assert_eq!(project.mounts[0].target, "/container/tmp");
        assert_eq!(project.env.len(), 1);
        assert_eq!(project.env[0].name, "FOO");
    }

    #[test]
    fn user_layer_loads_from_injected_config_home() {
        // The user layer reads from the injected config home, not the
        // developer's real `~/.config/ramekin/`. A bare `profile` selection
        // there overrides the binary `pi` default.
        let config_home = tempfile::tempdir().unwrap();
        fs_err::write(
            config_home.path().join("config.kdl"),
            "profile \"claude\"\n",
        )
        .unwrap();
        let workspace = tempfile::tempdir().unwrap();

        let config =
            ScopedConfig::load_from(Some(config_home.path()), workspace.path(), WS, None).unwrap();

        assert_eq!(config.selected_by, Some(Scope::User));
        assert_eq!(config.agent(), Agent::Claude);
    }

    #[test]
    fn merged_env_deduplicates_by_name() {
        let user = ConfigLayer {
            scope: Scope::User,
            path: None,
            mounts: vec![],
            env: vec![
                EnvVar {
                    name: "FOO".into(),
                    value: Some("user".into()),
                },
                EnvVar {
                    name: "BAR".into(),
                    value: Some("user".into()),
                },
            ],
        };
        let project = ConfigLayer {
            scope: Scope::Project,
            path: None,
            mounts: vec![],
            env: vec![EnvVar {
                name: "FOO".into(),
                value: Some("project".into()),
            }],
        };
        let config = scoped(vec![user, project]);
        let merged = config.merged_env();
        assert_eq!(merged.len(), 2);
        let foo = merged.iter().find(|sv| sv.value.name == "FOO").unwrap();
        assert_eq!(foo.value.value.as_deref(), Some("project"));
        assert_eq!(foo.scope, Scope::Project);
        let bar = merged.iter().find(|sv| sv.value.name == "BAR").unwrap();
        assert_eq!(bar.scope, Scope::User);
    }

    #[test]
    fn parse_profile_definition() {
        let raw = parse_config(
            r#"
            profile "claude-bedrock" {
                agent "claude"
                env {
                    CLAUDE_CODE_USE_BEDROCK "1"
                    AWS_PROFILE
                }
                mounts { "~/.aws" }
            }
            "#,
        )
        .unwrap();
        assert!(raw.selections.is_empty());
        assert_eq!(raw.profiles.len(), 1);
        let p = &raw.profiles[0];
        assert_eq!(p.name, "claude-bedrock");
        assert_eq!(p.agent, Agent::Claude);
        assert_eq!(p.env.len(), 2);
        assert_eq!(p.env[1].name, "AWS_PROFILE");
        assert_eq!(p.env[1].value, None);
        assert_eq!(p.mounts.len(), 1);
        assert_eq!(p.mounts[0].source, "~/.aws");
    }

    #[test]
    fn parse_profile_with_args() {
        let raw = parse_config(
            r#"
            profile "pi-bedrock" {
                agent "pi"
                args "--provider" "amazon-bedrock"
            }
            "#,
        )
        .unwrap();
        let p = &raw.profiles[0];
        assert_eq!(p.args, vec!["--provider", "amazon-bedrock"]);
    }

    #[test]
    fn parse_profile_concatenates_multiple_args_nodes() {
        let raw = parse_config(
            r#"
            profile "x" {
                agent "pi"
                args "--provider" "amazon-bedrock"
                args "--model" "some-model"
            }
            "#,
        )
        .unwrap();
        assert_eq!(
            raw.profiles[0].args,
            vec!["--provider", "amazon-bedrock", "--model", "some-model"]
        );
    }

    #[test]
    fn parse_profile_rejects_non_string_args() {
        let result = parse_config("profile \"x\" {\n    agent \"pi\"\n    args 42\n}");
        assert!(result.is_err());
    }

    #[test]
    fn parse_profile_selection() {
        let raw = parse_config(r#"profile "pi-glm""#).unwrap();
        assert!(raw.profiles.is_empty());
        assert_eq!(raw.selections, vec!["pi-glm".to_string()]);
    }

    #[test]
    fn profile_without_agent_is_rejected() {
        let result = parse_config(r#"profile "x" { env { FOO "1" } }"#);
        assert!(result.is_err());
    }

    #[test]
    fn profile_with_unknown_agent_is_rejected() {
        let result = parse_config(r#"profile "x" { agent "glm" }"#);
        assert!(result.is_err());
    }

    #[test]
    fn builtin_trivial_profile_is_the_default() {
        let dir = tempfile::tempdir().unwrap();
        let config = ScopedConfig::load_from(None, dir.path(), WS, None).unwrap();
        assert_eq!(config.profile.name, "pi");
        assert_eq!(config.selected_by, Some(Scope::Binary));
        assert_eq!(config.agent(), Agent::Pi);
    }

    #[test]
    fn project_selection_beats_binary_default() {
        let dir = tempfile::tempdir().unwrap();
        let ramekin_dir = dir.path().join(".ramekin");
        fs_err::create_dir_all(&ramekin_dir).unwrap();
        fs_err::write(ramekin_dir.join("config.kdl"), "profile \"claude\"\n").unwrap();

        let config = ScopedConfig::load_from(None, dir.path(), WS, None).unwrap();
        assert_eq!(config.profile.name, "claude");
        assert_eq!(config.selected_by, Some(Scope::Project));
        assert_eq!(config.agent(), Agent::Claude);
    }

    #[test]
    fn cli_selection_beats_layers() {
        let dir = tempfile::tempdir().unwrap();
        let ramekin_dir = dir.path().join(".ramekin");
        fs_err::create_dir_all(&ramekin_dir).unwrap();
        fs_err::write(ramekin_dir.join("config.kdl"), "profile \"claude\"\n").unwrap();

        let config = ScopedConfig::load_from(None, dir.path(), WS, Some("pi")).unwrap();
        assert_eq!(config.profile.name, "pi");
        // `-p` isn't a layer; selected_by is None.
        assert_eq!(config.selected_by, None);
        assert_eq!(config.agent(), Agent::Pi);
    }

    #[test]
    fn unknown_profile_selection_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = ScopedConfig::load_from(None, dir.path(), WS, Some("bogus")).unwrap_err();
        assert!(err.to_string().contains("bogus"), "{err}");
    }

    #[test]
    fn profile_env_and_mounts_form_a_layer_below_project() {
        let dir = tempfile::tempdir().unwrap();
        let ramekin_dir = dir.path().join(".ramekin");
        fs_err::create_dir_all(&ramekin_dir).unwrap();
        fs_err::write(
            ramekin_dir.join("config.kdl"),
            r#"
            profile "pi-glm" {
                agent "pi"
                env {
                    ANTHROPIC_BASE_URL "https://open.bigmodel.cn/api/anthropic"
                    ZHIPU_API_KEY
                }
                mounts {
                    "/tmp" target="/root/extra"
                }
            }
            profile "pi-glm"
            env {
                ANTHROPIC_BASE_URL "https://elsewhere.example"
            }
            "#,
        )
        .unwrap();

        let config = ScopedConfig::load_from(None, dir.path(), WS, None).unwrap();
        assert_eq!(config.profile.name, "pi-glm");

        // Layered env overlays the profile's env per variable.
        let merged = config.merged_env();
        let base_url = merged
            .iter()
            .find(|sv| sv.value.name == "ANTHROPIC_BASE_URL")
            .unwrap();
        assert_eq!(base_url.scope, Scope::Project);
        assert_eq!(
            base_url.value.value.as_deref(),
            Some("https://elsewhere.example")
        );
        let key = merged
            .iter()
            .find(|sv| sv.value.name == "ZHIPU_API_KEY")
            .unwrap();
        assert_eq!(key.scope, Scope::Profile);
        assert_eq!(key.value.value, None);

        // Profile mounts join the merge at profile scope.
        let mounts = config.merged_mounts();
        let extra = mounts
            .iter()
            .find(|sv| sv.value.target == "/root/extra")
            .unwrap();
        assert_eq!(extra.scope, Scope::Profile);
    }

    #[test]
    fn later_layer_redefines_profile_wholesale() {
        let dir = tempfile::tempdir().unwrap();
        let ramekin_dir = dir.path().join(".ramekin");
        fs_err::create_dir_all(&ramekin_dir).unwrap();
        // Redefine the builtin trivial `pi` profile from the project layer.
        fs_err::write(
            ramekin_dir.join("config.kdl"),
            r#"
            profile "pi" {
                agent "claude"
            }
            "#,
        )
        .unwrap();

        let config = ScopedConfig::load_from(None, dir.path(), WS, None).unwrap();
        // The default selection still names `pi`, but the project layer's
        // definition wins wholesale: it now runs claude.
        assert_eq!(config.profile.name, "pi");
        assert_eq!(config.selected_by, Some(Scope::Binary));
        assert_eq!(config.agent(), Agent::Claude);
        assert_eq!(config.profiles.get("pi").unwrap().scope, Scope::Project);
    }

    #[test]
    fn agent_allowlists() {
        assert!(Agent::Pi.config_allowlist().contains(&"AGENTS.md"));
        assert!(Agent::Claude.config_allowlist().contains(&"CLAUDE.md"));
        assert!(Agent::Claude.config_allowlist().contains(&"hooks"));
        assert_eq!(Agent::Claude.container_config_dir(), "/root/.claude");
    }
}
