//! Container runtimes: how an image gets built and a session gets run.

use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};

use miette::{Context, IntoDiagnostic, Result, bail};
use serde::Serialize;
use tracing::error;

use crate::config;

/// The container runtime ramekin drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Runtime {
    Docker,
}

/// An image to build.
pub struct Build<'a> {
    pub tag: &'a str,
    pub dockerfile: &'a Path,
    pub context: &'a Path,
    pub no_cache: bool,
    /// Pull a fresh base image. Never set for a project image: its base,
    /// `ramekin-agent`, exists only locally.
    pub pull: bool,
    /// Host token exposed to the build as the `github-token` secret.
    pub github_token: Option<&'a str>,
}

/// A target hidden from the agent. Kept apart from binds because runtimes
/// hide things differently, and Docker's way depends on whether a directory
/// or a file is being hidden.
pub struct Mask {
    pub target: String,
    pub over_dir: bool,
    pub writable: bool,
}

/// One agent session, ready to run.
pub struct Session<'a> {
    /// Unique per session, so concurrent runs don't collide.
    pub name: &'a str,
    /// Session-scoped scratch dir for files the runtime needs.
    pub dir: &'a Path,
    pub image: &'a str,
    pub working_dir: &'a str,
    /// The agent binary to run — the image carries both, with no ENTRYPOINT
    /// of its own.
    pub entrypoint: &'a str,
    pub command: &'a [String],
    /// Ordered lexicographically by target so parents precede children.
    pub binds: &'a [config::ResolvedMount],
    pub masks: &'a [Mask],
    pub env: &'a [config::ScopedValue<&'a config::EnvVar>],
}

impl Runtime {
    pub fn build(&self, build: &Build) -> Result<()> {
        let mut cmd = Command::new(self.program());
        cmd.args(["build", "-t", build.tag, "-f"])
            .arg(build.dockerfile);
        if let Some(token) = build.github_token {
            cmd.env("RAMEKIN_GH_TOKEN", token)
                .args(["--secret", "id=github-token,env=RAMEKIN_GH_TOKEN"]);
        }
        if build.no_cache {
            cmd.arg("--no-cache");
        }
        if build.pull {
            cmd.arg("--pull");
        }
        cmd.arg(build.context);
        let status = cmd
            .status()
            .into_diagnostic()
            .wrap_err_with(|| format!("failed to run `{} build`", self.program()))?;
        if !status.success() {
            bail!("building image {} failed ({status})", build.tag);
        }
        Ok(())
    }

    /// Run the session attached to the terminal and tear it down afterwards,
    /// returning the agent's exit status.
    pub fn run(&self, session: &Session) -> Result<ExitStatus> {
        match self {
            Self::Docker => docker_run(session),
        }
    }

    fn program(&self) -> &'static str {
        match self {
            Self::Docker => "docker",
        }
    }
}

fn docker_run(session: &Session) -> Result<ExitStatus> {
    let empty_dir = session.dir.join("empty");
    fs_err::create_dir_all(&empty_dir).into_diagnostic()?;
    let compose_file = session.dir.join("compose.yml");
    fs_err::write(&compose_file, generate_compose(session, &empty_dir)).into_diagnostic()?;

    let compose = |args: &[&str]| {
        let mut cmd = Command::new("docker");
        cmd.args(["compose", "-f"])
            .arg(&compose_file)
            .args(["--project-name", session.name])
            .args(args);
        cmd
    };

    let status = compose(&["up", "-d"])
        .status()
        .into_diagnostic()
        .wrap_err("failed to run docker compose up")?;
    if !status.success() {
        bail!("docker compose up failed ({})", status);
    }

    let status = compose(&["attach", "agent"])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .into_diagnostic()
        .wrap_err("failed to attach to agent")?;

    // Always tear down, regardless of attach exit status.
    let down_status = compose(&["down"])
        .status()
        .into_diagnostic()
        .wrap_err("failed to run docker compose down")?;
    if !down_status.success() {
        error!("docker compose down failed ({})", down_status);
    }

    Ok(status)
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
    image: String,
    stdin_open: bool,
    tty: bool,
    working_dir: String,
    entrypoint: Vec<String>,
    environment: Vec<String>,
    volumes: Vec<VolumeBind>,
    command: Vec<String>,
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

/// Generate a Docker Compose config for the session.
///
/// A mask binds `/dev/null` over a file, but over a directory Docker refuses
/// that mount and the run dies at startup, so a directory mask binds
/// `empty_dir` instead: listing the target succeeds and comes back empty.
fn generate_compose(session: &Session, empty_dir: &Path) -> String {
    let binds = session.binds.iter().map(|m| VolumeBind {
        kind: "bind",
        source: m.source.display().to_string(),
        target: m.target.clone(),
        read_only: !m.writable,
    });
    let masks = session.masks.iter().map(|m| VolumeBind {
        kind: "bind",
        source: if m.over_dir {
            empty_dir.display().to_string()
        } else {
            config::MASK_SOURCE.to_string()
        },
        target: m.target.clone(),
        read_only: !m.writable,
    });
    // Docker processes mounts in order, so a parent declared after its child
    // would shadow it; sorting by target puts parents first.
    let mut volumes: Vec<VolumeBind> = binds.chain(masks).collect();
    volumes.sort_by(|a, b| a.target.cmp(&b.target));

    // A bare name (no value) is compose's passthrough form: the variable is
    // forwarded from the environment ramekin runs in, and stays unset in the
    // container when the host doesn't have it either.
    let environment = session
        .env
        .iter()
        .map(|sv| match &sv.value.value {
            Some(value) => format!("{}={value}", sv.value.name),
            None => sv.value.name.clone(),
        })
        .collect();

    let config = ComposeConfig {
        services: Services {
            agent: AgentService {
                image: session.image.to_string(),
                stdin_open: true,
                tty: true,
                working_dir: session.working_dir.to_string(),
                entrypoint: vec![session.entrypoint.to_string()],
                environment,
                volumes,
                command: session.command.to_vec(),
            },
        },
    };

    serde_yaml::to_string(&config).expect("failed to serialize compose config")
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn session<'a>(
        binds: &'a [config::ResolvedMount],
        masks: &'a [Mask],
        env: &'a [config::ScopedValue<&'a config::EnvVar>],
    ) -> Session<'a> {
        Session {
            name: "ramekin-test",
            dir: Path::new("/session"),
            image: "ramekin-agent",
            working_dir: "/workspace/x-1",
            entrypoint: "pi",
            command: &[],
            binds,
            masks,
            env,
        }
    }

    fn compose(session: &Session) -> String {
        generate_compose(session, Path::new("/session/empty"))
    }

    #[test]
    fn generate_compose_long_form_binds() {
        let mount = config::ResolvedMount {
            source: PathBuf::from("/host/.config/git"),
            target: "/root/.config/git".into(),
            writable: false,
        };
        let yaml = compose(&session(&[mount], &[], &[]));
        assert!(yaml.contains("type: bind"), "{yaml}");
        assert!(yaml.contains("source: /host/.config/git"), "{yaml}");
        assert!(yaml.contains("target: /root/.config/git"), "{yaml}");
        assert!(yaml.contains("read_only: true"), "{yaml}");
        assert!(yaml.contains("working_dir: /workspace/x-1"), "{yaml}");
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
        let yaml = compose(&session(&[], &[], &env));
        assert!(yaml.contains("- FOO=bar"), "{yaml}");
        assert!(yaml.contains("- GITHUB_TOKEN\n"), "{yaml}");
        assert!(!yaml.contains("GITHUB_TOKEN="), "{yaml}");
    }

    #[test]
    fn generate_compose_sets_the_agent_entrypoint() {
        let mut session = session(&[], &[], &[]);
        session.entrypoint = "claude";
        let yaml = compose(&session);
        assert!(yaml.contains("entrypoint:\n    - claude"), "{yaml}");
    }

    #[test]
    fn directory_mask_binds_an_empty_dir() {
        let mask = Mask {
            target: "/workspace/slug/node_modules".into(),
            over_dir: true,
            writable: false,
        };
        let yaml = compose(&session(&[], &[mask], &[]));
        assert!(yaml.contains("source: /session/empty"), "{yaml}");
        assert!(!yaml.contains("/dev/null"), "{yaml}");
    }

    #[test]
    fn file_mask_binds_dev_null() {
        let mask = Mask {
            target: "/workspace/slug/.envrc".into(),
            over_dir: false,
            writable: false,
        };
        let yaml = compose(&session(&[], &[mask], &[]));
        assert!(yaml.contains("source: /dev/null"), "{yaml}");
    }

    #[test]
    fn masks_sort_after_the_binds_they_sit_inside() {
        let workspace = config::ResolvedMount {
            source: PathBuf::from("/host/repo"),
            target: "/workspace/slug".into(),
            writable: true,
        };
        let mask = Mask {
            target: "/workspace/slug/.envrc".into(),
            over_dir: false,
            writable: false,
        };
        let yaml = compose(&session(&[workspace], &[mask], &[]));
        let parent = yaml.find("target: /workspace/slug\n").expect("{yaml}");
        let child = yaml.find("target: /workspace/slug/.envrc").expect("{yaml}");
        assert!(parent < child, "{yaml}");
    }
}
