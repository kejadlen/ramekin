use std::path::PathBuf;

#[derive(Debug, PartialEq)]
pub struct Config {
    pub mounts: Vec<Mount>,
}

#[derive(Debug, PartialEq)]
pub struct Mount {
    pub source: String,
    pub target: Option<String>,
    pub read_only: bool,
}

/// A mount with tilde-expanded paths ready for Docker.
#[derive(Debug, PartialEq)]
pub struct ResolvedMount {
    pub source: PathBuf,
    pub target: String,
    pub read_only: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            mounts: vec![
                Mount {
                    source: "~/.config/git".into(),
                    target: None,
                    read_only: true,
                },
                Mount {
                    source: "~/.config/jj".into(),
                    target: None,
                    read_only: true,
                },
                Mount {
                    source: "~/.local/share/ranger".into(),
                    target: None,
                    read_only: false,
                },
            ],
        }
    }
}

impl Config {
    /// Resolve all mounts, skipping any whose source directory does not exist.
    pub fn resolve_mounts(&self) -> Vec<ResolvedMount> {
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
            read_only: self.read_only,
        })
    }
}

impl ResolvedMount {
    /// Format as a Docker volume mount string (`source:target` or `source:target:ro`).
    pub fn to_volume_string(&self) -> String {
        if self.read_only {
            format!("{}:{}:ro", self.source.display(), self.target)
        } else {
            format!("{}:{}", self.source.display(), self.target)
        }
    }

    /// Label for display in `config` output (target, with ` (ro)` suffix when read-only).
    pub fn display_target(&self) -> String {
        if self.read_only {
            format!("{} (ro)", self.target)
        } else {
            self.target.clone()
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
    fn default_has_three_mounts() {
        let config = Config::default();
        assert_eq!(config.mounts.len(), 3);
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
            read_only: false,
        };
        assert!(mount.resolve().is_none());
    }

    #[test]
    fn resolve_with_existing_dir_and_explicit_target() {
        let mount = Mount {
            source: "/tmp".into(),
            target: Some("/container/tmp".into()),
            read_only: true,
        };
        let resolved = mount.resolve().unwrap();
        assert_eq!(resolved.source, PathBuf::from("/tmp"));
        assert_eq!(resolved.target, "/container/tmp");
        assert!(resolved.read_only);
    }

    #[test]
    fn resolve_derives_target_from_source_when_no_tilde() {
        let mount = Mount {
            source: "/tmp".into(),
            target: None,
            read_only: false,
        };
        let resolved = mount.resolve().unwrap();
        assert_eq!(resolved.target, "/tmp");
    }

    #[test]
    fn volume_string_read_only() {
        let m = ResolvedMount {
            source: PathBuf::from("/home/user/.config/git"),
            target: "/root/.config/git".into(),
            read_only: true,
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
            read_only: false,
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
            read_only: true,
        };
        assert_eq!(m.display_target(), "/root/.config/git (ro)");
    }

    #[test]
    fn display_target_read_write() {
        let m = ResolvedMount {
            source: PathBuf::from("/x"),
            target: "/root/.local/share/ranger".into(),
            read_only: false,
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
                    read_only: false,
                },
                Mount {
                    source: "/nonexistent".into(),
                    target: None,
                    read_only: false,
                },
            ],
        };
        let resolved = config.resolve_mounts();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].target, "/container/tmp");
    }
}
