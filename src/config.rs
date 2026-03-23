use std::fmt;
use std::path::{Path, PathBuf};

use color_eyre::eyre::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub mounts: Vec<Mount>,
    #[serde(default)]
    pub pi: Vec<PiEntry>,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct PiEntry {
    pub source: String,
    pub target: Option<String>,
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
            Some(t) => resolve_container_tilde(t),
            None => resolve_container_tilde(&self.source),
        };

        Some(ResolvedMount {
            source: expanded,
            target,
            writable: self.writable,
        })
    }
}

/// A pi entry with its source path expanded and target resolved.
#[derive(Debug)]
pub struct ResolvedPiEntry {
    pub source: PathBuf,
    /// Target path relative to the agent dir.
    pub target: String,
}

impl PiEntry {
    /// Expand tildes and resolve the target.
    ///
    /// Uses the explicit target when set, otherwise falls back to
    /// the source's basename.
    pub fn resolve(&self) -> ResolvedPiEntry {
        let source = PathBuf::from(shellexpand::tilde(&self.source).as_ref());
        let target = self.target.clone().unwrap_or_else(|| {
            source
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        });
        ResolvedPiEntry { source, target }
    }
}

/// Clear the agent directory, preserving only `auth.json`.
pub fn clear_agent_dir(agent_dir: &Path) -> Result<()> {
    if !agent_dir.exists() {
        return Ok(());
    }
    for entry in fs_err::read_dir(agent_dir)? {
        let entry = entry?;
        if entry.file_name() == "auth.json" {
            continue;
        }
        if entry.file_type()?.is_dir() {
            fs_err::remove_dir_all(entry.path())?;
        } else {
            fs_err::remove_file(entry.path())?;
        }
    }
    Ok(())
}

/// Copy pi config entries into the agent directory.
///
/// Auto-detects file vs directory from the source. Warns and skips
/// sources that don't exist on the host.
pub fn assemble_pi(agent_dir: &Path, entries: &[ResolvedPiEntry]) -> Result<()> {
    for entry in entries {
        if !entry.source.exists() {
            tracing::warn!(
                source = %entry.source.display(),
                target = %entry.target,
                "pi source does not exist, skipping"
            );
            continue;
        }

        let target = agent_dir.join(&entry.target);

        if entry.source.is_dir() {
            copy_dir(&entry.source, &target)?;
        } else {
            fs_err::copy(&entry.source, &target)?;
        }
    }
    Ok(())
}

/// Recursively copy a directory tree.
fn copy_dir(src: &Path, dst: &Path) -> Result<()> {
    fs_err::create_dir_all(dst)?;
    for entry in fs_err::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let target = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            fs_err::copy(entry.path(), &target)?;
        }
    }
    Ok(())
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

/// Home directory inside the agent container. The ramekin Dockerfile runs
/// everything as root, so `~` in container target paths maps here. If the
/// image ever switches to a non-root user, update this constant.
const CONTAINER_HOME: &str = "/root";

/// Replace a leading `~` with the container home directory.
fn resolve_container_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        format!("{CONTAINER_HOME}/{rest}")
    } else if path == "~" {
        CONTAINER_HOME.to_string()
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
    fn resolve_container_tilde_with_subpath() {
        assert_eq!(
            resolve_container_tilde("~/.config/git"),
            "/root/.config/git"
        );
    }

    #[test]
    fn resolve_container_tilde_bare() {
        assert_eq!(resolve_container_tilde("~"), "/root");
    }

    #[test]
    fn resolve_container_tilde_absolute_unchanged() {
        assert_eq!(resolve_container_tilde("/some/path"), "/some/path");
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
    fn resolve_expands_tilde_in_explicit_target() {
        let mount = Mount {
            source: "/tmp".into(),
            target: Some("~/downloads".into()),
            writable: true,
        };
        let resolved = mount.resolve().unwrap();
        assert_eq!(resolved.target, "/root/downloads");
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
            pi: vec![],
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
    fn load_with_no_project_config() {
        // Use a workspace with no .ramekin/config.kdl
        let config = Config::load(Path::new("/tmp"), vec![]).unwrap();
        // Builtin layer is always last
        assert_eq!(config.layers.last().unwrap().scope, Scope::Builtin);
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

        let config = Config::load(dir.path(), vec![]).unwrap();

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

    #[test]
    fn kdl_pi_entries() {
        let kdl = r#"
            pi {
                source "~/.dotfiles/ai/AGENTS.md"
            }
            pi {
                source "~/.dotfiles/ai/skills"
            }
        "#;
        let parsed: Config = serde_kdl2::from_str(kdl).unwrap();
        assert!(parsed.mounts.is_empty());
        assert_eq!(parsed.pi.len(), 2);
        assert_eq!(parsed.pi[0].source, "~/.dotfiles/ai/AGENTS.md");
        assert_eq!(parsed.pi[0].target, None);
        assert_eq!(parsed.pi[1].source, "~/.dotfiles/ai/skills");
    }

    #[test]
    fn kdl_pi_with_explicit_target() {
        let kdl = r#"
            pi {
                source "~/.dotfiles/ai/my-project-skills"
                target "skills"
            }
            pi {
                source "~/.dotfiles/ai/AGENTS.md"
            }
        "#;
        let parsed: Config = serde_kdl2::from_str(kdl).unwrap();
        assert_eq!(parsed.pi.len(), 2);
        assert_eq!(parsed.pi[0].target, Some("skills".into()));
        assert_eq!(parsed.pi[1].target, None);
    }

    #[test]
    fn kdl_no_pi_section() {
        let kdl = r#"
            mounts {
                source "/tmp"
                target "/container/tmp"
            }
            mounts {
                source "/tmp"
                target "/container/tmp2"
            }
        "#;
        let parsed: Config = serde_kdl2::from_str(kdl).unwrap();
        assert!(parsed.pi.is_empty());
        assert_eq!(parsed.mounts.len(), 2);
    }

    #[test]
    fn kdl_pi_and_mounts_together() {
        let kdl = r#"
            mounts {
                source "~/.config/git"
            }
            mounts {
                source "~/.config/jj"
            }
            pi {
                source "~/.dotfiles/ai/AGENTS.md"
            }
        "#;
        let parsed: Config = serde_kdl2::from_str(kdl).unwrap();
        assert_eq!(parsed.mounts.len(), 2);
        assert_eq!(parsed.pi.len(), 1);
        assert_eq!(parsed.pi[0].source, "~/.dotfiles/ai/AGENTS.md");
    }

    #[test]
    fn clear_agent_dir_preserves_auth_json() {
        let agent_dir = tempfile::tempdir().unwrap();

        fs_err::write(agent_dir.path().join("auth.json"), "secret").unwrap();
        fs_err::write(agent_dir.path().join("AGENTS.md"), "old").unwrap();
        fs_err::write(agent_dir.path().join("settings.json"), "{}").unwrap();
        fs_err::create_dir_all(agent_dir.path().join("skills")).unwrap();
        fs_err::write(agent_dir.path().join("skills/x.md"), "x").unwrap();

        clear_agent_dir(agent_dir.path()).unwrap();

        assert!(agent_dir.path().join("auth.json").exists());
        assert_eq!(
            fs_err::read_to_string(agent_dir.path().join("auth.json")).unwrap(),
            "secret"
        );
        assert!(!agent_dir.path().join("AGENTS.md").exists());
        assert!(!agent_dir.path().join("settings.json").exists());
        assert!(!agent_dir.path().join("skills").exists());
    }

    #[test]
    fn clear_agent_dir_handles_empty_dir() {
        let agent_dir = tempfile::tempdir().unwrap();
        clear_agent_dir(agent_dir.path()).unwrap();
    }

    #[test]
    fn clear_agent_dir_handles_nonexistent_dir() {
        clear_agent_dir(Path::new("/nonexistent/path")).unwrap();
    }

    #[test]
    fn assemble_pi_copies_file() {
        let src_dir = tempfile::tempdir().unwrap();
        let agent_dir = tempfile::tempdir().unwrap();

        let prompt = src_dir.path().join("AGENTS.md");
        fs_err::write(&prompt, "# my prompt").unwrap();

        let entries = vec![ResolvedPiEntry {
            source: prompt,
            target: "AGENTS.md".into(),
        }];

        assemble_pi(agent_dir.path(), &entries).unwrap();

        assert_eq!(
            fs_err::read_to_string(agent_dir.path().join("AGENTS.md")).unwrap(),
            "# my prompt"
        );
    }

    #[test]
    fn assemble_pi_copies_directory() {
        let src_dir = tempfile::tempdir().unwrap();
        let agent_dir = tempfile::tempdir().unwrap();

        let skills = src_dir.path().join("skills");
        fs_err::create_dir_all(skills.join("my-skill")).unwrap();
        fs_err::write(skills.join("my-skill/SKILL.md"), "# skill").unwrap();

        let entries = vec![ResolvedPiEntry {
            source: skills,
            target: "skills".into(),
        }];

        assemble_pi(agent_dir.path(), &entries).unwrap();

        assert_eq!(
            fs_err::read_to_string(agent_dir.path().join("skills/my-skill/SKILL.md")).unwrap(),
            "# skill"
        );
    }

    #[test]
    fn assemble_pi_skips_missing_source() {
        let agent_dir = tempfile::tempdir().unwrap();

        let entries = vec![ResolvedPiEntry {
            source: PathBuf::from("/nonexistent/file"),
            target: "file".into(),
        }];

        assemble_pi(agent_dir.path(), &entries).unwrap();
        assert!(!agent_dir.path().join("file").exists());
    }

    #[test]
    fn pi_resolve_defaults_target_to_basename() {
        let entry = PiEntry {
            source: "~/.dotfiles/ai/AGENTS.md".into(),
            target: None,
        };
        let resolved = entry.resolve();
        assert_eq!(resolved.target, "AGENTS.md");
    }

    #[test]
    fn pi_resolve_defaults_target_to_directory_basename() {
        let entry = PiEntry {
            source: "~/.dotfiles/ai/skills".into(),
            target: None,
        };
        let resolved = entry.resolve();
        assert_eq!(resolved.target, "skills");
    }

    #[test]
    fn pi_resolve_uses_explicit_target() {
        let entry = PiEntry {
            source: "~/.dotfiles/ai/my-project-skills".into(),
            target: Some("skills".into()),
        };
        let resolved = entry.resolve();
        assert_eq!(resolved.target, "skills");
    }

    #[test]
    fn clear_then_assemble_full_cycle() {
        let src_dir = tempfile::tempdir().unwrap();
        let agent_dir = tempfile::tempdir().unwrap();

        // Simulate previous run state.
        fs_err::write(agent_dir.path().join("auth.json"), "secret").unwrap();
        fs_err::write(agent_dir.path().join("AGENTS.md"), "old prompt").unwrap();
        fs_err::create_dir_all(agent_dir.path().join("old-skills")).unwrap();

        // New config only has a prompt.
        let prompt = src_dir.path().join("AGENTS.md");
        fs_err::write(&prompt, "new prompt").unwrap();

        clear_agent_dir(agent_dir.path()).unwrap();

        let entries = vec![ResolvedPiEntry {
            source: prompt,
            target: "AGENTS.md".into(),
        }];
        assemble_pi(agent_dir.path(), &entries).unwrap();

        // auth.json preserved.
        assert_eq!(
            fs_err::read_to_string(agent_dir.path().join("auth.json")).unwrap(),
            "secret"
        );
        // New prompt copied.
        assert_eq!(
            fs_err::read_to_string(agent_dir.path().join("AGENTS.md")).unwrap(),
            "new prompt"
        );
        // Old stale dir gone.
        assert!(!agent_dir.path().join("old-skills").exists());
    }
}
