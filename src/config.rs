use std::fmt;
use std::path::PathBuf;

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

/// Where a configuration was loaded from.
#[derive(Debug, PartialEq)]
pub enum ConfigSource {
    /// Hardcoded defaults (no config file found).
    Default,
    /// User-level config file at the given path.
    User(PathBuf),
}

impl fmt::Display for ConfigSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Default => write!(f, "default"),
            Self::User(path) => write!(f, "{}", path.display()),
        }
    }
}

/// A loaded configuration together with its source.
#[derive(Debug)]
pub struct ResolvedConfig {
    pub source: ConfigSource,
    pub mounts: Vec<ResolvedMount>,
}

/// A mount with tilde-expanded paths ready for Docker.
#[derive(Debug, PartialEq)]
pub struct ResolvedMount {
    pub source: PathBuf,
    pub target: String,
    pub writable: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self::fallback()
    }
}

impl Config {
    /// Load configuration and resolve mounts.
    ///
    /// Tries `~/.config/ramekin/config.kdl` first; falls back to hardcoded
    /// defaults when the file doesn't exist. Returns an error if the file
    /// exists but can't be parsed.
    pub fn load() -> Result<ResolvedConfig> {
        let xdg = xdg::BaseDirectories::with_prefix("ramekin");
        let config_path = xdg
            .place_config_file("config.kdl")
            .wrap_err("failed to determine config file path")?;

        if config_path.exists() {
            let content =
                fs_err::read_to_string(&config_path).wrap_err("failed to read config file")?;
            let config: Config =
                serde_kdl2::from_str(&content).wrap_err("failed to parse config file")?;
            let mounts = config.resolve_mounts();
            Ok(ResolvedConfig {
                source: ConfigSource::User(config_path),
                mounts,
            })
        } else {
            let config = Self::fallback();
            let mounts = config.resolve_mounts();
            Ok(ResolvedConfig {
                source: ConfigSource::Default,
                mounts,
            })
        }
    }

    /// Fallback configuration with hardcoded mounts.
    fn fallback() -> Self {
        Self {
            mounts: vec![
                Mount {
                    source: "~/.config/git".into(),
                    target: None,
                    writable: false,
                },
                Mount {
                    source: "~/.config/jj".into(),
                    target: None,
                    writable: false,
                },
                Mount {
                    source: "~/.local/share/ranger".into(),
                    target: None,
                    writable: true,
                },
            ],
        }
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
    fn fallback_has_three_mounts() {
        let config = Config::fallback();
        assert_eq!(config.mounts.len(), 3);
    }

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
    fn config_source_display_default() {
        let source = ConfigSource::Default;
        assert_eq!(source.to_string(), "default");
    }

    #[test]
    fn config_source_display_user() {
        let source = ConfigSource::User(PathBuf::from("/home/user/.config/ramekin/config.kdl"));
        assert_eq!(source.to_string(), "/home/user/.config/ramekin/config.kdl");
    }
}
