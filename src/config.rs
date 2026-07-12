use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

use kdl::{KdlDocument, KdlNode};
use miette::{Context, IntoDiagnostic, Result, bail, miette};

/// Configuration scope, ordered from lowest to highest precedence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Scope {
    /// Compiled into the binary: staples and host agent-config mounts.
    Binary,
    /// The user layer: every `*.kdl` in `~/.config/ramekin/`, merged.
    User,
    /// Project-level `<workspace>/.ramekin/config.kdl`, committed.
    Project,
    /// Project-local `<workspace>/.ramekin/config.local.kdl`, gitignored.
    ProjectLocal,
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Binary => write!(f, "binary"),
            Self::User => write!(f, "user"),
            Self::Project => write!(f, "project"),
            Self::ProjectLocal => write!(f, "project-local"),
        }
    }
}

/// A mount as written in config: unexpanded paths, optional target.
#[derive(Debug, PartialEq)]
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

/// All configuration layers, ordered from lowest to highest precedence.
#[derive(Debug)]
pub struct ScopedConfig {
    pub layers: Vec<ConfigLayer>,
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
    /// 1. Binary (staples and host agent-config mounts)
    /// 2. User (every `*.kdl` in `~/.config/ramekin/`, merged as one layer)
    /// 3. Project (`<workspace>/.ramekin/config.kdl`)
    /// 4. Project-local (`<workspace>/.ramekin/config.local.kdl`)
    ///
    /// Returns an error if a config file can't be parsed, or if two files
    /// within the user layer define the same key.
    pub fn load(workspace: &Path, workspace_target: &str) -> Result<Self> {
        let mut layers = vec![ConfigLayer {
            scope: Scope::Binary,
            path: None,
            mounts: binary_mounts(),
            env: Vec::new(),
        }];

        // User layer: every *.kdl in the config dir, sorted by name for
        // deterministic merging. Which files exist (and which are symlinks
        // into dotfiles) is a dotfiles decision, not a ramekin one.
        let xdg = xdg::BaseDirectories::with_prefix("ramekin");
        if let Some(config_dir) = xdg.get_config_home().filter(|d| d.is_dir()) {
            let mut files: Vec<PathBuf> = fs_err::read_dir(&config_dir)
                .into_diagnostic()?
                .filter_map(|entry| entry.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|ext| ext == "kdl") && p.is_file())
                .collect();
            files.sort();
            if !files.is_empty() {
                let mut builder = LayerBuilder::new(Scope::User, Some(config_dir));
                for file in &files {
                    builder.add_file(file, workspace_target)?;
                }
                layers.push(builder.build());
            }
        }

        // Project layers
        for (scope, name) in [
            (Scope::Project, "config.kdl"),
            (Scope::ProjectLocal, "config.local.kdl"),
        ] {
            let path = workspace.join(".ramekin").join(name);
            if path.exists() {
                let mut builder = LayerBuilder::new(scope, Some(path.clone()));
                builder.add_file(&path, workspace_target)?;
                layers.push(builder.build());
            }
        }

        Ok(Self { layers })
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
            "mounts" => raw.mounts.push(parse_mount(node)?),
            "env" => raw.env.extend(parse_env(node)?),
            other => bail!("unknown config node `{other}`"),
        }
    }
    Ok(raw)
}

fn parse_mount(node: &KdlNode) -> Result<Mount> {
    if !node.entries().is_empty() {
        bail!("`mounts` takes a block of child nodes, not inline values");
    }
    let children = node
        .children()
        .ok_or_else(|| miette!("`mounts` requires a block with a `source`"))?;

    let mut source = None;
    let mut target = None;
    let mut writable = false;
    for child in children.nodes() {
        match child.name().value() {
            "source" => source = Some(single_string_arg(child)?),
            "target" => target = Some(single_string_arg(child)?),
            "writable" => writable = bool_flag(child)?,
            other => bail!("unknown `mounts` field `{other}`"),
        }
    }

    Ok(Mount {
        source: source.ok_or_else(|| miette!("`mounts` block is missing `source`"))?,
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

/// A boolean flag node: bare means true, or one boolean argument.
fn bool_flag(node: &KdlNode) -> Result<bool> {
    match node.entries() {
        [] => Ok(true),
        [entry] if entry.name().is_none() => entry.value().as_bool().ok_or_else(|| {
            miette!(
                "`{}` takes a boolean value, got {}",
                node.name().value(),
                entry.value()
            )
        }),
        _ => bail!("`{}` takes at most one boolean value", node.name().value()),
    }
}

// ---------------------------------------------------------------------------
// Builtin mounts and target resolution
// ---------------------------------------------------------------------------

/// Staple mounts every machine gets: read-only, skipped when missing on the
/// host, overridable (or maskable) by any config layer. The bar for a staple
/// is "true on every machine".
const STAPLES: &[&str] = &["~/.config/git", "~/.config/jj"];

/// Host directory where pi keeps its agent config and state.
const HOST_PI_AGENT_DIR: &str = "~/.pi/agent";

/// The config-shaped entries of the host's pi agent dir. Only these mount
/// into the container (read-only); the rest of the dir is runtime state —
/// credentials, session history — which must not leak in.
pub const PI_AGENT_CONFIG: &[&str] = &["AGENTS.md", "skills"];

/// Pi's agent dir inside the container.
pub const PI_AGENT_DIR: &str = "/root/.pi/agent";

/// Mounts compiled into the binary: staples plus the host's pi agent config.
///
/// Sources are canonicalized because agent dirs and staples commonly symlink
/// into dotfiles, and bind sources need real paths. Missing entries are
/// skipped.
fn binary_mounts() -> Vec<ResolvedMount> {
    let staples = STAPLES
        .iter()
        .map(|source| ((*source).to_string(), resolve_container_target(source, "")));
    let agent_config = PI_AGENT_CONFIG.iter().map(|entry| {
        (
            format!("{HOST_PI_AGENT_DIR}/{entry}"),
            format!("{PI_AGENT_DIR}/{entry}"),
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

    #[test]
    fn parse_mounts() {
        let raw = parse_config(
            r#"
            mounts {
                source "~/.config/git"
            }
            mounts {
                source "~/.local/share/ranger"
                writable
            }
            mounts {
                source "~/Downloads"
                target "/root/downloads"
            }
            mounts {
                source "/x"
                writable #false
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
    fn unknown_mount_field_is_rejected() {
        let result = parse_config(
            r#"
            mounts {
                source "/x"
                bogus "y"
            }
            "#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn mount_without_source_is_rejected() {
        let result = parse_config("mounts {\ntarget \"/x\"\n}");
        assert!(result.is_err());
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
        let a = parse_config("mounts {\nsource \"/tmp\"\ntarget \"/t\"\n}").unwrap();
        let b = parse_config("mounts {\nsource \"/dev/null\"\ntarget \"/t\"\n}").unwrap();
        builder.add(a, Path::new("a.kdl"), WS).unwrap();
        let err = builder.add(b, Path::new("b.kdl"), WS).unwrap_err();
        assert!(err.to_string().contains("/t"), "{err}");
    }

    #[test]
    fn missing_source_skips_duplicate_detection() {
        let mut builder = LayerBuilder::new(Scope::User, None);
        let a = parse_config("mounts {\nsource \"/nonexistent-a\"\ntarget \"/t\"\n}").unwrap();
        let b = parse_config("mounts {\nsource \"/tmp\"\ntarget \"/t\"\n}").unwrap();
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
        assert_eq!(Scope::ProjectLocal.to_string(), "project-local");
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
        let config = ScopedConfig {
            layers: vec![
                layer(Scope::Binary, vec![mount("/a", "/a", false)]),
                layer(Scope::User, vec![mount("/b", "/b", true)]),
                layer(Scope::Project, vec![mount("/c", "/c", true)]),
            ],
        };
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
        let config = ScopedConfig {
            layers: vec![
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
            ],
        };
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
        let config = ScopedConfig {
            layers: vec![
                layer(
                    Scope::Binary,
                    vec![mount("/host/agents-md", "/root/.pi/agent/AGENTS.md", false)],
                ),
                layer(
                    Scope::User,
                    vec![mount("/host/agent-dir", "/root/.pi/agent", true)],
                ),
            ],
        };
        let merged = config.merged_mounts();
        let targets: Vec<&str> = merged.iter().map(|sv| sv.value.target.as_str()).collect();
        assert_eq!(
            targets,
            vec!["/root/.pi/agent", "/root/.pi/agent/AGENTS.md"]
        );
    }

    #[test]
    fn dev_null_mask_removes_inherited_mount() {
        let config = ScopedConfig {
            layers: vec![
                layer(
                    Scope::Binary,
                    vec![mount("/host/skills", "/root/.pi/agent/skills", false)],
                ),
                layer(
                    Scope::Project,
                    vec![mount("/dev/null", "/root/.pi/agent/skills", false)],
                ),
            ],
        };
        let merged = config.merged_mounts();
        assert!(merged.is_empty(), "mask should remove the inherited mount");
    }

    #[test]
    fn dev_null_without_inherited_mount_stays_a_bind() {
        let config = ScopedConfig {
            layers: vec![layer(
                Scope::Project,
                vec![mount("/dev/null", "/workspace/test-slug/.envrc", false)],
            )],
        };
        let merged = config.merged_mounts();
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].value.source, PathBuf::from("/dev/null"));
    }

    #[test]
    fn mount_overriding_mask_survives() {
        let config = ScopedConfig {
            layers: vec![
                layer(Scope::Binary, vec![mount("/host/skills", "/s", false)]),
                layer(Scope::User, vec![mount("/dev/null", "/s", false)]),
                layer(Scope::Project, vec![mount("/project/skills", "/s", false)]),
            ],
        };
        let merged = config.merged_mounts();
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].value.source, PathBuf::from("/project/skills"));
        assert_eq!(merged[0].scope, Scope::Project);
    }

    #[test]
    fn load_with_no_project_config() {
        // Use a workspace with no .ramekin/ config files
        let config = ScopedConfig::load(Path::new("/tmp"), WS).unwrap();
        // Binary layer is always first (lowest precedence)
        assert_eq!(config.layers.first().unwrap().scope, Scope::Binary);
        // No project layers
        assert!(!config.layers.iter().any(|l| l.scope == Scope::Project));
        assert!(!config.layers.iter().any(|l| l.scope == Scope::ProjectLocal));
    }

    #[test]
    fn load_with_project_config() {
        let dir = tempfile::tempdir().unwrap();
        let ramekin_dir = dir.path().join(".ramekin");
        fs_err::create_dir_all(&ramekin_dir).unwrap();
        fs_err::write(
            ramekin_dir.join("config.kdl"),
            "mounts {\nsource \"/tmp\"\ntarget \"/container/tmp\"\n}\n",
        )
        .unwrap();
        fs_err::write(
            ramekin_dir.join("config.local.kdl"),
            "env {\nFOO \"bar\"\n}\n",
        )
        .unwrap();

        let config = ScopedConfig::load(dir.path(), WS).unwrap();

        let project = config
            .layers
            .iter()
            .find(|l| l.scope == Scope::Project)
            .expect("project layer missing");
        assert_eq!(project.mounts.len(), 1);
        assert_eq!(project.mounts[0].target, "/container/tmp");

        let local = config
            .layers
            .iter()
            .find(|l| l.scope == Scope::ProjectLocal)
            .expect("project-local layer missing");
        assert_eq!(local.env.len(), 1);
        assert_eq!(local.env[0].name, "FOO");
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
        let config = ScopedConfig {
            layers: vec![user, project],
        };
        let merged = config.merged_env();
        assert_eq!(merged.len(), 2);
        let foo = merged.iter().find(|sv| sv.value.name == "FOO").unwrap();
        assert_eq!(foo.value.value.as_deref(), Some("project"));
        assert_eq!(foo.scope, Scope::Project);
        let bar = merged.iter().find(|sv| sv.value.name == "BAR").unwrap();
        assert_eq!(bar.scope, Scope::User);
    }
}
