use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};

use miette::{Context, IntoDiagnostic, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub mounts: Vec<Mount>,
    #[serde(default)]
    pub env: HashMap<String, String>,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct Mount {
    pub source: String,
    pub target: Option<String>,
    #[serde(
        default,
        deserialize_with = "serde_kdl2::bare_defaults::bool::bare_true"
    )]
    pub writable: bool,
}

/// Configuration scope, ordered from lowest to highest precedence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Scope {
    /// Compiled into the binary: staples and host agent-config mounts.
    Binary,
    /// User-level `~/.config/ramekin/config.kdl`.
    User,
    /// Project-level `<workspace>/.ramekin/config.kdl`.
    Project,
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Binary => write!(f, "binary"),
            Self::User => write!(f, "user"),
            Self::Project => write!(f, "project"),
        }
    }
}

/// A single configuration layer: a scope, an optional file path, and resolved mounts.
#[derive(Debug)]
pub struct ConfigLayer {
    pub scope: Scope,
    /// `None` for the binary scope; `Some(path)` for file-backed scopes.
    pub path: Option<PathBuf>,
    pub mounts: Vec<ResolvedMount>,
    pub env: HashMap<String, String>,
}

/// A value tagged with the config scope it came from.
#[derive(Debug, Clone, PartialEq)]
pub struct ScopedValue<T> {
    pub scope: Scope,
    pub value: T,
}

/// All configuration layers, ordered from lowest to highest precedence.
///
/// The effective configuration comes from the highest-precedence layer
/// that is present.
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
    pub fn merged_env(&self) -> Vec<ScopedValue<(&str, &str)>> {
        let mut by_name: BTreeMap<&str, ScopedValue<(&str, &str)>> = BTreeMap::new();
        for layer in &self.layers {
            for (name, value) in &layer.env {
                by_name.insert(
                    name.as_str(),
                    ScopedValue {
                        scope: layer.scope,
                        value: (name.as_str(), value.as_str()),
                    },
                );
            }
        }
        by_name.into_values().collect()
    }
}

/// A mount with tilde-expanded paths ready for Docker.
#[derive(Debug, PartialEq)]
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

impl Config {
    /// Load all configuration layers for the given workspace.
    ///
    /// `workspace_target` is the container path of the workspace mount;
    /// relative mount targets resolve against it.
    ///
    /// Layers are returned in precedence order (lowest first):
    /// 1. Binary (staples and host agent-config mounts)
    /// 2. User (`~/.config/ramekin/config.kdl`) — only if the file exists
    /// 3. Project (`<workspace>/.ramekin/config.kdl`) — only if the file exists
    ///
    /// Returns an error if a config file exists but can't be parsed.
    pub fn load(workspace: &Path, workspace_target: &str) -> Result<ScopedConfig> {
        let mut layers = vec![ConfigLayer {
            scope: Scope::Binary,
            path: None,
            mounts: binary_mounts(),
            env: HashMap::new(),
        }];

        // User layer
        let xdg = xdg::BaseDirectories::with_prefix("ramekin");
        let user_path = xdg
            .place_config_file("config.kdl")
            .into_diagnostic()
            .wrap_err("failed to determine user config path")?;

        if user_path.exists() {
            let config = Self::load_file(&user_path)
                .wrap_err_with(|| format!("failed to load user config: {}", user_path.display()))?;
            layers.push(ConfigLayer {
                scope: Scope::User,
                path: Some(user_path),
                mounts: config.resolve_mounts(workspace_target),
                env: config.env,
            });
        }

        // Project layer
        let project_path = workspace.join(".ramekin/config.kdl");
        if project_path.exists() {
            let config = Self::load_file(&project_path).wrap_err_with(|| {
                format!("failed to load project config: {}", project_path.display())
            })?;
            layers.push(ConfigLayer {
                scope: Scope::Project,
                path: Some(project_path),
                mounts: config.resolve_mounts(workspace_target),
                env: config.env,
            });
        }

        Ok(ScopedConfig { layers })
    }

    /// Parse a config file.
    fn load_file(path: &Path) -> Result<Self> {
        let content = fs_err::read_to_string(path)
            .into_diagnostic()
            .wrap_err("failed to read config file")?;
        serde_kdl2::from_str(&content)
            .into_diagnostic()
            .wrap_err("failed to parse config file")
    }

    /// Resolve all mounts, skipping any whose source does not exist.
    fn resolve_mounts(&self, workspace_target: &str) -> Vec<ResolvedMount> {
        self.mounts
            .iter()
            .filter_map(|m| m.resolve(workspace_target))
            .collect()
    }
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
    fn kdl_deserialization_works() {
        let kdl_content = r#"
            mounts{
            source "~/.config/git"
            }
            mounts{
            source "~/.config/jj"
            }
            mounts{
            source "~/.local/share/ranger"
            writable #true
            }
            mounts{
            source "~/Downloads"
            target "/root/downloads"
            }
        "#;

        let parsed: Config = serde_kdl2::from_str(kdl_content).unwrap();
        assert_eq!(parsed.mounts.len(), 4);
        assert_eq!(parsed.mounts[0].source, "~/.config/git");
        assert!(!parsed.mounts[0].writable);
        assert!(parsed.mounts[2].writable);
        assert_eq!(parsed.mounts[3].target, Some("/root/downloads".into()));
        // writable defaults to false when omitted.
        assert!(!parsed.mounts[3].writable);
    }

    #[test]
    fn kdl_bare_writable() {
        let kdl_content = r#"
            mounts {
                source "~/.local/share/ranger"
                writable
            }
            mounts {
                source "~/.config/git"
            }
        "#;

        let parsed: Config = serde_kdl2::from_str(kdl_content).unwrap();
        assert_eq!(parsed.mounts.len(), 2);
        assert!(parsed.mounts[0].writable);
        assert!(!parsed.mounts[1].writable);
    }

    #[test]
    fn kdl_parse_error_on_invalid_syntax() {
        let invalid_kdl = "invalid_syntax_here{ missing closing brace";
        let result: Result<Config, _> = serde_kdl2::from_str(invalid_kdl);
        assert!(result.is_err());
    }

    #[test]
    fn kdl_dash_notation_mounts_writable() {
        let kdl = r#"
            mounts {
                - { source "~/.local/share/ranger"; writable }
            }
        "#;
        let parsed: Config = serde_kdl2::from_str(kdl).unwrap();
        assert_eq!(parsed.mounts.len(), 1);
        assert!(parsed.mounts[0].writable);
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
    fn resolve_with_existing_dir_and_explicit_target() {
        let mount = Mount {
            source: "/tmp".into(),
            target: Some("/container/tmp".into()),
            writable: false,
        };
        let resolved = mount.resolve(WS).unwrap();
        assert_eq!(resolved.source, PathBuf::from("/tmp"));
        assert_eq!(resolved.target, "/container/tmp");
        assert!(!resolved.writable);
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
    fn resolve_mounts_filters_nonexistent() {
        let config = Config {
            mounts: vec![
                Mount {
                    source: "/tmp".into(),
                    target: Some("/container/tmp".into()),
                    writable: true,
                },
                Mount {
                    source: "/nonexistent".into(),
                    target: None,
                    writable: false,
                },
            ],
            env: HashMap::new(),
        };
        let resolved = config.resolve_mounts(WS);
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].target, "/container/tmp");
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
            env: HashMap::new(),
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
        // Use a workspace with no .ramekin/config.kdl
        let config = Config::load(Path::new("/tmp"), WS).unwrap();
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
        let config_path = ramekin_dir.join("config.kdl");
        // serde_kdl2 requires at least two repeated nodes for Vec deserialization
        fs_err::write(
            &config_path,
            "mounts{\nsource \"/tmp\"\ntarget \"/container/tmp\"\n}\nmounts{\nsource \"/tmp\"\ntarget \"/container/tmp2\"\n}\n",
        )
        .unwrap();

        let config = Config::load(dir.path(), WS).unwrap();

        // Should have binary + project layers
        assert!(config.layers.len() >= 2);
        let project_layer = config
            .layers
            .iter()
            .find(|l| l.scope == Scope::Project)
            .expect("project layer missing");
        assert_eq!(project_layer.mounts.len(), 2);
        assert_eq!(project_layer.mounts[0].target, "/container/tmp");
        // Binary is always first (lowest precedence)
        assert_eq!(config.layers.first().unwrap().scope, Scope::Binary);
    }

    #[test]
    fn kdl_env_flat_props() {
        let kdl = r#"
            env FOO="bar"
        "#;
        let parsed: Config = serde_kdl2::from_str(kdl).unwrap();
        assert_eq!(parsed.env.get("FOO").unwrap(), "bar");
    }

    #[test]
    fn kdl_env_multiple() {
        let kdl = r#"
            env {
                API_KEY "secret123"
                NODE_ENV "production"
            }
        "#;
        let parsed: Config = serde_kdl2::from_str(kdl).unwrap();
        assert_eq!(parsed.env.len(), 2);
        assert_eq!(parsed.env.get("API_KEY").unwrap(), "secret123");
        assert_eq!(parsed.env.get("NODE_ENV").unwrap(), "production");
    }

    #[test]
    fn merged_env_deduplicates_by_name() {
        let mut user_env = HashMap::new();
        user_env.insert("FOO".into(), "user".into());
        user_env.insert("BAR".into(), "user".into());

        let mut project_env = HashMap::new();
        project_env.insert("FOO".into(), "project".into());

        let config = ScopedConfig {
            layers: vec![
                ConfigLayer {
                    scope: Scope::User,
                    path: None,
                    mounts: vec![],
                    env: user_env,
                },
                ConfigLayer {
                    scope: Scope::Project,
                    path: None,
                    mounts: vec![],
                    env: project_env,
                },
            ],
        };
        let merged = config.merged_env();
        assert_eq!(merged.len(), 2);
        let foo = merged.iter().find(|sv| sv.value.0 == "FOO").unwrap();
        assert_eq!(foo.value.1, "project");
        assert_eq!(foo.scope, Scope::Project);
        let bar = merged.iter().find(|sv| sv.value.0 == "BAR").unwrap();
        assert_eq!(bar.value.1, "user");
        assert_eq!(bar.scope, Scope::User);
    }
}
