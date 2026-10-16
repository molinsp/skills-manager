//! Dev mode for synced repos: edit picked skills in a git worktree (`dev/`),
//! then propose the change upstream as a pull / merge request.
//!
//! The `current` link decides which tree the picked skills read from, so
//! switching dev mode on or off only flips that link and re-hashes the picked
//! skills. Everything here runs under the per-repo [`SyncedRepoLock`].
//!
//! Terminology: the *repo version* is the HEAD of the canonical clone
//! (`repo/`), which `refresh` fast-forwards even while dev mode is on.

use anyhow::{Context, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};

use super::skill_store::{SkillStore, SyncedRepoRecord};
use super::synced_repo_git::{self as git, Remote, RebaseOutcome};
use super::synced_repo_lock::SyncedRepoLock;
use super::synced_repos::{
    clone_dir, dev_dir, find_skill_dirs, now_ms, refresh, rehash_picked, require_repo, scan_available,
    set_current_link,
};
use super::{central_repo, git_credentials, github_api, skill_metadata};
use crate::commands::skills::{classify_diff_bytes, SkillSourceDiffEntryDto};

// ───────────────────────────── result types ─────────────────────────────

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ChangedSkill {
    pub rel_path: String,
    pub name: String,
    pub files_changed: usize,
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DevStatus {
    pub dev_mode: bool,
    pub branch: Option<String>,
    pub base_revision: Option<String>,
    pub changed_skills: Vec<ChangedSkill>,
    pub outside_subfolder_files: Vec<String>,
    /// Commits on the dev branch that the repo version does not have.
    pub ahead_of_repo: usize,
    /// Commits in the repo version that the dev branch does not have.
    pub repo_ahead_by: usize,
    /// Commits that exist nowhere on the remote.
    pub unpushed_commits: usize,
    pub merged: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillDevDiff {
    pub rel_path: String,
    pub name: String,
    pub entries: Vec<SkillSourceDiffEntryDto>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DevDiff {
    pub skills: Vec<SkillDevDiff>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RebaseResult {
    /// `ok` | `conflict`
    pub status: String,
    pub conflicted_files: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PrKind {
    Api,
    PushOption,
    ComparePage,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PrResult {
    pub url: String,
    pub kind: PrKind,
    pub outside_subfolder_files: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct FinishResult {
    /// `finished` | `needs_confirmation`
    pub status: String,
    pub unpushed_commits: usize,
}

/// What `commit_and_push` did.
#[derive(Debug, Clone)]
pub struct PushedBranch {
    pub branch: String,
    pub base_branch: String,
    pub outside_subfolder_files: Vec<String>,
    /// Merge request URL printed by the host (GitLab push option), if any.
    pub mr_link: Option<String>,
}

// ───────────────────────────── helpers ─────────────────────────────

struct Ctx {
    repo: SyncedRepoRecord,
    clone: PathBuf,
    dev: PathBuf,
}

fn ctx(store: &SkillStore, slug: &str) -> Result<Ctx> {
    let repo = require_repo(store, slug)?;
    let clone = clone_dir(slug);
    if !clone.join(".git").exists() {
        anyhow::bail!("The local clone of '{}' is missing", repo.name);
    }
    Ok(Ctx { repo, clone, dev: dev_dir(slug) })
}

impl Ctx {
    fn branch(&self) -> Result<String> {
        match self.repo.resolved_branch.clone().or_else(|| self.repo.branch.clone()) {
            Some(b) => Ok(b),
            None => git::current_branch(&self.clone),
        }
    }

    fn subpath(&self) -> Option<String> {
        self.repo
            .subpath
            .as_deref()
            .map(|s| s.trim_matches('/').to_string())
            .filter(|s| !s.is_empty())
    }

    /// Dev branch name; errors when dev mode was never started.
    fn dev_branch(&self) -> Result<String> {
        if !self.dev.join(".git").exists() {
            anyhow::bail!("Dev mode has not been started for this repo");
        }
        self.repo
            .dev_branch
            .clone()
            .ok_or_else(|| anyhow::anyhow!("Dev mode has not been started for this repo"))
    }

    fn repo_version(&self) -> Result<String> {
        git::head_revision(&self.clone)
    }

    /// Merge base of the repo version and the dev branch.
    fn merge_base(&self, dev_branch: &str) -> Result<String> {
        let head = self.repo_version()?;
        git::merge_base(&self.clone, &head, dev_branch)
    }
}

/// Split a repo-relative path into the part inside the subfolder, if inside.
fn strip_subfolder<'a>(path: &'a str, subpath: Option<&str>) -> Option<&'a str> {
    match subpath {
        None => Some(path),
        Some(sp) => path.strip_prefix(sp).and_then(|r| r.strip_prefix('/')),
    }
}

fn exec_bit(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata().map(|m| m.permissions().mode() & 0o111 != 0).unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        false
    }
}

/// Known skill rel_paths (rows + skills found in the dev tree), longest first.
fn skill_rel_paths(store: &SkillStore, ctx: &Ctx) -> Result<Vec<(String, String)>> {
    let slug = &ctx.repo.slug;
    let mut out: Vec<(String, String)> = store
        .list_synced_repo_skills(slug)?
        .into_iter()
        .map(|r| (r.rel_path, r.name))
        .collect();
    let mut root = ctx.dev.clone();
    if let Some(sp) = ctx.subpath() {
        root = root.join(sp);
    }
    for rel in find_skill_dirs(&root) {
        if !out.iter().any(|(r, _)| *r == rel) {
            let name = skill_metadata::infer_skill_name(&root.join(&rel));
            out.push((rel, name));
        }
    }
    out.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then(a.0.cmp(&b.0)));
    Ok(out)
}

fn skill_for<'a>(skills: &'a [(String, String)], inner: &str) -> Option<&'a (String, String)> {
    skills.iter().find(|(rel, _)| inner.strip_prefix(rel.as_str()).is_some_and(|r| r.starts_with('/')))
}

// ───────────────────────────── worktree lifecycle ─────────────────────────────

fn unix_secs() -> i64 {
    now_ms() / 1000
}

/// Make sure `dev/` is a worktree with a dev branch recorded on `repo`.
/// `start` overrides the start point (default `origin/<branch>`).
fn ensure_worktree(c: &mut Ctx, start: Option<&str>) -> Result<()> {
    if c.dev.join(".git").exists() {
        let _ = git::worktree_repair(&c.clone, &c.dev);
        if c.repo.dev_branch.is_none() {
            c.repo.dev_branch = git::current_branch(&c.dev).ok();
        }
        if c.repo.dev_base_revision.is_none() {
            if let Some(b) = c.repo.dev_branch.clone() {
                c.repo.dev_base_revision = c.merge_base(&b).ok();
            }
        }
        return Ok(());
    }

    let branch = c.branch()?;
    git::worktree_prune(&c.clone);
    let start_rev = match start {
        Some(s) => git::rev_parse(&c.clone, s)?,
        None => git::rev_parse(&c.clone, &format!("origin/{branch}"))
            .or_else(|_| git::head_revision(&c.clone))?,
    };

    match c.repo.dev_branch.clone().filter(|b| git::branch_exists(&c.clone, b)) {
        Some(existing) => git::worktree_add_existing(&c.clone, &c.dev, &existing)?,
        None => {
            let mut name = format!("skills-manager/dev-{}", unix_secs());
            let mut n = 2;
            while git::branch_exists(&c.clone, &name) {
                name = format!("skills-manager/dev-{}-{n}", unix_secs());
                n += 1;
            }
            git::worktree_add(&c.clone, &c.dev, &name, &start_rev)?;
            c.repo.dev_branch = Some(name);
            c.repo.dev_base_revision = Some(start_rev);
        }
    }
    Ok(())
}

/// After `current` moved: rescan skills and re-hash/redeploy picked ones.
fn after_switch(store: &SkillStore, c: &Ctx) -> Result<()> {
    scan_available(store, &c.repo.slug)?;
    let rev = git::head_revision(&c.clone).unwrap_or_default();
    rehash_picked(store, &c.repo.slug, &rev)?;
    Ok(())
}

/// Turn dev mode on or off. On: creates the worktree on first use and points
/// `current` at it. Off: points `current` back at `repo/`; edits stay in `dev/`.
pub fn set_dev_mode(store: &SkillStore, slug: &str, enabled: bool) -> Result<SyncedRepoRecord> {
    let _lock = SyncedRepoLock::acquire_foreground(slug, "dev-mode")?;
    let mut c = ctx(store, slug)?;
    if enabled == c.repo.dev_mode {
        return Ok(c.repo);
    }
    if enabled {
        if c.repo.state == "canonical_dirty" {
            anyhow::bail!("The local clone has edits; move them to dev mode or discard them first");
        }
        ensure_worktree(&mut c, None)?;
        set_current_link(slug, "dev")?;
        c.repo.dev_mode = true;
    } else {
        set_current_link(slug, "repo")?;
        c.repo.dev_mode = false;
    }
    store.update_synced_repo(&c.repo)?;
    after_switch(store, &c)?;
    require_repo(store, slug)
}

/// `canonical_dirty` resolution "dev": carry the stray edits of `repo/` into
/// the dev worktree (created from the clone's current commit so they apply
/// cleanly) and turn dev mode on.
pub fn move_edits_to_dev(store: &SkillStore, slug: &str) -> Result<()> {
    let _lock = SyncedRepoLock::acquire_foreground(slug, "move-to-dev")?;
    let mut c = ctx(store, slug)?;
    if !git::is_clean(&c.clone)? {
        let head = git::head_revision(&c.clone)?;
        if !git::stash_push(&c.clone)? {
            anyhow::bail!("Could not set the edits aside");
        }
        if let Err(e) = ensure_worktree(&mut c, Some(&head)) {
            let _ = git::stash_pop(&c.clone);
            return Err(e);
        }
        git::stash_pop(&c.dev).context(
            "Could not apply the edits to the dev folder; they are kept in the repo's git stash",
        )?;
    } else {
        ensure_worktree(&mut c, None)?;
    }
    set_current_link(slug, "dev")?;
    c.repo.dev_mode = true;
    if c.repo.state == "canonical_dirty" {
        c.repo.state = "ok".to_string();
    }
    store.update_synced_repo(&c.repo)?;
    after_switch(store, &c)?;
    Ok(())
}

// ───────────────────────────── status & diff ─────────────────────────────

/// Commits never pushed: 0 once the branch was pushed and is level with (or
/// gone from) the remote.
fn count_unpushed(c: &Ctx, dev_branch: &str) -> Result<usize> {
    let remote_ref = format!("origin/{dev_branch}");
    if git::rev_parse(&c.clone, &remote_ref).is_ok() {
        return git::rev_count(&c.clone, &[&format!("{remote_ref}..{dev_branch}")]);
    }
    let pushed_before = git::config_get(&c.clone, &format!("branch.{dev_branch}.merge")).as_deref()
        == Some(format!("refs/heads/{dev_branch}").as_str());
    if pushed_before {
        return Ok(0);
    }
    git::rev_count(&c.clone, &[dev_branch, "--not", "--remotes=origin"])
}

/// Whether the dev branch's change landed upstream: it changed the subfolder,
/// nothing there is uncommitted, and the branch is an ancestor of
/// `origin/<branch>` (merge / fast-forward) or the subfolder matches it
/// exactly (squash / rebase merge).
fn is_merged(c: &Ctx, dev_branch: &str) -> Result<bool> {
    let upstream = format!("origin/{}", c.branch()?);
    if git::rev_parse(&c.clone, &upstream).is_err() {
        return Ok(false);
    }
    let sub = c.subpath();
    let base = match c.repo.dev_base_revision.clone() {
        Some(b) => b,
        None => c.merge_base(dev_branch)?,
    };
    if git::tree_equal(&c.clone, &base, dev_branch, sub.as_deref())? {
        return Ok(false); // nothing to have merged
    }
    if git::dirty_under(&c.dev, sub.as_deref())? {
        return Ok(false);
    }
    Ok(git::is_ancestor(&c.clone, dev_branch, &upstream)
        || git::tree_equal(&c.clone, &upstream, dev_branch, sub.as_deref())?)
}

fn status_inner(store: &SkillStore, c: &Ctx) -> Result<DevStatus> {
    let mut st = DevStatus { dev_mode: c.repo.dev_mode, ..Default::default() };
    let Ok(dev_branch) = c.dev_branch() else {
        return Ok(st);
    };
    st.branch = Some(dev_branch.clone());
    st.base_revision = c.repo.dev_base_revision.clone();

    let head = c.repo_version()?;
    let base = c.merge_base(&dev_branch)?;
    let sub = c.subpath();
    let skills = skill_rel_paths(store, c)?;

    let mut counts: Vec<ChangedSkill> = Vec::new();
    for change in git::changed_since(&c.dev, &base)? {
        match strip_subfolder(&change.path, sub.as_deref()) {
            None => st.outside_subfolder_files.push(change.path),
            Some(inner) => {
                if let Some((rel, name)) = skill_for(&skills, inner) {
                    match counts.iter_mut().find(|s| s.rel_path == *rel) {
                        Some(s) => s.files_changed += 1,
                        None => counts.push(ChangedSkill {
                            rel_path: rel.clone(),
                            name: name.clone(),
                            files_changed: 1,
                        }),
                    }
                }
            }
        }
    }
    counts.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    st.changed_skills = counts;
    st.ahead_of_repo = git::rev_count(&c.clone, &[&format!("{head}..{dev_branch}")])?;
    st.repo_ahead_by = git::rev_count(&c.clone, &[&format!("{dev_branch}..{head}")])?;
    st.unpushed_commits = count_unpushed(c, &dev_branch)?;
    st.merged = is_merged(c, &dev_branch)?;
    Ok(st)
}

pub fn status(store: &SkillStore, slug: &str) -> Result<DevStatus> {
    let c = ctx(store, slug)?;
    status_inner(store, &c)
}

/// Per-skill diff of the dev branch (committed and uncommitted) against the
/// merge base with the repo version.
pub fn diff(store: &SkillStore, slug: &str) -> Result<DevDiff> {
    let c = ctx(store, slug)?;
    let dev_branch = c.dev_branch()?;
    let base = c.merge_base(&dev_branch)?;
    let sub = c.subpath();
    let skills = skill_rel_paths(store, &c)?;

    let mut grouped: Vec<SkillDevDiff> = Vec::new();
    for change in git::changed_since(&c.dev, &base)? {
        let Some(inner) = strip_subfolder(&change.path, sub.as_deref()) else { continue };
        let Some((rel, name)) = skill_for(&skills, inner) else { continue };
        let file_rel = inner[rel.len() + 1..].to_string();

        let original = git::show_bytes(&c.clone, &base, &change.path);
        let updated_path = c.dev.join(&change.path);
        let updated = if updated_path.is_file() { std::fs::read(&updated_path).ok() } else { None };
        let exec_before = original.is_some() && git::is_executable_at(&c.clone, &base, &change.path);
        let exec_after = updated.is_some() && exec_bit(&updated_path);

        let entry = match (original, updated) {
            (None, None) => continue,
            (None, Some(u)) => {
                let (kind, text) = classify_diff_bytes(Some(u));
                SkillSourceDiffEntryDto {
                    relative_path: file_rel,
                    status: "added".into(),
                    content_kind: kind.into(),
                    original_text: None,
                    updated_text: text,
                    executable_before: false,
                    executable_after: exec_after,
                }
            }
            (Some(o), None) => {
                let (kind, text) = classify_diff_bytes(Some(o));
                SkillSourceDiffEntryDto {
                    relative_path: file_rel,
                    status: "removed".into(),
                    content_kind: kind.into(),
                    original_text: text,
                    updated_text: None,
                    executable_before: exec_before,
                    executable_after: false,
                }
            }
            (Some(o), Some(u)) => {
                if o == u {
                    if exec_before == exec_after {
                        continue;
                    }
                    SkillSourceDiffEntryDto {
                        relative_path: file_rel,
                        status: "modified".into(),
                        content_kind: "permission_only".into(),
                        original_text: None,
                        updated_text: None,
                        executable_before: exec_before,
                        executable_after: exec_after,
                    }
                } else {
                    let (ok, ot) = classify_diff_bytes(Some(o));
                    let (uk, ut) = classify_diff_bytes(Some(u));
                    let (kind, original_text, updated_text) = if ok == "text" && uk == "text" {
                        ("text", ot, ut)
                    } else if ok == "too_large" || uk == "too_large" {
                        ("too_large", None, None)
                    } else {
                        ("binary", None, None)
                    };
                    SkillSourceDiffEntryDto {
                        relative_path: file_rel,
                        status: "modified".into(),
                        content_kind: kind.into(),
                        original_text,
                        updated_text,
                        executable_before: exec_before,
                        executable_after: exec_after,
                    }
                }
            }
        };
        match grouped.iter_mut().find(|g| g.rel_path == *rel) {
            Some(g) => g.entries.push(entry),
            None => grouped.push(SkillDevDiff { rel_path: rel.clone(), name: name.clone(), entries: vec![entry] }),
        }
    }
    grouped.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    for g in &mut grouped {
        g.entries.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    }
    Ok(DevDiff { skills: grouped })
}

// ───────────────────────────── rebase & discard ─────────────────────────────

/// Commit work in progress and rebase the dev branch onto the repo version.
/// On conflict the rebase is aborted (the WIP commit is undone again) and the
/// conflicting files are returned.
pub fn rebase(store: &SkillStore, slug: &str, proxy: Option<&str>) -> Result<RebaseResult> {
    // Bring the repo version up to date first (takes the lock itself).
    if let Err(e) = refresh(store, slug, proxy, false, None) {
        log::warn!("synced repo {slug}: refresh before rebase failed: {e:#}");
    }
    let _lock = SyncedRepoLock::acquire_foreground(slug, "dev-rebase")?;
    let mut c = ctx(store, slug)?;
    let dev_branch = c.dev_branch()?;
    let head = c.repo_version()?;
    let ok = || RebaseResult { status: "ok".into(), conflicted_files: Vec::new() };

    if git::rev_count(&c.clone, &[&format!("{dev_branch}..{head}")])? == 0 {
        return Ok(ok());
    }
    let committed = git::commit_all_in(&c.dev, "WIP: skills-manager")?;
    let outcome = match git::rebase(&c.dev, &head) {
        Ok(o) => o,
        Err(e) => {
            let _ = git::rebase_abort(&c.dev);
            if committed {
                let _ = git::undo_last_commit(&c.dev);
            }
            return Err(e);
        }
    };
    match outcome {
        RebaseOutcome::Clean => {
            c.repo.dev_base_revision = Some(head);
            store.update_synced_repo(&c.repo)?;
            if c.repo.dev_mode {
                after_switch(store, &c)?;
            }
            Ok(ok())
        }
        RebaseOutcome::Conflicts(files) => {
            git::rebase_abort(&c.dev)?;
            if committed {
                git::undo_last_commit(&c.dev)?;
            }
            Ok(RebaseResult { status: "conflict".into(), conflicted_files: files })
        }
    }
}

fn sanitize_for_filename(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' }).collect()
}

/// Reset the dev branch to the repo version after saving everything that
/// differs as a `.patch` under `synced_repos/<slug>/patches/`. Returns the
/// patch path (`None` when there was nothing to save).
pub fn discard(store: &SkillStore, slug: &str) -> Result<Option<PathBuf>> {
    let _lock = SyncedRepoLock::acquire_foreground(slug, "dev-discard")?;
    let mut c = ctx(store, slug)?;
    let dev_branch = c.dev_branch()?;
    let head = c.repo_version()?;
    let base = c.merge_base(&dev_branch)?;

    let patch = git::patch_since(&c.dev, &base)?;
    let mut saved = None;
    if !patch.is_empty() {
        let dir = central_repo::synced_repo_dir(slug).join("patches");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{}-{}.patch", sanitize_for_filename(&dev_branch), now_ms()));
        std::fs::write(&path, patch)?;
        saved = Some(path);
    }
    git::reset_hard_to(&c.dev, &head)?;
    c.repo.dev_base_revision = Some(head);
    c.repo.pr_url = None;
    store.update_synced_repo(&c.repo)?;
    if c.repo.dev_mode {
        after_switch(store, &c)?;
    }
    Ok(saved)
}

// ───────────────────────────── pull / merge requests ─────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebRemote {
    /// `https://host[:port]`, no trailing slash.
    pub web_base: String,
    pub host: String,
    /// `owner/repo` (or `group/sub/repo`) without `.git`.
    pub path: String,
}

/// Turn a clone URL (https, ssh://, scp-like `git@host:path`, git://) into
/// its web location. `None` for local paths and anything unparsable.
pub fn parse_remote_url(url: &str) -> Option<WebRemote> {
    let url = url.trim();
    let (scheme, authority, path) = if let Some((scheme, rest)) = url.split_once("://") {
        let (authority, path) = rest.split_once('/')?;
        (scheme.to_ascii_lowercase(), authority.to_string(), path.to_string())
    } else if let Some((user_host, path)) = url.split_once(':') {
        // scp-like: [user@]host:path
        if user_host.contains('/') || path.starts_with("//") || user_host.len() == 1 {
            return None;
        }
        ("ssh".to_string(), user_host.to_string(), path.to_string())
    } else {
        return None;
    };
    let host_port = authority.rsplit('@').next()?;
    let (host, port) = match host_port.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() => (h, Some(p)),
        _ => (host_port, None),
    };
    if host.is_empty() {
        return None;
    }
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path).trim_matches('/');
    if path.is_empty() {
        return None;
    }
    // ssh/git ports are not web ports; http(s) ports are.
    let web_base = match (scheme.as_str(), port) {
        ("http", Some(p)) => format!("http://{host}:{p}"),
        ("https", Some(p)) => format!("https://{host}:{p}"),
        ("http", None) => format!("http://{host}"),
        _ => format!("https://{host}"),
    };
    Some(WebRemote { web_base, host: host.to_ascii_lowercase(), path: path.to_string() })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    GitHub,
    GitLab,
    Bitbucket,
    Unknown,
}

pub fn detect_provider(host: &str) -> Provider {
    let h = host.to_ascii_lowercase();
    if h.contains("github") {
        Provider::GitHub
    } else if h.contains("gitlab") {
        Provider::GitLab
    } else if h == "bitbucket.org" || h == "www.bitbucket.org" {
        Provider::Bitbucket
    } else {
        Provider::Unknown
    }
}

fn enc(s: &str) -> String {
    urlencoding::encode(s).into_owned()
}

fn enc_branch(s: &str) -> String {
    enc(s).replace("%2F", "/")
}

fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// Page where the user can open the PR / MR by hand, prefilled where the host
/// allows it. Pure.
pub fn compare_page_url(remote_url: &str, base: &str, head: &str, title: &str, description: &str) -> Option<String> {
    let r = parse_remote_url(remote_url)?;
    let description = truncate_chars(description, 1500);
    Some(match detect_provider(&r.host) {
        Provider::GitHub => format!(
            "{}/{}/compare/{}...{}?expand=1&title={}&body={}",
            r.web_base,
            r.path,
            enc_branch(base),
            enc_branch(head),
            enc(title),
            enc(&description)
        ),
        Provider::GitLab => format!(
            "{}/{}/-/merge_requests/new?merge_request%5Bsource_branch%5D={}&merge_request%5Btarget_branch%5D={}&merge_request%5Btitle%5D={}&merge_request%5Bdescription%5D={}",
            r.web_base,
            r.path,
            enc(head),
            enc(base),
            enc(title),
            enc(&description)
        ),
        Provider::Bitbucket => format!(
            "{}/{}/pull-requests/new?source={}&dest={}",
            r.web_base,
            r.path,
            enc(head),
            enc(base)
        ),
        Provider::Unknown => format!("{}/{}", r.web_base, r.path),
    })
}

/// `-o` push options asking GitLab to create the merge request. Newlines are
/// not allowed in push options, so they are flattened.
pub fn gitlab_push_options(base: &str, title: &str, description: &str) -> Vec<String> {
    let flat = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut opts = vec![
        "merge_request.create".to_string(),
        format!("merge_request.target={base}"),
        format!("merge_request.title={}", flat(title)),
    ];
    let d = flat(&truncate_chars(description, 1500));
    if !d.is_empty() {
        opts.push(format!("merge_request.description={d}"));
    }
    opts
}

/// Find the merge request link in `git push` output (`remote:` lines).
pub fn parse_mr_link(output: &str) -> Option<String> {
    output.split_whitespace().find_map(|tok| {
        let tok = tok.trim_end_matches(['.', ',', ')', ';']);
        let is_link = (tok.starts_with("https://") || tok.starts_with("http://"))
            && tok.contains("/merge_requests/");
        let is_new_page = tok.contains("/merge_requests/new");
        (is_link && !is_new_page).then(|| tok.to_string())
    })
}

fn github_token() -> Option<String> {
    let cred = git_credentials::load_credential("github.com").ok().flatten()?;
    let token = if cred.password.is_empty() { cred.username } else { cred.password };
    (!token.trim().is_empty()).then_some(token)
}

fn commit_and_push_locked(
    c: &Ctx,
    title: &str,
    description: &str,
    proxy: Option<&str>,
) -> Result<PushedBranch> {
    let dev_branch = c.dev_branch()?;
    let base_branch = c.branch()?;
    let sub = c.subpath();
    let message = if description.trim().is_empty() {
        title.trim().to_string()
    } else {
        format!("{}\n\n{}", title.trim(), description.trim())
    };
    if message.is_empty() {
        anyhow::bail!("A title is required");
    }

    let committed = git::commit_paths_in(&c.dev, &message, sub.as_deref())?;
    let head = c.repo_version()?;
    if !committed && git::rev_count(&c.clone, &[&format!("{head}..{dev_branch}")])? == 0 {
        anyhow::bail!("There are no changes to propose");
    }

    let base = c.merge_base(&dev_branch)?;
    let outside: Vec<String> = git::changed_since(&c.dev, &base)?
        .into_iter()
        .filter(|ch| strip_subfolder(&ch.path, sub.as_deref()).is_none())
        .map(|ch| ch.path)
        .collect();

    let remote = Remote::new(&c.repo.url, proxy);
    let remote_ref = format!("origin/{dev_branch}");
    let lease = git::rev_parse(&c.clone, &remote_ref).is_ok() && !git::is_ancestor(&c.clone, &remote_ref, &dev_branch);

    let provider = parse_remote_url(&c.repo.url).map(|r| detect_provider(&r.host));
    let mut mr_link = None;
    if provider == Some(Provider::GitLab) {
        let opts = gitlab_push_options(&base_branch, title, description);
        match git::push_branch_with_options(&c.dev, &remote, &dev_branch, lease, &opts) {
            Ok(out) => mr_link = parse_mr_link(&out),
            Err(e) if format!("{e:#}").to_ascii_lowercase().contains("push option") => {
                git::push_branch(&c.dev, &remote, &dev_branch, lease)?;
            }
            Err(e) => return Err(e),
        }
    } else {
        git::push_branch(&c.dev, &remote, &dev_branch, lease)?;
    }
    Ok(PushedBranch { branch: dev_branch, base_branch, outside_subfolder_files: outside, mr_link })
}

/// Commit the subfolder's changes and push the dev branch (no link step).
pub fn commit_and_push(
    store: &SkillStore,
    slug: &str,
    title: &str,
    description: &str,
    proxy: Option<&str>,
) -> Result<PushedBranch> {
    let _lock = SyncedRepoLock::acquire_foreground(slug, "dev-push")?;
    let c = ctx(store, slug)?;
    commit_and_push_locked(&c, title, description, proxy)
}

/// Commit, push and produce the link to open the PR / MR: GitHub API when a
/// token is stored, else the GitLab push-option link, else a compare page.
pub fn open_pr(
    store: &SkillStore,
    slug: &str,
    title: &str,
    description: &str,
    proxy: Option<&str>,
) -> Result<PrResult> {
    let _lock = SyncedRepoLock::acquire_foreground(slug, "dev-open-pr")?;
    let mut c = ctx(store, slug)?;
    let pushed = commit_and_push_locked(&c, title, description, proxy)?;
    let remote = parse_remote_url(&c.repo.url);

    let mut result: Option<(String, PrKind)> = None;
    if let Some(r) = remote.as_ref().filter(|r| r.host == "github.com") {
        if let (Some(token), Some((owner, name))) = (github_token(), r.path.split_once('/')) {
            match github_api::create_pull_request(
                &token,
                owner,
                name,
                &pushed.branch,
                &pushed.base_branch,
                title.trim(),
                description,
                proxy,
            ) {
                Ok(pr) => result = Some((pr.url, PrKind::Api)),
                Err(e) => log::warn!("synced repo {slug}: GitHub API PR failed, using compare page: {e:#}"),
            }
        }
    }
    if result.is_none() {
        if let Some(link) = pushed.mr_link.clone() {
            result = Some((link, PrKind::PushOption));
        }
    }
    if result.is_none() {
        if let Some(url) = compare_page_url(&c.repo.url, &pushed.base_branch, &pushed.branch, title, description) {
            result = Some((url, PrKind::ComparePage));
        }
    }
    let (url, kind) = result.ok_or_else(|| {
        anyhow::anyhow!("The branch '{}' was pushed, but no pull request link could be built for this remote", pushed.branch)
    })?;

    c.repo.pr_url = Some(url.clone());
    store.update_synced_repo(&c.repo)?;
    Ok(PrResult { url, kind, outside_subfolder_files: pushed.outside_subfolder_files })
}

// ───────────────────────────── finish ─────────────────────────────

/// End the dev session once the branch is merged: dev mode off, worktree and
/// branch removed. Commits that never reached the remote need `force`.
pub fn finish(store: &SkillStore, slug: &str, force: bool, proxy: Option<&str>) -> Result<FinishResult> {
    if let Err(e) = refresh(store, slug, proxy, false, None) {
        log::warn!("synced repo {slug}: refresh before finish failed: {e:#}");
    }
    let _lock = SyncedRepoLock::acquire_foreground(slug, "dev-finish")?;
    let mut c = ctx(store, slug)?;
    let dev_branch = c.dev_branch()?;
    let st = status_inner(store, &c)?;
    if !st.merged && !force {
        anyhow::bail!("The branch has not been merged yet");
    }
    if st.unpushed_commits > 0 && !force {
        return Ok(FinishResult { status: "needs_confirmation".into(), unpushed_commits: st.unpushed_commits });
    }

    set_current_link(slug, "repo")?;
    if git::worktree_remove(&c.clone, &c.dev).is_err() {
        let _ = std::fs::remove_dir_all(&c.dev);
        git::worktree_prune(&c.clone);
    }
    if let Err(e) = git::branch_delete(&c.clone, &dev_branch) {
        log::warn!("synced repo {slug}: could not delete branch {dev_branch}: {e:#}");
    }
    c.repo.dev_mode = false;
    c.repo.dev_branch = None;
    c.repo.dev_base_revision = None;
    c.repo.pr_url = None;
    store.update_synced_repo(&c.repo)?;
    after_switch(store, &c)?;
    Ok(FinishResult { status: "finished".into(), unpushed_commits: 0 })
}

/// The dev folder, for "open in file manager".
pub fn dev_folder(store: &SkillStore, slug: &str) -> Result<PathBuf> {
    require_repo(store, slug)?;
    let dev = dev_dir(slug);
    if !dev.is_dir() {
        anyhow::bail!("There is no dev folder yet; turn dev mode on first");
    }
    Ok(dev)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_urls_are_parsed() {
        let w = parse_remote_url("https://github.com/acme/skills.git").unwrap();
        assert_eq!((w.web_base.as_str(), w.host.as_str(), w.path.as_str()), ("https://github.com", "github.com", "acme/skills"));
        let w = parse_remote_url("git@github.com:acme/skills.git").unwrap();
        assert_eq!((w.web_base.as_str(), w.path.as_str()), ("https://github.com", "acme/skills"));
        let w = parse_remote_url("ssh://git@gitlab.example.com:2222/grp/sub/repo.git").unwrap();
        assert_eq!((w.web_base.as_str(), w.path.as_str()), ("https://gitlab.example.com", "grp/sub/repo"));
        let w = parse_remote_url("https://user:tok@git.corp.io:8443/a/b/").unwrap();
        assert_eq!((w.web_base.as_str(), w.host.as_str(), w.path.as_str()), ("https://git.corp.io:8443", "git.corp.io", "a/b"));
        assert!(parse_remote_url("/tmp/remote.git").is_none());
        assert!(parse_remote_url("C:\\repos\\x").is_none());
    }

    #[test]
    fn compare_urls_per_provider() {
        let gh = compare_page_url("git@github.com:acme/skills.git", "main", "skills-manager/dev-1", "Fix a & b", "Body\nline").unwrap();
        assert_eq!(
            gh,
            "https://github.com/acme/skills/compare/main...skills-manager/dev-1?expand=1&title=Fix%20a%20%26%20b&body=Body%0Aline"
        );
        let gl = compare_page_url("https://gitlab.com/grp/repo.git", "main", "feat/x", "T", "").unwrap();
        assert_eq!(
            gl,
            "https://gitlab.com/grp/repo/-/merge_requests/new?merge_request%5Bsource_branch%5D=feat%2Fx&merge_request%5Btarget_branch%5D=main&merge_request%5Btitle%5D=T&merge_request%5Bdescription%5D="
        );
        let sh = compare_page_url("git@gitlab.mycorp.com:team/repo.git", "dev", "b", "T", "D").unwrap();
        assert!(sh.starts_with("https://gitlab.mycorp.com/team/repo/-/merge_requests/new?"));
        let bb = compare_page_url("git@bitbucket.org:ws/repo.git", "main", "feature/x", "T", "D").unwrap();
        assert_eq!(bb, "https://bitbucket.org/ws/repo/pull-requests/new?source=feature%2Fx&dest=main");
        let other = compare_page_url("https://git.example.org/a/b.git", "main", "x", "T", "D").unwrap();
        assert_eq!(other, "https://git.example.org/a/b");
        assert!(compare_page_url("/tmp/local.git", "main", "x", "T", "D").is_none());
    }

    #[test]
    fn gitlab_push_options_and_link_parsing() {
        let opts = gitlab_push_options("main", "Title", "line1\nline2");
        assert_eq!(
            opts,
            vec![
                "merge_request.create",
                "merge_request.target=main",
                "merge_request.title=Title",
                "merge_request.description=line1 line2"
            ]
        );
        assert_eq!(gitlab_push_options("main", "T", " ").len(), 3);

        let out = "remote: remote: View merge request for feat: remote: https://gitlab.com/grp/repo/-/merge_requests/12 remote: To gitlab.com:grp/repo.git";
        assert_eq!(parse_mr_link(out).as_deref(), Some("https://gitlab.com/grp/repo/-/merge_requests/12"));
        let only_new = "remote: To create a merge request for feat, visit: remote: https://gitlab.com/grp/repo/-/merge_requests/new?merge_request%5Bsource_branch%5D=feat";
        assert!(parse_mr_link(only_new).is_none());
        assert!(parse_mr_link("Everything up-to-date").is_none());
    }

    #[test]
    fn providers_are_detected() {
        assert_eq!(detect_provider("github.com"), Provider::GitHub);
        assert_eq!(detect_provider("github.enterprise.corp"), Provider::GitHub);
        assert_eq!(detect_provider("gitlab.example.com"), Provider::GitLab);
        assert_eq!(detect_provider("bitbucket.org"), Provider::Bitbucket);
        assert_eq!(detect_provider("git.example.com"), Provider::Unknown);
    }
}
