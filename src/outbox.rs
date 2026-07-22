//! The outbox: the one reviewed path for stateful modification of shared
//! config.
//!
//! Config is read-only inside the container by design. When an agent wants
//! a config change, it writes the changed file into its session's outbox
//! dir (mounted writable at [`OUTBOX_TARGET`]), mirroring the agent config
//! layout. Host side, `ramekin outbox` lists pending proposals, diffs them
//! against the host source each entry was mounted from, and applies or
//! discards them. Nothing reaches host config without an explicit apply.

use std::path::{Component, Path, PathBuf};

use miette::{IntoDiagnostic, Result, bail};

use crate::config::Agent;

/// Container path of the session's writable outbox dir — the only
/// agent-writable path outside the workspace and the agent state mounts.
pub const OUTBOX_TARGET: &str = "/root/.ramekin/outbox";

/// Host paths for one session's outbox: the mounted dir and its sidecar
/// metadata file recording which agent the session ran (the same relative
/// path maps to different host config dirs per agent).
fn session_paths(data_home: &Path, slug: &str, session_id: &str) -> (PathBuf, PathBuf) {
    let outbox = data_home.join(format!("repos/{slug}/outbox"));
    (
        outbox.join(session_id),
        outbox.join(format!("{session_id}.agent")),
    )
}

/// Create a fresh, empty outbox dir for a session, plus its agent sidecar.
/// The sidecar sits *beside* the mounted dir, out of the agent's reach, so
/// a confused or malicious proposal can't redirect where apply maps it.
pub fn create_session(
    data_home: &Path,
    slug: &str,
    session_id: &str,
    agent: Agent,
) -> Result<PathBuf> {
    let (dir, meta) = session_paths(data_home, slug, session_id);
    fs_err::create_dir_all(&dir).into_diagnostic()?;
    fs_err::write(&meta, agent.name()).into_diagnostic()?;
    Ok(dir)
}

/// Session teardown: drop the outbox if the agent left nothing in it,
/// keep it (returning the pending count) otherwise.
pub fn finish_session(data_home: &Path, slug: &str, session_id: &str) -> Result<usize> {
    let (dir, meta) = session_paths(data_home, slug, session_id);
    let mut files = Vec::new();
    collect_files(&dir, &dir, &mut files)?;
    if files.is_empty() {
        fs_err::remove_dir_all(&dir).into_diagnostic()?;
        fs_err::remove_file(&meta).into_diagnostic()?;
    }
    Ok(files.len())
}

/// One proposed file in some session's outbox.
#[derive(Debug)]
pub struct Proposal {
    pub slug: String,
    pub session: String,
    /// Path relative to the session outbox dir, mirroring the agent config
    /// layout.
    pub rel: PathBuf,
    /// The agent the session ran, from the sidecar. `None` if the sidecar
    /// is missing or unparseable.
    pub agent: Option<Agent>,
    /// Absolute host path of the proposal file.
    pub file: PathBuf,
}

impl Proposal {
    /// The address `ramekin outbox` commands take: `<slug>/<session>/<rel>`.
    pub fn entry(&self) -> String {
        format!("{}/{}/{}", self.slug, self.session, self.rel.display())
    }

    /// The host config file this proposal maps back to: the agent's host
    /// config dir plus the relative path — but only when the path's first
    /// component is an allowlisted entry, i.e. something that was actually
    /// mounted. Anything else needs an explicit destination to apply.
    pub fn host_target(&self) -> Option<PathBuf> {
        let agent = self.agent?;
        let first = match self.rel.components().next()? {
            Component::Normal(name) => name.to_str()?.to_string(),
            _ => return None,
        };
        if !agent.config_allowlist().contains(&first.as_str()) {
            return None;
        }
        let base = PathBuf::from(shellexpand::tilde(agent.host_config_dir()).as_ref());
        Some(base.join(&self.rel))
    }
}

/// All pending proposals across every repo and session, oldest path first.
pub fn scan(data_home: &Path) -> Result<Vec<Proposal>> {
    let mut proposals = Vec::new();
    let repos = data_home.join("repos");
    if !repos.is_dir() {
        return Ok(proposals);
    }
    for repo in sorted_dir(&repos)? {
        let Some(slug) = dir_name(&repo) else {
            continue;
        };
        let outbox = repo.join("outbox");
        if !outbox.is_dir() {
            continue;
        }
        for session_dir in sorted_dir(&outbox)? {
            if !session_dir.is_dir() {
                continue; // .agent sidecars
            }
            let Some(session) = dir_name(&session_dir) else {
                continue;
            };
            let agent = fs_err::read_to_string(outbox.join(format!("{session}.agent")))
                .ok()
                .and_then(|s| Agent::parse(s.trim()).ok());
            let mut files = Vec::new();
            collect_files(&session_dir, &session_dir, &mut files)?;
            for rel in files {
                proposals.push(Proposal {
                    slug: slug.clone(),
                    session: session.clone(),
                    file: session_dir.join(&rel),
                    rel,
                    agent,
                });
            }
        }
    }
    Ok(proposals)
}

/// Proposals matching an entry: either one file (`<slug>/<session>/<rel>`)
/// or a whole session (`<slug>/<session>`).
pub fn find(data_home: &Path, entry: &str) -> Result<Vec<Proposal>> {
    let matches: Vec<Proposal> = scan(data_home)?
        .into_iter()
        .filter(|p| {
            let session_prefix = format!("{}/{}", p.slug, p.session);
            p.entry() == entry || session_prefix == entry
        })
        .collect();
    if matches.is_empty() {
        bail!("no outbox entry matches `{entry}` (see `ramekin outbox list`)");
    }
    Ok(matches)
}

/// Remove a proposal file and prune its session outbox if now empty.
pub fn remove(data_home: &Path, proposal: &Proposal) -> Result<()> {
    fs_err::remove_file(&proposal.file).into_diagnostic()?;
    // Prune now-empty parent dirs up to (and including, via finish) the
    // session dir.
    let (session_dir, _) = session_paths(data_home, &proposal.slug, &proposal.session);
    let mut dir = proposal.file.parent().map(Path::to_path_buf);
    while let Some(d) = dir {
        if d == session_dir || fs_err::remove_dir(&d).is_err() {
            break;
        }
        dir = d.parent().map(Path::to_path_buf);
    }
    finish_session(data_home, &proposal.slug, &proposal.session)?;
    Ok(())
}

/// Recursively collect files under `dir` as paths relative to `root`.
fn collect_files(dir: &Path, root: &Path, found: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs_err::read_dir(dir).into_diagnostic()? {
        let entry = entry.into_diagnostic()?;
        let path = entry.path();
        if entry.file_type().into_diagnostic()?.is_dir() {
            collect_files(&path, root, found)?;
        } else {
            found.push(
                path.strip_prefix(root)
                    .expect("walk stays under root")
                    .to_path_buf(),
            );
        }
    }
    found.sort();
    Ok(())
}

fn sorted_dir(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut entries: Vec<PathBuf> = fs_err::read_dir(dir)
        .into_diagnostic()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    entries.sort();
    Ok(entries)
}

fn dir_name(path: &Path) -> Option<String> {
    path.file_name().map(|n| n.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_proposal(data_home: &Path, slug: &str, session: &str, agent: &str, rel: &str) {
        let (dir, meta) = session_paths(data_home, slug, session);
        let file = dir.join(rel);
        fs_err::create_dir_all(file.parent().unwrap()).unwrap();
        fs_err::write(&file, "proposed").unwrap();
        fs_err::write(&meta, agent).unwrap();
    }

    #[test]
    fn create_then_finish_empty_session_leaves_nothing() {
        let data_home = tempfile::tempdir().unwrap();
        let dir = create_session(data_home.path(), "repo-1", "abc", Agent::Pi).unwrap();
        assert!(dir.is_dir());

        let pending = finish_session(data_home.path(), "repo-1", "abc").unwrap();
        assert_eq!(pending, 0);
        assert!(!dir.exists());
        assert!(scan(data_home.path()).unwrap().is_empty());
    }

    #[test]
    fn finish_keeps_nonempty_session() {
        let data_home = tempfile::tempdir().unwrap();
        let dir = create_session(data_home.path(), "repo-1", "abc", Agent::Pi).unwrap();
        fs_err::write(dir.join("AGENTS.md"), "new").unwrap();

        let pending = finish_session(data_home.path(), "repo-1", "abc").unwrap();
        assert_eq!(pending, 1);
        assert!(dir.exists());
    }

    #[test]
    fn scan_finds_proposals_with_agent() {
        let data_home = tempfile::tempdir().unwrap();
        write_proposal(
            data_home.path(),
            "repo-1",
            "abc",
            "pi",
            "skills/foo/SKILL.md",
        );

        let proposals = scan(data_home.path()).unwrap();
        assert_eq!(proposals.len(), 1);
        let p = &proposals[0];
        assert_eq!(p.slug, "repo-1");
        assert_eq!(p.session, "abc");
        assert_eq!(p.agent, Some(Agent::Pi));
        assert_eq!(p.entry(), "repo-1/abc/skills/foo/SKILL.md");
        let target = p.host_target().unwrap();
        assert!(
            target.ends_with(".pi/agent/skills/foo/SKILL.md"),
            "got {}",
            target.display()
        );
    }

    #[test]
    fn claude_proposal_maps_to_claude_dir() {
        let data_home = tempfile::tempdir().unwrap();
        write_proposal(data_home.path(), "repo-1", "abc", "claude", "CLAUDE.md");

        let proposals = scan(data_home.path()).unwrap();
        let target = proposals[0].host_target().unwrap();
        assert!(
            target.ends_with(".claude/CLAUDE.md"),
            "got {}",
            target.display()
        );
    }

    #[test]
    fn unallowlisted_proposal_has_no_host_target() {
        let data_home = tempfile::tempdir().unwrap();
        write_proposal(data_home.path(), "repo-1", "abc", "pi", "settings.json");

        let proposals = scan(data_home.path()).unwrap();
        // settings.json is claude-shaped, not in pi's allowlist.
        assert_eq!(proposals[0].host_target(), None);
    }

    #[test]
    fn missing_sidecar_means_no_target() {
        let data_home = tempfile::tempdir().unwrap();
        let (dir, _) = session_paths(data_home.path(), "repo-1", "abc");
        fs_err::create_dir_all(&dir).unwrap();
        fs_err::write(dir.join("AGENTS.md"), "x").unwrap();

        let proposals = scan(data_home.path()).unwrap();
        assert_eq!(proposals.len(), 1);
        assert_eq!(proposals[0].agent, None);
        assert_eq!(proposals[0].host_target(), None);
    }

    #[test]
    fn find_matches_file_and_session() {
        let data_home = tempfile::tempdir().unwrap();
        write_proposal(data_home.path(), "repo-1", "abc", "pi", "AGENTS.md");
        write_proposal(data_home.path(), "repo-1", "abc", "pi", "skills/x.md");

        let by_file = find(data_home.path(), "repo-1/abc/AGENTS.md").unwrap();
        assert_eq!(by_file.len(), 1);

        let by_session = find(data_home.path(), "repo-1/abc").unwrap();
        assert_eq!(by_session.len(), 2);

        assert!(find(data_home.path(), "repo-1/nope").is_err());
    }

    #[test]
    fn remove_prunes_empty_dirs_and_session() {
        let data_home = tempfile::tempdir().unwrap();
        write_proposal(
            data_home.path(),
            "repo-1",
            "abc",
            "pi",
            "skills/foo/SKILL.md",
        );

        let proposals = scan(data_home.path()).unwrap();
        remove(data_home.path(), &proposals[0]).unwrap();

        let (dir, meta) = session_paths(data_home.path(), "repo-1", "abc");
        assert!(!dir.exists());
        assert!(!meta.exists());
    }
}
