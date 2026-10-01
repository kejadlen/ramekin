//! The outbox: the one channel from an agent back to its host config.
//!
//! Config is read-only inside the container by design. When an agent hits
//! a config problem, it writes a Markdown report describing it into its
//! session's outbox dir (mounted writable at [`OUTBOX_TARGET`]). Host side,
//! `ramekin outbox` lists, shows, and discards pending reports; any fix to
//! host config happens there, outside the container.

use std::path::{Path, PathBuf};

use miette::{IntoDiagnostic, Result, bail};

use crate::config::Agent;

/// Container path of the session's writable outbox dir — the only
/// agent-writable path outside the workspace and the agent state mounts.
pub const OUTBOX_TARGET: &str = "/root/.ramekin/outbox";

/// Host paths for one session's outbox: the mounted dir and its sidecar
/// metadata file recording which agent the session ran, i.e. whose config
/// the reports are about.
fn session_paths(data_home: &Path, slug: &str, session_id: &str) -> (PathBuf, PathBuf) {
    let outbox = data_home.join(format!("repos/{slug}/outbox"));
    (
        outbox.join(session_id),
        outbox.join(format!("{session_id}.agent")),
    )
}

/// Create a fresh, empty outbox dir for a session, plus its agent sidecar.
/// The sidecar sits *beside* the mounted dir, out of the agent's reach, so
/// a report can't misstate which agent filed it.
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

/// One report in some session's outbox.
#[derive(Debug)]
pub struct Report {
    pub slug: String,
    pub session: String,
    /// Path relative to the session outbox dir.
    pub rel: PathBuf,
    /// The agent the session ran, from the sidecar. `None` if the sidecar
    /// is missing or unparseable.
    pub agent: Option<Agent>,
    /// Absolute host path of the report file.
    pub file: PathBuf,
}

impl Report {
    /// The address `ramekin outbox` commands take: `<slug>/<session>/<rel>`.
    pub fn entry(&self) -> String {
        format!("{}/{}/{}", self.slug, self.session, self.rel.display())
    }
}

/// All pending reports across every repo and session, sorted by path.
pub fn scan(data_home: &Path) -> Result<Vec<Report>> {
    let mut reports = Vec::new();
    let repos = data_home.join("repos");
    if !repos.is_dir() {
        return Ok(reports);
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
                reports.push(Report {
                    slug: slug.clone(),
                    session: session.clone(),
                    file: session_dir.join(&rel),
                    rel,
                    agent,
                });
            }
        }
    }
    Ok(reports)
}

/// Reports matching an entry: either one file (`<slug>/<session>/<rel>`)
/// or a whole session (`<slug>/<session>`).
pub fn find(data_home: &Path, entry: &str) -> Result<Vec<Report>> {
    let matches: Vec<Report> = scan(data_home)?
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

/// Remove a report file and prune its session outbox if now empty.
pub fn remove(data_home: &Path, report: &Report) -> Result<()> {
    fs_err::remove_file(&report.file).into_diagnostic()?;
    // Prune now-empty parent dirs up to (and including, via finish) the
    // session dir.
    let (session_dir, _) = session_paths(data_home, &report.slug, &report.session);
    let mut dir = report.file.parent().map(Path::to_path_buf);
    while let Some(d) = dir {
        if d == session_dir || fs_err::remove_dir(&d).is_err() {
            break;
        }
        dir = d.parent().map(Path::to_path_buf);
    }
    finish_session(data_home, &report.slug, &report.session)?;
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

    fn write_report(data_home: &Path, slug: &str, session: &str, agent: &str, rel: &str) {
        let (dir, meta) = session_paths(data_home, slug, session);
        let file = dir.join(rel);
        fs_err::create_dir_all(file.parent().unwrap()).unwrap();
        fs_err::write(&file, "reported").unwrap();
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
        fs_err::write(dir.join("report.md"), "new").unwrap();

        let pending = finish_session(data_home.path(), "repo-1", "abc").unwrap();
        assert_eq!(pending, 1);
        assert!(dir.exists());
    }

    #[test]
    fn scan_finds_reports_with_agent() {
        let data_home = tempfile::tempdir().unwrap();
        write_report(data_home.path(), "repo-1", "abc", "pi", "skill-gap.md");

        let reports = scan(data_home.path()).unwrap();
        assert_eq!(reports.len(), 1);
        let r = &reports[0];
        assert_eq!(r.slug, "repo-1");
        assert_eq!(r.session, "abc");
        assert_eq!(r.agent, Some(Agent::Pi));
        assert_eq!(r.entry(), "repo-1/abc/skill-gap.md");
    }

    #[test]
    fn missing_sidecar_means_no_agent() {
        let data_home = tempfile::tempdir().unwrap();
        let (dir, _) = session_paths(data_home.path(), "repo-1", "abc");
        fs_err::create_dir_all(&dir).unwrap();
        fs_err::write(dir.join("report.md"), "x").unwrap();

        let reports = scan(data_home.path()).unwrap();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].agent, None);
    }

    #[test]
    fn find_matches_file_and_session() {
        let data_home = tempfile::tempdir().unwrap();
        write_report(data_home.path(), "repo-1", "abc", "pi", "a.md");
        write_report(data_home.path(), "repo-1", "abc", "pi", "nested/b.md");

        let by_file = find(data_home.path(), "repo-1/abc/a.md").unwrap();
        assert_eq!(by_file.len(), 1);

        let by_session = find(data_home.path(), "repo-1/abc").unwrap();
        assert_eq!(by_session.len(), 2);

        assert!(find(data_home.path(), "repo-1/nope").is_err());
    }

    #[test]
    fn remove_prunes_empty_dirs_and_session() {
        let data_home = tempfile::tempdir().unwrap();
        write_report(data_home.path(), "repo-1", "abc", "pi", "nested/deep/a.md");

        let reports = scan(data_home.path()).unwrap();
        remove(data_home.path(), &reports[0]).unwrap();

        let (dir, meta) = session_paths(data_home.path(), "repo-1", "abc");
        assert!(!dir.exists());
        assert!(!meta.exists());
    }
}
