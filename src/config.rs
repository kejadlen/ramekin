use std::fmt;
use std::path::{Path, PathBuf};

use color_eyre::eyre::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct Config {
    pub mounts: Vec<Mount>,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// User-level `~/.config/ramekin/config.kdl`.
    User,
    /// Project-level `<workspace>/.ramekin/config.kdl`.
    Project,
    /// Internal mounts managed by ramekin (highest precedence).
    Builtin,
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::User => write!(f, "user"),
            Self::Project => write!(f, "project"),
            Self::Builtin => write!(f, "builtin"),
        }
    }
}

/// A single configuration layer: a scope, an optional file path, and resolved mounts.
#[derive(Debug)]
pub struct ConfigLayer {
    pub scope: Scope,
    /// `None` for the default scope; `Some(path)` for file-backed scopes.
    pub path: Option<PathBuf>,
    pub mounts: Vec<ResolvedMount>,
}

/// All configuration layers, ordered from lowest to highest precedence.
///
/// The effective configuration comes from the highest-precedence layer
/// that is present.
#[derive(Debug)]
pub struct ScopedConfig {
    pub layers: Vec<ConfigLayer>,
}

impl ScopedConfig {
    /// Return merged mounts from all layers, de-duplicated by container target.
    ///
    /// Mounts accumulate across layers. When multiple layers define mounts with
    /// the same container target path, the higher-precedence layer wins.
    /// Each mount is tagged with the scope it came from.
    pub fn merged_mounts(&self) -> Vec<(Scope, &ResolvedMount)> {
        let mut seen = std::collections::HashSet::new();
        let mut result = Vec::new();
        // Iterate in reverse (highest precedence first) so higher layers win,
        // then reverse the result to preserve low-to-high ordering.
        for layer in self.layers.iter().rev() {
            for mount in layer.mounts.iter().rev() {
                if seen.insert(&mount.target) {
                    result.push((layer.scope, mount));
                }
            }
        }
        result.reverse();
        result
    }
}

/// A mount with tilde-expanded paths ready for Docker.
#[derive(Debug, PartialEq)]
pub struct ResolvedMount {
    pub source: PathBuf,
    pub target: String,
    pub writable: bool,
}

impl Config {
    /// Load all configuration layers for the given workspace.
    ///
    /// Layers are returned in precedence order (lowest first):
    /// 1. User (`~/.config/ramekin/config.kdl`) — only if the file exists
    /// 2. Project (`<workspace>/.ramekin/config.kdl`) — only if the file exists
    /// 3. Builtin (internal mounts passed by the caller)
    ///
    /// Returns an error if a config file exists but can't be parsed.
    pub fn load(workspace: &Path, builtin_mounts: Vec<ResolvedMount>) -> Result<ScopedConfig> {
        let mut layers = Vec::new();

        // User layer
        let xdg = xdg::BaseDirectories::with_prefix("ramekin");
        let user_path = xdg
            .place_config_file("config.kdl")
            .wrap_err("failed to determine user config path")?;

        if user_path.exists() {
            let config = Self::load_file(&user_path)
                .wrap_err_with(|| format!("failed to load user config: {}", user_path.display()))?;
            layers.push(ConfigLayer {
                scope: Scope::User,
                path: Some(user_path),
                mounts: config.resolve_mounts(),
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
                mounts: config.resolve_mounts(),
            });
        }

        // Builtin layer (always present, highest precedence)
        layers.push(ConfigLayer {
            scope: Scope::Builtin,
            path: None,
            mounts: builtin_mounts,
        });

        Ok(ScopedConfig { layers })
    }

    /// Parse a config file.
    fn load_file(path: &Path) -> Result<Self> {
        let content = fs_err::read_to_string(path).wrap_err("failed to read config file")?;
        serde_kdl2::from_str(&content).wrap_err("failed to parse config file")
    }

    /// Resolve all mounts, skipping any whose source directory does not exist.
    fn resolve_mounts(&self) -> Vec<ResolvedMount> {
        self.mounts.iter().filter_map(|m| m.resolve()).collect()
    }
}

impl Mount {
    /// Expand tildes and derive the container target path.
    ///
    /// Returns `None` if the source directory does not exist on the host.
    pub fn resolve(&self) -> Option<ResolvedMount> {
        let expanded = PathBuf::from(shellexpand::tilde(&self.source).as_ref());
        if !expanded.is_dir() {
            return None;
        }

        let target = match &self.target {
            Some(t) => t.clone(),
            None => tilde_to_root(&self.source),
        };

        Some(ResolvedMount {
            source: expanded,
            target,
            writable: self.writable,
        })
    }
}

impl ResolvedMount {
    /// Format as a Docker volume mount string (`source:target` or `source:target:ro`).
    pub fn to_volume_string(&self) -> String {
        if self.writable {
            format!("{}:{}", self.source.display(), self.target)
        } else {
            format!("{}:{}:ro", self.source.display(), self.target)
        }
    }

    /// Label for display in `config` output (target, with ` (ro)` suffix when read-only).
    pub fn display_target(&self) -> String {
        if self.writable {
            self.target.clone()
        } else {
            format!("{} (ro)", self.target)
        }
    }
}

/// Replace a leading `~` with `/root` to derive a container path.
fn tilde_to_root(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        format!("/root/{rest}")
    } else if path == "~" {
        "/root".to_string()
    } else {
        path.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn tilde_to_root_with_subpath() {
        assert_eq!(tilde_to_root("~/.config/git"), "/root/.config/git");
    }

    #[test]
    fn tilde_to_root_bare() {
        assert_eq!(tilde_to_root("~"), "/root");
    }

    #[test]
    fn tilde_to_root_absolute_unchanged() {
        assert_eq!(tilde_to_root("/some/path"), "/some/path");
    }

    #[test]
    fn resolve_skips_nonexistent_source() {
        let mount = Mount {
            source: "/nonexistent/path/that/does/not/exist".into(),
            target: None,
            writable: false,
        };
        assert!(mount.resolve().is_none());
    }

    #[test]
    fn resolve_with_existing_dir_and_explicit_target() {
        let mount = Mount {
            source: "/tmp".into(),
            target: Some("/container/tmp".into()),
            writable: false,
        };
        let resolved = mount.resolve().unwrap();
        assert_eq!(resolved.source, PathBuf::from("/tmp"));
        assert_eq!(resolved.target, "/container/tmp");
        assert!(!resolved.writable);
    }

    #[test]
    fn resolve_derives_target_from_source_when_no_tilde() {
        let mount = Mount {
            source: "/tmp".into(),
            target: None,
            writable: true,
        };
        let resolved = mount.resolve().unwrap();
        assert_eq!(resolved.target, "/tmp");
    }

    #[test]
    fn volume_string_read_only() {
        let m = ResolvedMount {
            source: PathBuf::from("/home/user/.config/git"),
            target: "/root/.config/git".into(),
            writable: false,
        };
        assert_eq!(
            m.to_volume_string(),
            "/home/user/.config/git:/root/.config/git:ro"
        );
    }

    #[test]
    fn volume_string_read_write() {
        let m = ResolvedMount {
            source: PathBuf::from("/home/user/.local/share/ranger"),
            target: "/root/.local/share/ranger".into(),
            writable: true,
        };
        assert_eq!(
            m.to_volume_string(),
            "/home/user/.local/share/ranger:/root/.local/share/ranger"
        );
    }

    #[test]
    fn display_target_read_only() {
        let m = ResolvedMount {
            source: PathBuf::from("/x"),
            target: "/root/.config/git".into(),
            writable: false,
        };
        assert_eq!(m.display_target(), "/root/.config/git (ro)");
    }

    #[test]
    fn display_target_read_write() {
        let m = ResolvedMount {
            source: PathBuf::from("/x"),
            target: "/root/.local/share/ranger".into(),
            writable: true,
        };
        assert_eq!(m.display_target(), "/root/.local/share/ranger");
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
        };
        let resolved = config.resolve_mounts();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].target, "/container/tmp");
    }

    #[test]
    fn scope_display() {
        assert_eq!(Scope::User.to_string(), "user");
        assert_eq!(Scope::Project.to_string(), "project");
        assert_eq!(Scope::Builtin.to_string(), "builtin");
    }

    #[test]
    fn merged_mounts_accumulates_across_layers() {
        let config = ScopedConfig {
            layers: vec![
                ConfigLayer {
                    scope: Scope::User,
                    path: Some(PathBuf::from("/user/config.kdl")),
                    mounts: vec![ResolvedMount {
                        source: PathBuf::from("/a"),
                        target: "/a".into(),
                        writable: false,
                    }],
                },
                ConfigLayer {
                    scope: Scope::Project,
                    path: Some(PathBuf::from("/project/config.kdl")),
                    mounts: vec![ResolvedMount {
                        source: PathBuf::from("/b"),
                        target: "/b".into(),
                        writable: true,
                    }],
                },
            ],
        };
        let merged = config.merged_mounts();
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].1.target, "/a");
        assert_eq!(merged[1].1.target, "/b");
    }

    #[test]
    fn merged_mounts_single_layer() {
        let config = ScopedConfig {
            layers: vec![ConfigLayer {
                scope: Scope::User,
                path: Some(PathBuf::from("/user/config.kdl")),
                mounts: vec![ResolvedMount {
                    source: PathBuf::from("/a"),
                    target: "/a".into(),
                    writable: false,
                }],
            }],
        };
        let merged = config.merged_mounts();
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].1.target, "/a");
    }

    #[test]
    fn merged_mounts_all_three_layers() {
        let config = ScopedConfig {
            layers: vec![
                ConfigLayer {
                    scope: Scope::User,
                    path: Some(PathBuf::from("/user/config.kdl")),
                    mounts: vec![ResolvedMount {
                        source: PathBuf::from("/a"),
                        target: "/a".into(),
                        writable: false,
                    }],
                },
                ConfigLayer {
                    scope: Scope::Project,
                    path: Some(PathBuf::from("/project/config.kdl")),
                    mounts: vec![ResolvedMount {
                        source: PathBuf::from("/b"),
                        target: "/b".into(),
                        writable: true,
                    }],
                },
                ConfigLayer {
                    scope: Scope::Builtin,
                    path: None,
                    mounts: vec![ResolvedMount {
                        source: PathBuf::from("/c"),
                        target: "/c".into(),
                        writable: true,
                    }],
                },
            ],
        };
        let merged = config.merged_mounts();
        assert_eq!(merged.len(), 3);
        assert_eq!(merged[0].1.target, "/a");
        assert_eq!(merged[1].1.target, "/b");
        assert_eq!(merged[2].1.target, "/c");
    }

    #[test]
    fn merged_mounts_deduplicates_by_target() {
        let config = ScopedConfig {
            layers: vec![
                ConfigLayer {
                    scope: Scope::User,
                    path: Some(PathBuf::from("/user/config.kdl")),
                    mounts: vec![
                        ResolvedMount {
                            source: PathBuf::from("/user/git"),
                            target: "/root/.config/git".into(),
                            writable: false,
                        },
                        ResolvedMount {
                            source: PathBuf::from("/user/jj"),
                            target: "/root/.config/jj".into(),
                            writable: false,
                        },
                    ],
                },
                ConfigLayer {
                    scope: Scope::Project,
                    path: Some(PathBuf::from("/project/config.kdl")),
                    mounts: vec![ResolvedMount {
                        // Override the user git mount with a different source
                        source: PathBuf::from("/project/git"),
                        target: "/root/.config/git".into(),
                        writable: true,
                    }],
                },
            ],
        };
        let merged = config.merged_mounts();
        // /root/.config/git appears in both layers; project layer wins
        assert_eq!(merged.len(), 2);
        // jj from user (not overridden)
        assert_eq!(merged[0].0, Scope::User);
        assert_eq!(merged[0].1.target, "/root/.config/jj");
        // git from project (overrides user)
        assert_eq!(merged[1].0, Scope::Project);
        assert_eq!(merged[1].1.target, "/root/.config/git");
        assert_eq!(merged[1].1.source, PathBuf::from("/project/git"));
        assert!(merged[1].1.writable);
    }

    #[test]
    fn load_with_no_config_files() {
        // Use a workspace with no .ramekin/config.kdl
        let config = Config::load(Path::new("/tmp"), vec![]).unwrap();
        // Should have only the builtin layer
        assert_eq!(config.layers.len(), 1);
        assert_eq!(config.layers[0].scope, Scope::Builtin);
    }

    #[test]
    fn load_with_project_config() {
        let dir = PathBuf::from("/tmp/ramekin-test-project-config");
        let ramekin_dir = dir.join(".ramekin");
        fs_err::create_dir_all(&ramekin_dir).unwrap();
        let config_path = ramekin_dir.join("config.kdl");
        // serde_kdl2 requires at least two repeated nodes for Vec deserialization
        fs_err::write(
            &config_path,
            "mounts{\nsource \"/tmp\"\ntarget \"/container/tmp\"\n}\nmounts{\nsource \"/tmp\"\ntarget \"/container/tmp2\"\n}\n",
        )
        .unwrap();

        let config = Config::load(&dir, vec![]).unwrap();

        // Clean up
        let _ = fs_err::remove_dir_all(&dir);

        // Should have project + builtin layers
        assert!(config.layers.len() >= 2);
        let project_layer = config
            .layers
            .iter()
            .find(|l| l.scope == Scope::Project)
            .expect("project layer missing");
        assert_eq!(project_layer.mounts.len(), 2);
        assert_eq!(project_layer.mounts[0].target, "/container/tmp");
        // Builtin is always last (highest precedence)
        assert_eq!(config.layers.last().unwrap().scope, Scope::Builtin);
    }
}
