//! Git CLI wrapper for synced repos.
//!
//! Modeled on `git_backup::run_git_env`: every call shells out to `git -C
//! <dir>`, injects keychain credentials for the remote through
//! `git_credentials::credential_env_for_url`, honours the app proxy setting
//! and redacts URLs in error messages. Nothing here knows about the database.

use anyhow::{Context, Result};
use std::path::Path;
use std::process::{Command, Output};

use super::git_backup::redact_urls_in_text;
use super::git_credentials;

fn git_command() -> Command {
    #[allow(unused_mut)]
    let mut cmd = Command::new("git");
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    cmd
}

/// Remote context for commands that talk to the network.
#[derive(Debug, Clone, Default)]
pub struct Remote<'a> {
    /// Remote URL (without userinfo); selects the keychain credential.
    pub url: Option<&'a str>,
    pub proxy: Option<&'a str>,
}

impl<'a> Remote<'a> {
    pub fn new(url: &'a str, proxy: Option<&'a str>) -> Self {
        Self { url: Some(url), proxy }
    }
}

struct Run {
    stdout: String,
    stderr: String,
}

fn run_output(dir: &Path, args: &[&str], remote: &Remote<'_>) -> Result<Output> {
    let mut cmd = git_command();
    cmd.arg("-C").arg(dir);
    if let Some(proxy) = remote.proxy.filter(|p| !p.is_empty()) {
        cmd.arg("-c").arg(format!("http.proxy={proxy}"));
        cmd.arg("-c").arg(format!("https.proxy={proxy}"));
    }
    cmd.args(args);
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    if let Some(url) = remote.url {
        for (k, v) in git_credentials::credential_env_for_url(url) {
            cmd.env(k, v);
        }
    }
    cmd.output().context("Failed to run git command")
}

fn failure_message(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let filtered = stderr
        .lines()
        .filter(|line| {
            let trimmed = line.trim().trim_start_matches("** ");
            !trimmed.starts_with("WARNING:")
                && !trimmed.starts_with("This session may")
                && !trimmed.starts_with("The server may")
                && !trimmed.starts_with("See https://openssh.com")
        })
        .collect::<Vec<_>>()
        .join("\n");
    let msg = if filtered.trim().is_empty() {
        stderr.trim().to_string()
    } else {
        filtered.trim().to_string()
    };
    if msg.is_empty() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        redact_urls_in_text(stdout.trim())
    } else {
        redact_urls_in_text(&msg)
    }
}

fn run_full(dir: &Path, args: &[&str], remote: &Remote<'_>) -> Result<Run> {
    let output = run_output(dir, args, remote)?;
    if output.status.success() {
        Ok(Run {
            stdout: String::from_utf8_lossy(&output.stdout).trim().to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        })
    } else {
        anyhow::bail!("git command failed: {}", failure_message(&output))
    }
}

fn run(dir: &Path, args: &[&str]) -> Result<String> {
    Ok(run_full(dir, args, &Remote::default())?.stdout)
}

fn run_net(dir: &Path, args: &[&str], remote: &Remote<'_>) -> Result<String> {
    Ok(run_full(dir, args, remote)?.stdout)
}

/// True when the message looks like an authentication / authorization failure.
pub fn is_auth_error(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("authentication failed")
        || lower.contains("could not read username")
        || lower.contains("could not read password")
        || lower.contains("permission denied")
        || lower.contains("terminal prompts disabled")
        || lower.contains("repository not found")
        || lower.contains("http 401")
        || lower.contains("http 403")
        || lower.contains("returned error: 403")
        || lower.contains("returned error: 401")
}

/// Full clone of `url` into `dest` (created by git; parent must exist).
pub fn clone(url: &str, dest: &Path, branch: Option<&str>, proxy: Option<&str>) -> Result<()> {
    let parent = dest.parent().context("clone destination has no parent")?;
    std::fs::create_dir_all(parent)?;
    let dest_str = dest.to_string_lossy().to_string();
    let mut args = vec!["clone", "--quiet"];
    if let Some(b) = branch.filter(|b| !b.is_empty()) {
        args.push("--branch");
        args.push(b);
    }
    args.push("--");
    args.push(url);
    args.push(&dest_str);
    run_net(parent, &args, &Remote::new(url, proxy)).map(|_| ())
}

/// `git fetch --prune origin`.
pub fn fetch(repo: &Path, remote: &Remote<'_>) -> Result<()> {
    run_net(repo, &["fetch", "--quiet", "--prune", "origin"], remote).map(|_| ())
}

pub fn current_branch(repo: &Path) -> Result<String> {
    run(repo, &["symbolic-ref", "--short", "HEAD"])
}

pub fn rev_parse(repo: &Path, rev: &str) -> Result<String> {
    run(repo, &["rev-parse", "--verify", "--quiet", rev])
}

pub fn head_revision(repo: &Path) -> Result<String> {
    rev_parse(repo, "HEAD")
}

/// True when the working tree has no tracked changes and no untracked files.
pub fn is_clean(repo: &Path) -> Result<bool> {
    Ok(status_porcelain(repo)?.is_empty())
}

pub fn status_porcelain(repo: &Path) -> Result<Vec<String>> {
    let out = run(repo, &["status", "--porcelain", "--untracked-files=all"])?;
    Ok(out.lines().map(|l| l.to_string()).collect())
}

/// Bring the checked-out branch to `origin/<branch>`: fast-forward when
/// possible, hard reset when history was rewritten upstream. Returns `true`
/// when a reset was needed. The caller guarantees a clean working tree.
pub fn ff_or_reset(repo: &Path, branch: &str) -> Result<bool> {
    let upstream = format!("origin/{branch}");
    if run(repo, &["merge", "--ff-only", "--quiet", &upstream]).is_ok() {
        return Ok(false);
    }
    run(repo, &["reset", "--hard", "--quiet", &upstream])?;
    Ok(true)
}

/// Discard every local edit, tracked and untracked.
pub fn discard_changes(repo: &Path) -> Result<()> {
    run(repo, &["reset", "--hard", "--quiet", "HEAD"])?;
    run(repo, &["clean", "-fd", "--quiet"])?;
    Ok(())
}

/// Switch the clone to `branch` tracking `origin/<branch>` (after a fetch).
pub fn checkout_tracking(repo: &Path, branch: &str) -> Result<()> {
    let upstream = format!("origin/{branch}");
    run(repo, &["checkout", "--quiet", "-B", branch, &upstream]).map(|_| ())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameStatus {
    /// `A`, `M`, `D`, `R`, `C`, `T`.
    pub status: char,
    /// Path (the old path for renames/copies).
    pub path: String,
    /// New path for renames/copies.
    pub new_path: Option<String>,
}

/// `git diff --name-status -M -z from to`, parsed.
pub fn diff_name_status(repo: &Path, from: &str, to: &str) -> Result<Vec<NameStatus>> {
    let out = run_output(
        repo,
        &["diff", "--name-status", "-M", "-z", "--no-color", from, to, "--"],
        &Remote::default(),
    )?;
    if !out.status.success() {
        anyhow::bail!("git command failed: {}", failure_message(&out));
    }
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let mut parts = text.split('\0').filter(|s| !s.is_empty());
    let mut entries = Vec::new();
    while let Some(status) = parts.next() {
        let code = status.chars().next().unwrap_or('M');
        let path = parts.next().unwrap_or_default().to_string();
        let new_path = if code == 'R' || code == 'C' {
            parts.next().map(|s| s.to_string())
        } else {
            None
        };
        entries.push(NameStatus { status: code, path, new_path });
    }
    Ok(entries)
}

pub fn worktree_add(repo: &Path, path: &Path, branch: &str, start_point: &str) -> Result<()> {
    let p = path.to_string_lossy().to_string();
    run(repo, &["worktree", "add", "--quiet", "--no-track", "-b", branch, &p, start_point]).map(|_| ())
}

pub fn worktree_remove(repo: &Path, path: &Path) -> Result<()> {
    let p = path.to_string_lossy().to_string();
    run(repo, &["worktree", "remove", "--force", &p])?;
    let _ = run(repo, &["worktree", "prune"]);
    Ok(())
}

pub fn worktree_repair(repo: &Path, path: &Path) -> Result<()> {
    let p = path.to_string_lossy().to_string();
    run(repo, &["worktree", "repair", &p]).map(|_| ())
}

pub fn merge_base(repo: &Path, a: &str, b: &str) -> Result<String> {
    run(repo, &["merge-base", a, b])
}

/// Stage everything and commit. Returns `false` when there was nothing to commit.
pub fn commit_all_in(dir: &Path, message: &str) -> Result<bool> {
    run(dir, &["add", "-A"])?;
    if run_output(dir, &["diff", "--cached", "--quiet"], &Remote::default())?
        .status
        .success()
    {
        return Ok(false);
    }
    let has_identity = run(dir, &["config", "user.email"])
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false);
    let mut args: Vec<&str> = Vec::new();
    if !has_identity {
        args.extend(["-c", "user.name=Skills Manager", "-c", "user.email=skills-manager@localhost"]);
    }
    args.extend(["commit", "--quiet", "-m", message]);
    run(dir, &args)?;
    Ok(true)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RebaseOutcome {
    Clean,
    /// The rebase stopped; these files have conflicts. It is left in progress
    /// so the caller can resolve or `rebase_abort`.
    Conflicts(Vec<String>),
}

/// `-c` flags supplying a fallback committer identity when none is configured.
fn identity_args(dir: &Path) -> Vec<&'static str> {
    let has_identity = run(dir, &["config", "user.email"])
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false);
    if has_identity {
        Vec::new()
    } else {
        vec!["-c", "user.name=Skills Manager", "-c", "user.email=skills-manager@localhost"]
    }
}

pub fn rebase(dir: &Path, onto: &str) -> Result<RebaseOutcome> {
    let mut args = identity_args(dir);
    args.extend(["rebase", onto]);
    let out = run_output(dir, &args, &Remote::default())?;
    if out.status.success() {
        return Ok(RebaseOutcome::Clean);
    }
    let conflicted = run(dir, &["diff", "--name-only", "--diff-filter=U"]).unwrap_or_default();
    let files: Vec<String> = conflicted
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.to_string())
        .collect();
    if files.is_empty() {
        anyhow::bail!("git command failed: {}", failure_message(&out));
    }
    Ok(RebaseOutcome::Conflicts(files))
}

pub fn rebase_abort(dir: &Path) -> Result<()> {
    run(dir, &["rebase", "--abort"]).map(|_| ())
}

pub fn rebase_continue(dir: &Path) -> Result<RebaseOutcome> {
    let mut cmd = git_command();
    cmd.arg("-C").arg(dir).args(identity_args(dir)).args(["rebase", "--continue"]).env("GIT_EDITOR", "true");
    let out = cmd.output().context("Failed to run git command")?;
    if out.status.success() {
        return Ok(RebaseOutcome::Clean);
    }
    let conflicted = run(dir, &["diff", "--name-only", "--diff-filter=U"]).unwrap_or_default();
    let files: Vec<String> = conflicted.lines().map(|l| l.to_string()).collect();
    if files.is_empty() {
        anyhow::bail!("git command failed: {}", failure_message(&out));
    }
    Ok(RebaseOutcome::Conflicts(files))
}

/// `git push -u origin <branch>`; returns git's stderr, which hosts use to
/// print a "create a pull request" URL.
pub fn push_branch(dir: &Path, remote: &Remote<'_>, branch: &str, force_with_lease: bool) -> Result<String> {
    push_branch_with_options(dir, remote, branch, force_with_lease, &[])
}

/// Like [`push_branch`] with `-o <option>` push options (GitLab merge request
/// creation). Returns git's stderr.
pub fn push_branch_with_options(
    dir: &Path,
    remote: &Remote<'_>,
    branch: &str,
    force_with_lease: bool,
    push_options: &[String],
) -> Result<String> {
    let mut args: Vec<&str> = vec!["push", "-u"];
    if force_with_lease {
        args.push("--force-with-lease");
    }
    for opt in push_options {
        args.push("-o");
        args.push(opt);
    }
    args.push("origin");
    args.push(branch);
    Ok(redact_urls_in_text(&run_full(dir, &args, remote)?.stderr))
}

/// Whether `path` exists in the tree at `rev`.
pub fn exists_at(repo: &Path, rev: &str, path: &str) -> bool {
    let spec = format!("{rev}:{path}");
    run_output(repo, &["cat-file", "-e", &spec], &Remote::default())
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Contents of `path` at `rev` (`git show rev:path`).
pub fn show_file(repo: &Path, rev: &str, path: &str) -> Result<String> {
    let spec = format!("{rev}:{path}");
    let out = run_output(repo, &["show", &spec], &Remote::default())?;
    if !out.status.success() {
        anyhow::bail!("git command failed: {}", failure_message(&out));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}


// ───────────────────────────── dev mode helpers ─────────────────────────────

/// Check out an existing local `branch` into a new worktree at `path`.
pub fn worktree_add_existing(repo: &Path, path: &Path, branch: &str) -> Result<()> {
    let p = path.to_string_lossy().to_string();
    run(repo, &["worktree", "add", "--quiet", &p, branch]).map(|_| ())
}

pub fn worktree_prune(repo: &Path) {
    let _ = run(repo, &["worktree", "prune"]);
}

pub fn branch_exists(repo: &Path, branch: &str) -> bool {
    rev_parse(repo, &format!("refs/heads/{branch}")).is_ok()
}

pub fn branch_delete(repo: &Path, branch: &str) -> Result<()> {
    run(repo, &["branch", "-D", "--quiet", branch]).map(|_| ())
}

/// `git merge-base --is-ancestor a b`.
pub fn is_ancestor(repo: &Path, a: &str, b: &str) -> bool {
    run_output(repo, &["merge-base", "--is-ancestor", a, b], &Remote::default())
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// `git rev-list --count <args>`.
pub fn rev_count(repo: &Path, args: &[&str]) -> Result<usize> {
    let mut full = vec!["rev-list", "--count"];
    full.extend_from_slice(args);
    let out = run(repo, &full)?;
    out.trim().parse::<usize>().with_context(|| format!("unexpected rev-list output: {out}"))
}

pub fn config_get(repo: &Path, key: &str) -> Option<String> {
    run(repo, &["config", "--get", key]).ok().filter(|v| !v.trim().is_empty())
}

/// Every file differing from `base` in the working tree of `dir`: committed
/// and uncommitted changes plus untracked files (`A`). No rename detection.
pub fn changed_since(dir: &Path, base: &str) -> Result<Vec<NameStatus>> {
    let out = run_output(
        dir,
        &["diff", "--name-status", "--no-renames", "-z", "--no-color", base, "--"],
        &Remote::default(),
    )?;
    if !out.status.success() {
        anyhow::bail!("git command failed: {}", failure_message(&out));
    }
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let mut parts = text.split('\0').filter(|s| !s.is_empty());
    let mut entries: Vec<NameStatus> = Vec::new();
    while let Some(status) = parts.next() {
        let code = status.chars().next().unwrap_or('M');
        let path = parts.next().unwrap_or_default().to_string();
        entries.push(NameStatus { status: code, path, new_path: None });
    }
    let untracked = run_output(
        dir,
        &["ls-files", "--others", "--exclude-standard", "-z"],
        &Remote::default(),
    )?;
    for p in String::from_utf8_lossy(&untracked.stdout).split('\0').filter(|s| !s.is_empty()) {
        if !entries.iter().any(|e| e.path == p) {
            entries.push(NameStatus { status: 'A', path: p.to_string(), new_path: None });
        }
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(entries)
}

/// Raw bytes of `path` at `rev`; `None` when it does not exist there.
pub fn show_bytes(repo: &Path, rev: &str, path: &str) -> Option<Vec<u8>> {
    let spec = format!("{rev}:{path}");
    let out = run_output(repo, &["show", &spec], &Remote::default()).ok()?;
    out.status.success().then_some(out.stdout)
}

/// True when the file is executable (`100755`) at `rev`.
pub fn is_executable_at(repo: &Path, rev: &str, path: &str) -> bool {
    run(repo, &["ls-tree", rev, "--", path])
        .map(|l| l.starts_with("100755"))
        .unwrap_or(false)
}

/// Stage and commit only `pathspec` (everything when `None`). Returns `false`
/// when there was nothing to commit there.
pub fn commit_paths_in(dir: &Path, message: &str, pathspec: Option<&str>) -> Result<bool> {
    let Some(spec) = pathspec.filter(|s| !s.is_empty()) else {
        return commit_all_in(dir, message);
    };
    run(dir, &["add", "-A", "--", spec])?;
    if run_output(dir, &["diff", "--cached", "--quiet", "--", spec], &Remote::default())?
        .status
        .success()
    {
        return Ok(false);
    }
    let has_identity = run(dir, &["config", "user.email"])
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false);
    let mut args: Vec<&str> = Vec::new();
    if !has_identity {
        args.extend(["-c", "user.name=Skills Manager", "-c", "user.email=skills-manager@localhost"]);
    }
    args.extend(["commit", "--quiet", "-m", message, "--", spec]);
    run(dir, &args)?;
    Ok(true)
}

/// True when committed trees `a` and `b` are identical under `pathspec`
/// (the whole tree when `None`).
pub fn tree_equal(repo: &Path, a: &str, b: &str, pathspec: Option<&str>) -> Result<bool> {
    let mut args = vec!["diff", "--quiet", a, b, "--"];
    if let Some(p) = pathspec.filter(|p| !p.is_empty()) {
        args.push(p);
    }
    let out = run_output(repo, &args, &Remote::default())?;
    match out.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => anyhow::bail!("git command failed: {}", failure_message(&out)),
    }
}

/// True when `dir` has uncommitted or untracked changes under `pathspec`.
pub fn dirty_under(dir: &Path, pathspec: Option<&str>) -> Result<bool> {
    let mut args = vec!["status", "--porcelain", "--untracked-files=all", "--"];
    if let Some(p) = pathspec.filter(|p| !p.is_empty()) {
        args.push(p);
    }
    Ok(!run(dir, &args)?.is_empty())
}

pub fn reset_hard_to(dir: &Path, rev: &str) -> Result<()> {
    run(dir, &["reset", "--hard", "--quiet", rev])?;
    run(dir, &["clean", "-fd", "--quiet"])?;
    Ok(())
}

/// Undo the last commit but keep its changes in the working tree.
pub fn undo_last_commit(dir: &Path) -> Result<()> {
    run(dir, &["reset", "--mixed", "--quiet", "HEAD~1"]).map(|_| ())
}

/// Binary-safe patch of everything in `dir` (including untracked files)
/// relative to `base`. Stages everything as a side effect.
pub fn patch_since(dir: &Path, base: &str) -> Result<Vec<u8>> {
    run(dir, &["add", "-A"])?;
    let out = run_output(dir, &["diff", "--cached", "--binary", "--no-color", base, "--"], &Remote::default())?;
    if !out.status.success() {
        anyhow::bail!("git command failed: {}", failure_message(&out));
    }
    Ok(out.stdout)
}

/// `git stash push -u` in `dir`; `false` when there was nothing to stash.
pub fn stash_push(dir: &Path) -> Result<bool> {
    let out = run(
        dir,
        &[
            "-c", "user.name=Skills Manager", "-c", "user.email=skills-manager@localhost",
            "stash", "push", "--include-untracked", "--quiet", "-m", "skills-manager: move to dev",
        ],
    )?;
    Ok(!out.contains("No local changes"))
}

/// `git stash pop` (stashes are shared by all worktrees of a repo).
pub fn stash_pop(dir: &Path) -> Result<()> {
    run(dir, &["stash", "pop", "--quiet"]).map(|_| ())
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use std::path::{Path, PathBuf};

    pub fn git(dir: &Path, args: &[&str]) -> String {
        let out = git_command()
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=T", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A bare remote seeded through a work clone; returns (bare, work).
    pub fn seeded_remote(root: &Path, files: &[(&str, &str)]) -> (PathBuf, PathBuf) {
        let bare = root.join("remote.git");
        let work = root.join("seed");
        std::fs::create_dir_all(&bare).unwrap();
        git(&bare, &["init", "--bare", "--quiet", "--initial-branch=main"]);
        std::fs::create_dir_all(&work).unwrap();
        git(&work, &["init", "--quiet", "--initial-branch=main"]);
        git(&work, &["remote", "add", "origin", &bare.to_string_lossy()]);
        for (path, content) in files {
            let p = work.join(path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, content).unwrap();
        }
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "--quiet", "-m", "seed"]);
        git(&work, &["push", "--quiet", "origin", "main"]);
        (bare, work)
    }

    pub fn push_change(work: &Path, message: &str) {
        git(work, &["add", "-A"]);
        git(work, &["commit", "--quiet", "-m", message]);
        git(work, &["push", "--quiet", "origin", "main"]);
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    const SKILL_A: &str = "---\nname: a\ndescription: A\n---\nbody\n";

    #[test]
    fn clone_fetch_fast_forward_and_diff() {
        let tmp = tempfile::tempdir().unwrap();
        let (bare, work) = seeded_remote(tmp.path(), &[("skills/a/SKILL.md", SKILL_A)]);
        let url = bare.to_string_lossy().to_string();
        let clone_dir = tmp.path().join("clone");
        clone(&url, &clone_dir, None, None).unwrap();
        assert_eq!(current_branch(&clone_dir).unwrap(), "main");
        assert!(is_clean(&clone_dir).unwrap());
        let old = head_revision(&clone_dir).unwrap();

        std::fs::write(work.join("skills/a/SKILL.md"), "---\nname: a\n---\nchanged\n").unwrap();
        push_change(&work, "edit");
        fetch(&clone_dir, &Remote::new(&url, None)).unwrap();
        let new = rev_parse(&clone_dir, "origin/main").unwrap();
        assert_ne!(old, new);
        let diff = diff_name_status(&clone_dir, &old, &new).unwrap();
        assert_eq!(diff, vec![NameStatus { status: 'M', path: "skills/a/SKILL.md".into(), new_path: None }]);
        assert_eq!(show_file(&clone_dir, &new, "skills/a/SKILL.md").unwrap().trim_end(), "---\nname: a\n---\nchanged");

        assert!(!ff_or_reset(&clone_dir, "main").unwrap());
        assert_eq!(head_revision(&clone_dir).unwrap(), new);
        assert_eq!(merge_base(&clone_dir, &old, &new).unwrap(), old);
    }

    #[test]
    fn rename_is_detected_and_dirty_state_is_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let (bare, work) = seeded_remote(tmp.path(), &[("skills/a/SKILL.md", SKILL_A)]);
        let url = bare.to_string_lossy().to_string();
        let clone_dir = tmp.path().join("clone");
        clone(&url, &clone_dir, None, None).unwrap();
        let old = head_revision(&clone_dir).unwrap();

        git(&work, &["mv", "skills/a", "skills/z"]);
        push_change(&work, "rename");
        fetch(&clone_dir, &Remote::new(&url, None)).unwrap();
        let new = rev_parse(&clone_dir, "origin/main").unwrap();
        let diff = diff_name_status(&clone_dir, &old, &new).unwrap();
        assert_eq!(diff[0].status, 'R');
        assert_eq!(diff[0].path, "skills/a/SKILL.md");
        assert_eq!(diff[0].new_path.as_deref(), Some("skills/z/SKILL.md"));

        std::fs::write(clone_dir.join("stray.txt"), "x").unwrap();
        assert!(!is_clean(&clone_dir).unwrap());
        discard_changes(&clone_dir).unwrap();
        assert!(is_clean(&clone_dir).unwrap());
    }

    #[test]
    fn worktree_commit_push_and_rebase_conflicts() {
        let tmp = tempfile::tempdir().unwrap();
        let (bare, work) = seeded_remote(tmp.path(), &[("skills/a/SKILL.md", SKILL_A)]);
        let url = bare.to_string_lossy().to_string();
        let clone_dir = tmp.path().join("clone");
        clone(&url, &clone_dir, None, None).unwrap();

        let dev = tmp.path().join("dev");
        worktree_add(&clone_dir, &dev, "dev-branch", "origin/main").unwrap();
        std::fs::write(dev.join("skills/a/SKILL.md"), "---\nname: a\n---\nmine\n").unwrap();
        assert!(commit_all_in(&dev, "mine").unwrap());
        assert!(!commit_all_in(&dev, "nothing").unwrap());

        std::fs::write(work.join("skills/a/SKILL.md"), "---\nname: a\n---\ntheirs\n").unwrap();
        push_change(&work, "theirs");
        fetch(&clone_dir, &Remote::new(&url, None)).unwrap();
        match rebase(&dev, "origin/main").unwrap() {
            RebaseOutcome::Conflicts(files) => assert_eq!(files, vec!["skills/a/SKILL.md".to_string()]),
            other => panic!("expected conflicts, got {other:?}"),
        }
        rebase_abort(&dev).unwrap();

        push_branch(&dev, &Remote::new(&url, None), "dev-branch", false).unwrap();
        worktree_repair(&clone_dir, &dev).unwrap();
        worktree_remove(&clone_dir, &dev).unwrap();
        assert!(!dev.exists());
    }

    #[test]
    fn auth_errors_are_recognised() {
        assert!(is_auth_error("fatal: Authentication failed for 'https://x'"));
        assert!(!is_auth_error("fatal: unable to access: Could not resolve host"));
    }
}
