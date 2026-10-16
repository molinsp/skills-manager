//! Synced repos: shared git repos users subscribe to, cloned outside the
//! Backup repo under `<base>/synced_repos/<slug>/`.
//!
//! Layout per repo: `repo/` (canonical clone), optional `dev/` (git worktree)
//! and `current`, a symlink to `repo` or `dev`. A picked skill's path is
//! `synced_repos/<slug>/current/<rel_path>`.
//!
//! Pull-only: skills are picked from the clone, kept fresh by `refresh`, and
//! never written back. No Tauri dependency; events go through callbacks.

use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use super::skill_store::{SkillRecord, SkillStore, SyncedRepoRecord, SyncedRepoSkillRecord};
use super::synced_repo_git;
use super::synced_repo_lock::SyncedRepoLock;
use super::repo_lock::RepoLock;
use super::{
    central_repo, content_hash, git_credentials, git_fetcher, installer, scenario_service,
    skill_metadata, sync_engine, sync_metadata,
};

/// Unsubscribe a picked synced skill: remove its tool targets, delete the
/// `skills` row and mark its `synced_repo_skills` row `available` again.
///
/// Never touches files inside the clone. A non-synced or unknown id is an
/// error so callers cannot use this to delete library skills.
pub fn unpick(store: &SkillStore, skill_id: &str) -> Result<()> {
    let skill = store
        .get_skill_by_id(skill_id)?
        .ok_or_else(|| anyhow::anyhow!("skill not found: {skill_id}"))?;
    if skill.source_type != "synced_repo" {
        anyhow::bail!("skill {skill_id} is not a synced-repo skill");
    }

    for target in store.get_targets_for_skill(skill_id)? {
        sync_engine::remove_recorded_target_or_warn(Path::new(&target.target_path), &target.mode);
    }

    // Find the discovery row before deleting the skill (the FK would null it).
    let discovered = store.get_synced_repo_skill_by_skill_id(skill_id)?;
    store.delete_skill(skill_id)?;
    if let Some(row) = discovered {
        store.set_synced_repo_skill_state(&row.repo_slug, &row.rel_path, "available", None)?;
    }
    Ok(())
}

/// Point `<repo dir>/current` at `target` (`"repo"` or `"dev"`), replacing an
/// existing link. The link is relative so it survives moving the base dir.
pub fn set_current_link(slug: &str, target: &str) -> Result<()> {
    let dir = central_repo::synced_repo_dir(slug);
    let link = dir.join("current");
    if std::fs::symlink_metadata(&link).is_ok() {
        std::fs::remove_file(&link)?;
    }
    create_dir_symlink(Path::new(target), &link)?;
    Ok(())
}

#[cfg(unix)]
fn create_dir_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn create_dir_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_dir(target, link)
}

/// Repair path-dependent state of every synced repo after the base dir moved:
/// run `git worktree repair` for repos that have a `dev/` worktree and
/// recreate each `current` link. Best effort per repo; returns the first
/// error after trying all repos.
pub fn repair_all(store: &SkillStore) -> Result<()> {
    let mut first_err: Option<anyhow::Error> = None;
    for repo in store.list_synced_repos()? {
        if let Err(err) = repair_one(&repo.slug, repo.dev_mode) {
            log::warn!("synced repo {}: repair failed: {err:#}", repo.slug);
            first_err.get_or_insert(err);
        }
    }
    match first_err {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

fn repair_one(slug: &str, dev_mode: bool) -> Result<()> {
    let dir = central_repo::synced_repo_dir(slug);
    let clone = dir.join("repo");
    let dev = dir.join("dev");
    if !clone.is_dir() {
        // Missing clone is surfaced by the repo state machine, not here.
        return Ok(());
    }

    let has_dev = dev.is_dir();
    if has_dev {
        let out = Command::new("git")
            .arg("-C")
            .arg(&clone)
            .args(["worktree", "repair"])
            .arg(&dev)
            .output()?;
        if !out.status.success() {
            anyhow::bail!(
                "git worktree repair failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
    }

    set_current_link(slug, if dev_mode && has_dev { "dev" } else { "repo" })
}

// ───────────────────────── paths & small helpers ─────────────────────────

/// Callback invoked with the slug after a refresh changed anything. Core stays
/// Tauri-free; the app wires this to the `synced-repos-updated` event.
pub type UpdatedCallback<'a> = Option<&'a dyn Fn(&str)>;

pub(super) fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub fn clone_dir(slug: &str) -> PathBuf {
    central_repo::synced_repo_dir(slug).join("repo")
}

pub fn dev_dir(slug: &str) -> PathBuf {
    central_repo::synced_repo_dir(slug).join("dev")
}

pub fn orphaned_dir(slug: &str) -> PathBuf {
    central_repo::synced_repo_dir(slug).join("orphaned")
}

/// Join a repo-relative subpath and a skill rel_path using `/` (git paths).
pub(super) fn join_git(subpath: Option<&str>, rel: &str) -> String {
    match subpath.filter(|s| !s.is_empty()) {
        Some(sp) => format!("{}/{}", sp.trim_matches('/'), rel),
        None => rel.to_string(),
    }
}

/// Filesystem path of a skill through the `current` link.
pub fn skill_path(repo: &SyncedRepoRecord, rel_path: &str) -> PathBuf {
    let mut p = central_repo::synced_repo_dir(&repo.slug).join("current");
    if let Some(sp) = repo.subpath.as_deref().filter(|s| !s.is_empty()) {
        p = p.join(sp);
    }
    p.join(rel_path)
}

pub(super) fn scan_root(repo: &SyncedRepoRecord) -> PathBuf {
    let mut p = central_repo::synced_repo_dir(&repo.slug).join("current");
    if let Some(sp) = repo.subpath.as_deref().filter(|s| !s.is_empty()) {
        p = p.join(sp);
    }
    p
}

fn normalize_subpath(raw: Option<&str>) -> Result<Option<String>> {
    let Some(raw) = raw else { return Ok(None) };
    let trimmed = raw.trim().trim_matches('/').replace('\\', "/");
    if trimmed.is_empty() || trimmed == "." {
        return Ok(None);
    }
    if Path::new(&trimmed).is_absolute()
        || trimmed.split('/').any(|c| c == ".." || c.is_empty() || c == ".git")
    {
        anyhow::bail!("Invalid subpath: {raw}");
    }
    Ok(Some(trimmed))
}

/// Last path segment of a git URL, without `.git`.
fn repo_name_from_url(url: &str) -> String {
    let trimmed = url.trim().trim_end_matches('/');
    let last = trimmed
        .rsplit(['/', ':', '\\'])
        .next()
        .unwrap_or(trimmed);
    let last = last.strip_suffix(".git").unwrap_or(last);
    if last.is_empty() {
        "repo".to_string()
    } else {
        last.to_string()
    }
}

/// Filesystem-safe lowercase slug: `[a-z0-9_-]`, no leading dot/dash.
pub fn slugify(name: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in name.chars().flat_map(|c| c.to_lowercase()) {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            out.push(ch);
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    let trimmed = out.trim_matches('-');
    let mut slug: String = trimmed.chars().take(48).collect();
    if slug.is_empty() {
        slug = "repo".to_string();
    }
    slug
}

fn choose_slug(store: &SkillStore, base: &str) -> Result<String> {
    let taken: HashSet<String> = store.list_synced_repos()?.into_iter().map(|r| r.slug).collect();
    let mut candidate = base.to_string();
    let mut n = 2;
    while taken.contains(&candidate) || central_repo::synced_repo_dir(&candidate).exists() {
        candidate = format!("{base}-{n}");
        n += 1;
    }
    Ok(candidate)
}

/// Deterministic, UUID-formatted skill id from `url + branch + rel_path`
/// (name-based, version/variant bits set like a v5 UUID; SHA-256 based).
pub fn derive_skill_id(url: &str, branch: &str, rel_path: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(url.as_bytes());
    hasher.update([0]);
    hasher.update(branch.as_bytes());
    hasher.update([0]);
    hasher.update(rel_path.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    uuid::Uuid::from_bytes(bytes).to_string()
}

pub(super) fn require_repo(store: &SkillStore, slug: &str) -> Result<SyncedRepoRecord> {
    store
        .get_synced_repo(slug)?
        .ok_or_else(|| anyhow::anyhow!("synced repo not found: {slug}"))
}

pub(super) fn app_err(e: crate::core::error::AppError) -> anyhow::Error {
    anyhow::anyhow!("{}", e.message)
}

// ───────────────────────────── add / scan ─────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct AddRequest {
    pub url: String,
    pub branch: Option<String>,
    pub subpath: Option<String>,
    pub name: Option<String>,
}

/// Subscribe to a shared repo: validate the URL, move embedded credentials
/// into the keychain, pick a slug, clone and scan for skills. Installs nothing.
pub fn add(store: &SkillStore, req: &AddRequest, proxy: Option<&str>) -> Result<SyncedRepoRecord> {
    git_fetcher::validate_git_url(&req.url)?;
    add_inner(store, req, proxy, false)
}

fn add_inner(
    store: &SkillStore,
    req: &AddRequest,
    proxy: Option<&str>,
    allow_local: bool,
) -> Result<SyncedRepoRecord> {
    let (mut clone_url, parsed_branch, parsed_subpath) = if allow_local {
        (req.url.trim().to_string(), None, None)
    } else {
        let parsed = git_fetcher::parse_git_source(&req.url);
        (parsed.clone_url, parsed.branch, parsed.subpath)
    };

    if let Some((cred, sanitized)) = git_credentials::split_credentials_from_url(&clone_url) {
        if let Some(host) = git_credentials::https_host(&sanitized) {
            git_credentials::store_credential(&host, &cred)?;
        }
        clone_url = sanitized;
    }

    let branch = req
        .branch
        .as_deref()
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .map(str::to_string)
        .or(parsed_branch);
    let subpath = normalize_subpath(req.subpath.as_deref().or(parsed_subpath.as_deref()))?;

    let raw_name = repo_name_from_url(&clone_url);
    let slug = choose_slug(store, &slugify(&raw_name))?;
    let name = req
        .name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .unwrap_or(raw_name);

    let dir = central_repo::synced_repo_dir(&slug);
    let _lock = SyncedRepoLock::acquire_foreground(&slug, "add")?;
    let result = (|| -> Result<SyncedRepoRecord> {
        std::fs::create_dir_all(&dir)?;
        let clone = dir.join("repo");
        synced_repo_git::clone(&clone_url, &clone, branch.as_deref(), proxy)?;
        if let Some(sp) = &subpath {
            if !clone.join(sp).is_dir() {
                anyhow::bail!("Subpath '{sp}' does not exist in the repository");
            }
        }
        std::fs::create_dir_all(dir.join("orphaned"))?;
        set_current_link(&slug, "repo")?;
        let record = SyncedRepoRecord {
            slug: slug.clone(),
            name,
            url: clone_url.clone(),
            branch,
            resolved_branch: synced_repo_git::current_branch(&clone).ok(),
            subpath,
            head_revision: synced_repo_git::head_revision(&clone).ok(),
            last_fetched_at: Some(now_ms()),
            last_error: None,
            auto_update: true,
            state: "ok".to_string(),
            dev_mode: false,
            dev_branch: None,
            dev_base_revision: None,
            pr_url: None,
            sort_order: store.list_synced_repos()?.len() as i32,
            created_at: now_ms(),
        };
        store.insert_synced_repo(&record)?;
        scan_available(store, &slug)?;
        Ok(record)
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&dir);
    }
    result
}

pub(super) fn find_skill_dirs(root: &Path) -> Vec<String> {
    fn walk(dir: &Path, rel: &str, depth: usize, out: &mut Vec<String>) {
        if depth > 8 {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        let mut names: Vec<_> = entries
            .flatten()
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n != ".git" && n != "node_modules" && n != ".hub")
            .collect();
        names.sort();
        for name in names {
            let path = dir.join(&name);
            let child_rel = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
            if skill_metadata::is_valid_skill_dir(&path) {
                out.push(child_rel);
            } else {
                walk(&path, &child_rel, depth + 1, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, "", 0, &mut out);
    out
}

/// Find skill folders in the repo and record them as `available`. Picked and
/// removed-upstream rows keep their state; available rows that disappeared are
/// dropped. Returns the number of skills found.
pub fn scan_available(store: &SkillStore, slug: &str) -> Result<usize> {
    let repo = require_repo(store, slug)?;
    let root = scan_root(&repo);
    let found = find_skill_dirs(&root);
    let now = now_ms();
    let existing: HashMap<String, SyncedRepoSkillRecord> = store
        .list_synced_repo_skills(slug)?
        .into_iter()
        .map(|r| (r.rel_path.clone(), r))
        .collect();

    for rel in &found {
        let dir = root.join(rel);
        let meta = skill_metadata::parse_skill_md(&dir);
        let name = skill_metadata::infer_skill_name(&dir);
        let (state, skill_id, first_seen) = match existing.get(rel) {
            Some(row) if row.state == "picked" => ("picked".to_string(), row.skill_id.clone(), row.first_seen_at),
            Some(row) if row.state == "removed_upstream" => {
                // Reappeared upstream: back to picked when still subscribed.
                if row.skill_id.is_some() {
                    ("picked".to_string(), row.skill_id.clone(), row.first_seen_at)
                } else {
                    ("available".to_string(), None, row.first_seen_at)
                }
            }
            Some(row) => ("available".to_string(), None, row.first_seen_at),
            None => ("available".to_string(), None, now),
        };
        store.upsert_synced_repo_skill(&SyncedRepoSkillRecord {
            repo_slug: slug.to_string(),
            rel_path: rel.clone(),
            skill_id,
            name,
            description: meta.description,
            state,
            first_seen_at: first_seen,
            last_seen_at: now,
        })?;
    }
    let found_set: HashSet<&String> = found.iter().collect();
    for (rel, row) in &existing {
        if !found_set.contains(rel) && row.state == "available" {
            store.delete_synced_repo_skill(slug, rel)?;
        }
    }
    Ok(found.len())
}

// ───────────────────────────── listing & clashes ─────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub struct SkillClash {
    /// `personal` (library skill) or `synced` (another synced skill).
    pub kind: String,
    pub skill_id: String,
    pub skill_name: String,
}

#[derive(Debug, Clone)]
pub struct RepoSkillView {
    pub record: SyncedRepoSkillRecord,
    pub clash: Option<SkillClash>,
}

fn dir_key(central_path: &Path, name: &str) -> String {
    sync_engine::target_dir_name(central_path, name).to_lowercase()
}

fn clash_index(store: &SkillStore) -> Result<HashMap<String, SkillClash>> {
    let mut map = HashMap::new();
    for skill in store.get_all_skills()? {
        let key = dir_key(Path::new(&skill.central_path), &skill.name);
        map.insert(
            key,
            SkillClash {
                kind: if skill.source_type == "synced_repo" { "synced" } else { "personal" }.to_string(),
                skill_id: skill.id,
                skill_name: skill.name,
            },
        );
    }
    Ok(map)
}

fn rel_basename(rel: &str) -> String {
    rel.rsplit('/').next().unwrap_or(rel).to_lowercase()
}

/// Discovered skills of a repo, with the name clash (if any) each unpicked
/// skill would cause when picked.
pub fn list_skills(store: &SkillStore, slug: &str) -> Result<Vec<RepoSkillView>> {
    require_repo(store, slug)?;
    let index = clash_index(store)?;
    Ok(store
        .list_synced_repo_skills(slug)?
        .into_iter()
        .map(|record| {
            let clash = if record.state == "available" {
                index.get(&rel_basename(&record.rel_path)).cloned()
            } else {
                None
            };
            RepoSkillView { record, clash }
        })
        .collect())
}

// ───────────────────────────── pick / unpick ─────────────────────────────

/// Subscribe to the given skills. Clashing personal skills must be named in
/// `replace_skill_ids` (they are deleted first); a clash with another synced
/// skill is always an error (unpick it first). Nothing is changed when any
/// requested skill cannot be picked. Returns the picked skill ids.
pub fn pick(
    store: &SkillStore,
    slug: &str,
    rel_paths: &[String],
    replace_skill_ids: &[String],
) -> Result<Vec<String>> {
    let _lock = SyncedRepoLock::acquire_foreground(slug, "pick")?;
    let repo = require_repo(store, slug)?;
    if repo.state == "missing_clone" {
        anyhow::bail!("The local clone of this repo is missing");
    }

    let index = clash_index(store)?;
    let replace: HashSet<&String> = replace_skill_ids.iter().collect();
    let mut to_delete: Vec<String> = Vec::new();
    let mut seen_names: HashSet<String> = HashSet::new();
    let mut plan = Vec::new();

    for rel in rel_paths {
        let row = store
            .get_synced_repo_skill(slug, rel)?
            .ok_or_else(|| anyhow::anyhow!("Skill not found in repo: {rel}"))?;
        if row.state != "available" {
            anyhow::bail!("Skill '{}' is not available to pick ({})", row.name, row.state);
        }
        let dir = skill_path(&repo, rel);
        if !skill_metadata::is_valid_skill_dir(&dir) {
            anyhow::bail!("'{rel}' is not a skill folder any more");
        }
        let key = rel_basename(rel);
        if !seen_names.insert(key.clone()) {
            anyhow::bail!("Two selected skills share the folder name '{key}'");
        }
        if let Some(clash) = index.get(&key) {
            match clash.kind.as_str() {
                "synced" => anyhow::bail!(
                    "A synced skill named '{}' already exists; unpick it first",
                    clash.skill_name
                ),
                _ if replace.contains(&clash.skill_id) => to_delete.push(clash.skill_id.clone()),
                _ => anyhow::bail!(
                    "Name clash with your skill '{}'; replace it or cancel",
                    clash.skill_name
                ),
            }
        }
        plan.push((row, dir));
    }

    if !to_delete.is_empty() {
        crate::commands::skills::delete_managed_skills_by_ids(store, &to_delete).map_err(app_err)?;
    }

    let branch = repo
        .resolved_branch
        .clone()
        .or_else(|| repo.branch.clone())
        .unwrap_or_default();
    let active = store.get_active_scenario_id().ok().flatten();
    let mut ids = Vec::new();
    for (row, dir) in plan {
        let id = derive_skill_id(&repo.url, &branch, &row.rel_path);
        let hash = content_hash::hash_directory(&dir)?;
        let name = skill_metadata::infer_skill_name(&dir);
        let now = now_ms();
        let record = SkillRecord {
            id: id.clone(),
            name: name.clone(),
            description: row.description.clone(),
            source_type: "synced_repo".to_string(),
            source_ref: Some(repo.url.clone()),
            source_ref_resolved: None,
            source_subpath: Some(row.rel_path.clone()),
            source_branch: repo.resolved_branch.clone().or_else(|| repo.branch.clone()),
            source_revision: repo.head_revision.clone(),
            remote_revision: repo.head_revision.clone(),
            central_path: dir.to_string_lossy().to_string(),
            content_hash: Some(hash),
            enabled: true,
            created_at: now,
            updated_at: now,
            status: "ok".to_string(),
            update_status: "up_to_date".to_string(),
            last_checked_at: Some(now),
            last_check_error: None,
        };
        store.upsert_skill(&record)?;
        store.set_synced_repo_skill_state(slug, &row.rel_path, "picked", Some(&id))?;
        if let Some(scenario_id) = active.as_deref() {
            store.add_skill_to_scenario(scenario_id, &id)?;
            if let Err(e) = scenario_service::sync_skill_to_active_scenario(store, scenario_id, &id) {
                log::warn!("synced repo {slug}: deploying {name} failed: {}", e.message);
            }
        }
        ids.push(id);
    }
    Ok(ids)
}

/// Remove a skill's tool targets, re-create them from its current
/// `central_path` and re-add them to the active preset's tools.
fn redeploy(store: &SkillStore, skill_id: &str) -> Result<()> {
    for target in store.get_targets_for_skill(skill_id)? {
        sync_engine::remove_recorded_target_or_warn(Path::new(&target.target_path), &target.mode);
        store.delete_target(skill_id, &target.tool)?;
    }
    for scenario_id in store.get_scenarios_for_skill(skill_id)? {
        if let Err(e) = scenario_service::sync_skill_to_active_scenario(store, &scenario_id, skill_id) {
            log::warn!("synced skill {skill_id}: redeploy failed: {}", e.message);
        }
    }
    Ok(())
}

/// Re-hash every picked skill of `slug` through the `current` link and, when
/// its content changed, update the record and refresh copy-mode targets.
/// Returns the number of skills whose content changed.
pub(super) fn rehash_picked(store: &SkillStore, slug: &str, revision: &str) -> Result<usize> {
    let mut changed = 0;
    for row in store.list_synced_repo_skills(slug)? {
        if row.state != "picked" {
            continue;
        }
        let Some(skill_id) = row.skill_id.as_deref() else { continue };
        let Some(skill) = store.get_skill_by_id(skill_id)? else { continue };
        let dir = Path::new(&skill.central_path);
        if !dir.is_dir() {
            continue;
        }
        let hash = content_hash::hash_directory(dir)?;
        if skill.content_hash.as_deref() != Some(hash.as_str()) {
            let meta = skill_metadata::parse_skill_md(dir);
            store.update_skill_after_install(
                skill_id,
                &skill_metadata::infer_skill_name(dir),
                meta.description.as_deref(),
                Some(revision),
                Some(revision),
                Some(&hash),
                "up_to_date",
            )?;
            crate::commands::skills::resync_copy_targets(store, skill_id).map_err(app_err)?;
            changed += 1;
        } else {
            store.update_skill_check_state(skill_id, Some(revision), "up_to_date", None)?;
        }
    }
    Ok(changed)
}

// ───────────────────────────── refresh ─────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefreshStatus {
    UpToDate,
    Updated,
    /// Local edits in `repo/` stopped the update (`canonical_dirty`).
    Blocked,
}

#[derive(Debug, Clone)]
pub struct RefreshOutcome {
    pub slug: String,
    pub status: RefreshStatus,
    pub updated_skills: usize,
    pub renamed_skills: usize,
    pub removed_upstream: usize,
}

fn record_error(store: &SkillStore, slug: &str, message: &str, state: Option<&str>) {
    if let Ok(Some(mut repo)) = store.get_synced_repo(slug) {
        repo.last_error = Some(message.to_string());
        if let Some(s) = state {
            repo.state = s.to_string();
        }
        let _ = store.update_synced_repo(&repo);
    }
}

/// Refresh one repo from its remote. `background` makes lock contention an
/// immediate "busy" error instead of a wait.
pub fn refresh(
    store: &SkillStore,
    slug: &str,
    proxy: Option<&str>,
    background: bool,
    on_updated: UpdatedCallback<'_>,
) -> Result<RefreshOutcome> {
    let repo = require_repo(store, slug)?;
    let clone = clone_dir(slug);
    if !clone.join(".git").exists() {
        record_error(store, slug, "The local clone is missing", Some("missing_clone"));
        anyhow::bail!("The local clone of '{}' is missing", repo.name);
    }

    // 1. Fetch without the lock: network time never blocks other operations.
    if let Err(e) = synced_repo_git::fetch(&clone, &synced_repo_git::Remote::new(&repo.url, proxy)) {
        let msg = format!("{e:#}");
        let state = synced_repo_git::is_auth_error(&msg).then_some("auth_required");
        record_error(store, slug, &msg, state);
        return Err(e);
    }

    // 2. Everything that touches the working tree runs under the repo lock.
    let _lock = if background {
        SyncedRepoLock::acquire(slug, "refresh")?
    } else {
        SyncedRepoLock::acquire_foreground(slug, "refresh")?
    };
    let mut repo = require_repo(store, slug)?;
    let mut outcome = RefreshOutcome {
        slug: slug.to_string(),
        status: RefreshStatus::UpToDate,
        updated_skills: 0,
        renamed_skills: 0,
        removed_upstream: 0,
    };

    if !synced_repo_git::is_clean(&clone)? {
        repo.state = "canonical_dirty".to_string();
        repo.last_fetched_at = Some(now_ms());
        repo.last_error = None;
        store.update_synced_repo(&repo)?;
        outcome.status = RefreshStatus::Blocked;
        if let Some(cb) = on_updated {
            cb(slug);
        }
        return Ok(outcome);
    }

    let branch = match repo.resolved_branch.clone() {
        Some(b) => b,
        None => synced_repo_git::current_branch(&clone)?,
    };
    let upstream = format!("origin/{branch}");
    let old_head = synced_repo_git::head_revision(&clone)?;
    let new_head = synced_repo_git::rev_parse(&clone, &upstream)
        .with_context(|| format!("branch '{branch}' not found on the remote"))?;
    let was_dirty_state = repo.state != "ok";
    repo.state = "ok".to_string();
    repo.last_fetched_at = Some(now_ms());
    repo.last_error = None;
    repo.resolved_branch = Some(branch.clone());

    if old_head == new_head {
        store.update_synced_repo(&repo)?;
        if was_dirty_state {
            if let Some(cb) = on_updated {
                cb(slug);
            }
        }
        return Ok(outcome);
    }

    // In dev mode the picked skills live in dev/ (rebased on demand through
    // `synced_repo_dev::rebase`); only the canonical clone follows the remote,
    // so dev status can report how far the repo version moved on.
    if repo.dev_mode {
        synced_repo_git::ff_or_reset(&clone, &branch)?;
        repo.head_revision = Some(new_head);
        store.update_synced_repo(&repo)?;
        outcome.status = RefreshStatus::Updated;
        if let Some(cb) = on_updated {
            cb(slug);
        }
        return Ok(outcome);
    }

    // 3. Work out renames and deletions of picked skills while the old tree
    //    is still on disk.
    let picked: Vec<SyncedRepoSkillRecord> = store
        .list_synced_repo_skills(slug)?
        .into_iter()
        .filter(|r| r.state == "picked" && r.skill_id.is_some())
        .collect();
    let diff = synced_repo_git::diff_name_status(&clone, &old_head, &new_head)?;
    let mut renames: Vec<(SyncedRepoSkillRecord, String)> = Vec::new();
    let mut deletions: Vec<SyncedRepoSkillRecord> = Vec::new();
    let subpath = repo.subpath.clone();
    let sub_prefix = subpath.as_deref().filter(|s| !s.is_empty()).map(|s| format!("{}/", s.trim_matches('/')));
    for row in &picked {
        let prefix = join_git(subpath.as_deref(), &row.rel_path);
        let rename_target = diff.iter().find_map(|e| {
            if e.status != 'R' {
                return None;
            }
            let old_parent = e.path.rsplit_once('/').map(|(p, _)| p)?;
            let file = e.path.rsplit('/').next()?;
            if old_parent != prefix || !(file == "SKILL.md" || file == "skill.md") {
                return None;
            }
            let new_path = e.new_path.as_deref()?;
            let new_parent = new_path.rsplit_once('/').map(|(p, _)| p.to_string())?;
            let new_file = new_path.rsplit('/').next()?;
            if !(new_file == "SKILL.md" || new_file == "skill.md") {
                return None;
            }
            match &sub_prefix {
                Some(sp) => new_parent.strip_prefix(sp.as_str()).map(str::to_string),
                None => Some(new_parent),
            }
        });
        if let Some(new_rel) = rename_target {
            if !new_rel.is_empty() && new_rel != row.rel_path {
                renames.push((row.clone(), new_rel));
                continue;
            }
        }
        let still_there = synced_repo_git::exists_at(&clone, &new_head, &format!("{prefix}/SKILL.md"))
            || synced_repo_git::exists_at(&clone, &new_head, &format!("{prefix}/skill.md"));
        if !still_there {
            deletions.push(row.clone());
        }
    }

    // Deleted upstream: keep a copy in orphaned/ and keep the skill working.
    for row in &deletions {
        let skill_id = row.skill_id.clone().unwrap();
        let Some(skill) = store.get_skill_by_id(&skill_id)? else { continue };
        let src = PathBuf::from(&skill.central_path);
        let base = src
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| skill.name.clone());
        let orphan_root = orphaned_dir(slug);
        std::fs::create_dir_all(&orphan_root)?;
        let mut dest = orphan_root.join(&base);
        let mut n = 2;
        while dest.exists() {
            dest = orphan_root.join(format!("{base}-{n}"));
            n += 1;
        }
        if src.is_dir() {
            installer::install_skill_dir_to_destination(&src, &skill.name, &dest)?;
        } else {
            continue;
        }
        store.update_skill_central_path(&skill_id, &dest.to_string_lossy())?;
        store.set_synced_repo_skill_state(slug, &row.rel_path, "removed_upstream", Some(&skill_id))?;
        redeploy(store, &skill_id)?;
        outcome.removed_upstream += 1;
    }

    // 4. Fast-forward (or reset after a force push), then follow renames.
    synced_repo_git::ff_or_reset(&clone, &branch)?;
    repo.head_revision = Some(new_head.clone());

    for (row, new_rel) in &renames {
        let skill_id = row.skill_id.clone().unwrap();
        let new_dir = skill_path(&repo, new_rel);
        if !skill_metadata::is_valid_skill_dir(&new_dir) {
            continue;
        }
        // The new row may already exist as an 'available' discovery.
        store.delete_synced_repo_skill(slug, &new_rel.clone())?;
        store.delete_synced_repo_skill(slug, &row.rel_path)?;
        let meta = skill_metadata::parse_skill_md(&new_dir);
        store.upsert_synced_repo_skill(&SyncedRepoSkillRecord {
            repo_slug: slug.to_string(),
            rel_path: new_rel.clone(),
            skill_id: Some(skill_id.clone()),
            name: skill_metadata::infer_skill_name(&new_dir),
            description: meta.description,
            state: "picked".to_string(),
            first_seen_at: row.first_seen_at,
            last_seen_at: now_ms(),
        })?;
        store.update_skill_central_path(&skill_id, &new_dir.to_string_lossy())?;
        store.update_skill_source_metadata_subpath(&skill_id, new_rel)?;
        redeploy(store, &skill_id)?;
        outcome.renamed_skills += 1;
    }
    store.update_synced_repo(&repo)?;

    // Rescan: new upstream skills only show up as 'available'.
    scan_available(store, slug)?;

    // Re-hash picked skills and refresh their copies.
    outcome.updated_skills += rehash_picked(store, slug, &new_head)?;

    outcome.status = RefreshStatus::Updated;
    if let Some(cb) = on_updated {
        cb(slug);
    }
    Ok(outcome)
}

// ───────────────────────────── resolutions ─────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemovedAction {
    /// Unsubscribe and delete the orphaned copy.
    Remove,
    /// Keep the skill as a personal skill in the library.
    Keep,
}

impl RemovedAction {
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "remove" => Ok(Self::Remove),
            "keep" => Ok(Self::Keep),
            other => anyhow::bail!("Unknown action '{other}' (expected 'remove' or 'keep')"),
        }
    }
}

/// Resolve a skill that disappeared upstream.
pub fn resolve_removed(
    store: &SkillStore,
    slug: &str,
    rel_path: &str,
    action: RemovedAction,
) -> Result<()> {
    let _lock = SyncedRepoLock::acquire_foreground(slug, "resolve-removed")?;
    let row = store
        .get_synced_repo_skill(slug, rel_path)?
        .ok_or_else(|| anyhow::anyhow!("Skill not found in repo: {rel_path}"))?;
    if row.state != "removed_upstream" {
        anyhow::bail!("Skill '{}' was not removed upstream", row.name);
    }
    let Some(skill_id) = row.skill_id.clone() else {
        store.delete_synced_repo_skill(slug, rel_path)?;
        return Ok(());
    };
    let Some(skill) = store.get_skill_by_id(&skill_id)? else {
        store.delete_synced_repo_skill(slug, rel_path)?;
        return Ok(());
    };
    let orphan = PathBuf::from(&skill.central_path);

    match action {
        RemovedAction::Remove => {
            for target in store.get_targets_for_skill(&skill_id)? {
                sync_engine::remove_recorded_target_or_warn(Path::new(&target.target_path), &target.mode);
            }
            store.delete_skill(&skill_id)?;
            store.delete_synced_repo_skill(slug, rel_path)?;
            if orphan.starts_with(orphaned_dir(slug)) {
                let _ = std::fs::remove_dir_all(&orphan);
            }
        }
        RemovedAction::Keep => {
            let _repo_lock = RepoLock::acquire_foreground("synced keep as personal")?;
            let skills_root = central_repo::skills_dir();
            std::fs::create_dir_all(&skills_root)?;
            let base = orphan
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| skill.name.clone());
            let mut dest = skills_root.join(&base);
            let mut n = 2;
            while dest.exists() {
                dest = skills_root.join(format!("{base}-{n}"));
                n += 1;
            }
            let result = installer::install_skill_dir_to_destination(&orphan, &skill.name, &dest)?;
            store.update_skill_after_reinstall(
                &skill_id,
                &result.name,
                result.description.as_deref(),
                "local",
                None,
                None,
                None,
                None,
                None,
                None,
                Some(&result.content_hash),
                "local_only",
            )?;
            for target in store.get_targets_for_skill(&skill_id)? {
                sync_engine::remove_recorded_target_or_warn(Path::new(&target.target_path), &target.mode);
                store.delete_target(&skill_id, &target.tool)?;
            }
            store.update_skill_central_path(&skill_id, &dest.to_string_lossy())?;
            store.delete_synced_repo_skill(slug, rel_path)?;
            for scenario_id in store.get_scenarios_for_skill(&skill_id)? {
                if let Err(e) = scenario_service::sync_skill_to_active_scenario(store, &scenario_id, &skill_id) {
                    log::warn!("synced keep: redeploy failed: {}", e.message);
                }
            }
            if orphan.starts_with(orphaned_dir(slug)) {
                let _ = std::fs::remove_dir_all(&orphan);
            }
            sync_metadata::write_all_from_db_unlocked(store)?;
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirtyAction {
    /// Move the stray edits into the dev worktree.
    MoveToDev,
    /// Throw the edits away.
    Discard,
}

impl DirtyAction {
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "dev" | "move_to_dev" => Ok(Self::MoveToDev),
            "discard" => Ok(Self::Discard),
            other => anyhow::bail!("Unknown action '{other}' (expected 'dev' or 'discard')"),
        }
    }
}

/// Resolve `canonical_dirty`: edits found in `repo/` blocked updates.
pub fn resolve_canonical_dirty(store: &SkillStore, slug: &str, action: DirtyAction) -> Result<()> {
    match action {
        DirtyAction::MoveToDev => {
            super::synced_repo_dev::move_edits_to_dev(store, slug)
        }
        DirtyAction::Discard => {
            let _lock = SyncedRepoLock::acquire_foreground(slug, "discard")?;
            let mut repo = require_repo(store, slug)?;
            synced_repo_git::discard_changes(&clone_dir(slug))?;
            if repo.state == "canonical_dirty" {
                repo.state = "ok".to_string();
                store.update_synced_repo(&repo)?;
            }
            Ok(())
        }
    }
}

// ───────────────────────────── settings & removal ─────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct RepoUpdate {
    pub name: Option<String>,
    pub auto_update: Option<bool>,
    pub branch: Option<String>,
    pub subpath: Option<String>,
}

/// Change repo settings. Changing branch or subpath moves every skill path,
/// so it is only allowed while no skill is picked and the clone is clean.
pub fn update_repo(
    store: &SkillStore,
    slug: &str,
    update: &RepoUpdate,
    proxy: Option<&str>,
) -> Result<SyncedRepoRecord> {
    let _lock = SyncedRepoLock::acquire_foreground(slug, "update")?;
    let mut repo = require_repo(store, slug)?;
    if let Some(name) = update.name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
        repo.name = name.to_string();
    }
    if let Some(auto) = update.auto_update {
        repo.auto_update = auto;
    }

    let new_branch = update
        .branch
        .as_deref()
        .map(str::trim)
        .map(|b| if b.is_empty() { None } else { Some(b.to_string()) });
    let new_subpath = match update.subpath.as_deref() {
        Some(raw) => Some(normalize_subpath(Some(raw))?),
        None => None,
    };
    let branch_changed = new_branch.as_ref().is_some_and(|b| *b != repo.branch);
    let subpath_changed = new_subpath.as_ref().is_some_and(|s| *s != repo.subpath);

    if branch_changed || subpath_changed {
        let has_picked = store
            .list_synced_repo_skills(slug)?
            .iter()
            .any(|r| r.state != "available");
        if has_picked || repo.dev_mode {
            anyhow::bail!("Unpick all skills of this repo before changing its branch or subpath");
        }
        let clone = clone_dir(slug);
        if branch_changed {
            if !synced_repo_git::is_clean(&clone)? {
                anyhow::bail!("The local clone has edits; resolve them before changing the branch");
            }
            synced_repo_git::fetch(&clone, &synced_repo_git::Remote::new(&repo.url, proxy))?;
            let target = new_branch.clone().unwrap();
            let target = match target {
                Some(b) => b,
                None => anyhow::bail!("A branch name is required"),
            };
            synced_repo_git::checkout_tracking(&clone, &target)?;
            repo.branch = Some(target.clone());
            repo.resolved_branch = Some(target);
            repo.head_revision = synced_repo_git::head_revision(&clone).ok();
        }
        if let Some(sp) = new_subpath {
            if let Some(p) = &sp {
                if !clone.join(p).is_dir() {
                    anyhow::bail!("Subpath '{p}' does not exist in the repository");
                }
            }
            repo.subpath = sp;
        }
        for row in store.list_synced_repo_skills(slug)? {
            store.delete_synced_repo_skill(slug, &row.rel_path)?;
        }
        store.update_synced_repo(&repo)?;
        scan_available(store, slug)?;
    } else {
        store.update_synced_repo(&repo)?;
    }
    require_repo(store, slug)
}

/// Unsubscribe from a repo: unpick every skill (including orphaned ones),
/// delete the row and the whole `synced_repos/<slug>` directory.
pub fn remove_repo(store: &SkillStore, slug: &str) -> Result<()> {
    let _lock = SyncedRepoLock::acquire_foreground(slug, "remove")?;
    require_repo(store, slug)?;
    for row in store.list_synced_repo_skills(slug)? {
        let Some(skill_id) = row.skill_id.as_deref() else { continue };
        if store.get_skill_by_id(skill_id)?.is_none() {
            continue;
        }
        for target in store.get_targets_for_skill(skill_id)? {
            sync_engine::remove_recorded_target_or_warn(Path::new(&target.target_path), &target.mode);
        }
        store.delete_skill(skill_id)?;
    }
    store.delete_synced_repo(slug)?;
    let dir = central_repo::synced_repo_dir(slug);
    if std::fs::symlink_metadata(&dir).is_ok() {
        std::fs::remove_dir_all(&dir)
            .with_context(|| format!("Failed to remove {}", dir.display()))?;
    }
    Ok(())
}

/// Refresh every repo that has automatic updates on. Failures are logged per
/// repo and do not stop the round.
pub fn refresh_all_auto(store: &SkillStore, proxy: Option<&str>, on_updated: UpdatedCallback<'_>) {
    let repos = match store.list_synced_repos() {
        Ok(r) => r,
        Err(e) => {
            log::warn!("synced repos: listing failed: {e:#}");
            return;
        }
    };
    for repo in repos.into_iter().filter(|r| r.auto_update) {
        if let Err(e) = refresh(store, &repo.slug, proxy, true, on_updated) {
            log::warn!("synced repo {}: refresh failed: {e:#}", repo.slug);
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::skill_store::{SkillRecord, SkillTargetRecord, SyncedRepoRecord, SyncedRepoSkillRecord};

    struct Env {
        _lock: std::sync::MutexGuard<'static, ()>,
        _tmp: tempfile::TempDir,
        store: SkillStore,
    }

    impl Drop for Env {
        fn drop(&mut self) {
            central_repo::set_test_base_dir_override(None);
        }
    }

    fn env() -> Env {
        let lock = central_repo::test_base_dir_lock();
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("base");
        central_repo::set_test_base_dir_override(Some(base.clone()));
        std::fs::create_dir_all(&base).unwrap();
        let store = SkillStore::new(&base.join("test.db")).unwrap();
        Env { _lock: lock, _tmp: tmp, store }
    }

    fn repo_record(slug: &str) -> SyncedRepoRecord {
        SyncedRepoRecord {
            slug: slug.to_string(),
            name: slug.to_string(),
            url: "https://example.com/r.git".to_string(),
            branch: None,
            resolved_branch: None,
            subpath: None,
            head_revision: None,
            last_fetched_at: None,
            last_error: None,
            auto_update: true,
            state: "ok".to_string(),
            dev_mode: false,
            dev_branch: None,
            dev_base_revision: None,
            pr_url: None,
            sort_order: 0,
            created_at: 1,
        }
    }

    #[test]
    fn unpick_removes_row_and_targets_but_leaves_the_clone_alone() {
        let env = env();
        let skill_dir = central_repo::synced_repo_dir("acme").join("repo/s1");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), "---\nname: s1\n---\n").unwrap();
        let target_dir = env._tmp.path().join("agent/s1");
        std::fs::create_dir_all(&target_dir).unwrap();
        std::fs::write(target_dir.join("SKILL.md"), "x").unwrap();

        env.store.insert_synced_repo(&repo_record("acme")).unwrap();
        env.store
            .insert_skill(&SkillRecord {
                id: "sk".into(),
                name: "s1".into(),
                description: None,
                source_type: "synced_repo".into(),
                source_ref: None,
                source_ref_resolved: None,
                source_subpath: None,
                source_branch: None,
                source_revision: None,
                remote_revision: None,
                central_path: skill_dir.to_string_lossy().into(),
                content_hash: None,
                enabled: true,
                created_at: 1,
                updated_at: 1,
                status: "ok".into(),
                update_status: "unknown".into(),
                last_checked_at: None,
                last_check_error: None,
            })
            .unwrap();
        env.store
            .insert_target(&SkillTargetRecord {
                id: "t".into(),
                skill_id: "sk".into(),
                tool: "claude_code".into(),
                target_path: target_dir.to_string_lossy().into(),
                mode: "copy".into(),
                status: "ok".into(),
                synced_at: Some(1),
                last_error: None,
                source_hash: None,
            })
            .unwrap();
        env.store
            .upsert_synced_repo_skill(&SyncedRepoSkillRecord {
                repo_slug: "acme".into(),
                rel_path: "s1".into(),
                skill_id: Some("sk".into()),
                name: "s1".into(),
                description: None,
                state: "picked".into(),
                first_seen_at: 1,
                last_seen_at: 1,
            })
            .unwrap();

        unpick(&env.store, "sk").unwrap();

        assert!(env.store.get_skill_by_id("sk").unwrap().is_none());
        assert!(!target_dir.exists());
        assert!(skill_dir.join("SKILL.md").is_file(), "clone must be untouched");
        let row = env.store.get_synced_repo_skill("acme", "s1").unwrap().unwrap();
        assert_eq!(row.state, "available");
        assert_eq!(row.skill_id, None);
    }

    #[test]
    fn unpick_refuses_non_synced_skills() {
        let env = env();
        assert!(unpick(&env.store, "missing").is_err());
    }

    #[test]
    #[cfg(unix)]
    fn repair_all_recreates_current_link() {
        let env = env();
        env.store.insert_synced_repo(&repo_record("acme")).unwrap();
        let dir = central_repo::synced_repo_dir("acme");
        std::fs::create_dir_all(dir.join("repo")).unwrap();
        // A stale absolute link, as left behind by a moved base dir.
        std::os::unix::fs::symlink("/nonexistent/old/repo", dir.join("current")).unwrap();

        repair_all(&env.store).unwrap();

        assert_eq!(std::fs::read_link(dir.join("current")).unwrap(), Path::new("repo"));
        assert!(dir.join("current").is_dir());
    }
}

#[cfg(test)]
mod flow_tests {
    use super::*;
    use crate::core::skill_store::{ScenarioRecord, SkillRecord};
    use crate::core::synced_repo_git::test_support::{git, push_change, seeded_remote};
    use crate::core::tool_adapters::{self, CustomToolDef};

    const SKILL_A: &str = "---\nname: a\ndescription: Skill A\n---\nalpha\n";
    const SKILL_B: &str = "---\nname: b\ndescription: Skill B\n---\nbeta\n";

    struct Flow {
        _lock: std::sync::MutexGuard<'static, ()>,
        tmp: tempfile::TempDir,
        store: SkillStore,
        url: String,
        work: PathBuf,
        agent: PathBuf,
    }

    impl Drop for Flow {
        fn drop(&mut self) {
            central_repo::set_test_base_dir_override(None);
        }
    }

    fn flow(mode: &str) -> Flow {
        git_credentials::use_mock_keyring();
        let lock = central_repo::test_base_dir_lock();
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("base");
        std::fs::create_dir_all(&base).unwrap();
        central_repo::set_test_base_dir_override(Some(base.clone()));
        let store = SkillStore::new(&base.join("test.db")).unwrap();

        // Only a custom tool in the temp dir; built-in tools are disabled so
        // nothing is ever deployed into the real home directory.
        let agent = tmp.path().join("agent-skills");
        std::fs::create_dir_all(&agent).unwrap();
        let builtin: Vec<String> = tool_adapters::all_tool_adapters(&store).into_iter().map(|a| a.key).collect();
        store.set_setting("disabled_tools", &serde_json::to_string(&builtin).unwrap()).unwrap();
        let custom = vec![CustomToolDef {
            key: "test_tool".into(),
            display_name: "Test Tool".into(),
            skills_dir: agent.to_string_lossy().into(),
            project_relative_skills_dir: None,
            category: Default::default(),
        }];
        store.set_setting("custom_tools", &serde_json::to_string(&custom).unwrap()).unwrap();
        store.set_setting("sync_mode", mode).unwrap();
        store
            .insert_scenario(&ScenarioRecord {
                id: "p1".into(),
                name: "P1".into(),
                description: None,
                icon: None,
                sort_order: 0,
                created_at: 1,
                updated_at: 1,
            })
            .unwrap();
        store.set_active_scenario("p1").unwrap();

        let (bare, work) = seeded_remote(
            tmp.path(),
            &[("skills/a/SKILL.md", SKILL_A), ("skills/b/SKILL.md", SKILL_B), ("README.md", "hi")],
        );
        Flow { _lock: lock, tmp, store, url: bare.to_string_lossy().into(), work, agent }
    }

    impl Flow {
        fn add(&self) -> SyncedRepoRecord {
            let req = AddRequest { url: self.url.clone(), ..Default::default() };
            add_inner(&self.store, &req, None, true).unwrap()
        }
        fn pick(&self, slug: &str, rel: &str) -> String {
            pick(&self.store, slug, &[rel.to_string()], &[]).unwrap().remove(0)
        }
        fn refresh(&self, slug: &str) -> RefreshOutcome {
            refresh(&self.store, slug, None, false, None).unwrap()
        }
        fn push(&self, msg: &str) {
            push_change(&self.work, msg);
        }
    }

    #[test]
    fn slug_and_ids_are_derived_from_the_url() {
        assert_eq!(slugify(&repo_name_from_url("https://github.com/acme/Agent_Skills.git")), "agent_skills");
        assert_eq!(repo_name_from_url("git@github.com:acme/agent-skills.git"), "agent-skills");
        assert_eq!(slugify(" Weird Name!! "), "weird-name");
        let a = derive_skill_id("u", "main", "skills/a");
        assert_eq!(a, derive_skill_id("u", "main", "skills/a"));
        assert_ne!(a, derive_skill_id("u", "main", "skills/b"));
        assert_ne!(a, derive_skill_id("u", "dev", "skills/a"));
        assert!(uuid::Uuid::parse_str(&a).is_ok());
        assert!(normalize_subpath(Some("../x")).is_err());
        assert_eq!(normalize_subpath(Some("/skills/")).unwrap().as_deref(), Some("skills"));
    }

    #[test]
    fn adding_a_repo_lists_skills_without_installing_any() {
        let f = flow("symlink");
        let repo = f.add();
        assert_eq!(repo.slug, "remote");
        assert_eq!(repo.resolved_branch.as_deref(), Some("main"));
        assert!(central_repo::synced_repo_dir("remote").join("orphaned").is_dir());
        assert_eq!(
            std::fs::read_link(central_repo::synced_repo_dir("remote").join("current")).unwrap(),
            Path::new("repo")
        );
        let skills = list_skills(&f.store, "remote").unwrap();
        let paths: Vec<_> = skills.iter().map(|s| (s.record.rel_path.as_str(), s.record.state.as_str())).collect();
        assert_eq!(paths, vec![("skills/a", "available"), ("skills/b", "available")]);
        assert_eq!(skills[0].record.description.as_deref(), Some("Skill A"));
        assert!(f.store.get_all_skills().unwrap().is_empty());
        assert_eq!(std::fs::read_dir(&f.agent).unwrap().count(), 0);

        // Same remote again: slug collision gets -2.
        let again = f.add();
        assert_eq!(again.slug, "remote-2");
    }

    #[test]
    fn picking_deploys_a_working_link_and_adds_to_the_active_preset() {
        let f = flow("symlink");
        f.add();
        let id = f.pick("remote", "skills/a");
        let skill = f.store.get_skill_by_id(&id).unwrap().unwrap();
        assert_eq!(skill.source_type, "synced_repo");
        assert!(skill.central_path.ends_with("remote/current/skills/a"));
        assert_eq!(f.store.get_scenarios_for_skill(&id).unwrap(), vec!["p1"]);
        let link = f.agent.join("a");
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(link.join("SKILL.md")).unwrap(), SKILL_A);
        let rows = list_skills(&f.store, "remote").unwrap();
        assert_eq!(rows[0].record.state, "picked");
        assert_eq!(rows[0].record.skill_id.as_deref(), Some(id.as_str()));
        assert_eq!(rows[1].record.state, "available");

        // Symlink users see remote edits immediately after fast-forward.
        std::fs::write(f.work.join("skills/a/SKILL.md"), "---\nname: a\n---\nv2\n").unwrap();
        f.push("v2");
        f.refresh("remote");
        assert!(std::fs::read_to_string(link.join("SKILL.md")).unwrap().contains("v2"));

        unpick(&f.store, &id).unwrap();
        assert!(std::fs::symlink_metadata(&link).is_err());
        assert!(central_repo::synced_repo_dir("remote").join("repo/skills/a/SKILL.md").is_file());
    }

    #[test]
    fn remote_edits_update_hash_and_copies() {
        let f = flow("copy");
        f.add();
        let id = f.pick("remote", "skills/a");
        let before = f.store.get_skill_by_id(&id).unwrap().unwrap();
        let copy = f.agent.join("a");
        assert!(!std::fs::symlink_metadata(&copy).unwrap().file_type().is_symlink());

        std::fs::write(f.work.join("skills/a/SKILL.md"), "---\nname: a\n---\nchanged upstream\n").unwrap();
        f.push("edit");
        let out = f.refresh("remote");
        assert_eq!(out.status, RefreshStatus::Updated);
        assert_eq!(out.updated_skills, 1);

        let after = f.store.get_skill_by_id(&id).unwrap().unwrap();
        assert_ne!(before.content_hash, after.content_hash);
        assert_eq!(after.source_revision, f.store.get_synced_repo("remote").unwrap().unwrap().head_revision);
        assert!(std::fs::read_to_string(copy.join("SKILL.md")).unwrap().contains("changed upstream"));

        // Nothing new upstream: no-op.
        assert_eq!(f.refresh("remote").status, RefreshStatus::UpToDate);
    }

    #[test]
    fn new_remote_skills_only_show_as_available() {
        let f = flow("symlink");
        f.add();
        f.pick("remote", "skills/a");
        std::fs::create_dir_all(f.work.join("skills/c")).unwrap();
        std::fs::write(f.work.join("skills/c/SKILL.md"), "---\nname: c\n---\n").unwrap();
        f.push("add c");
        f.refresh("remote");
        let rows = list_skills(&f.store, "remote").unwrap();
        let c = rows.iter().find(|r| r.record.rel_path == "skills/c").unwrap();
        assert_eq!(c.record.state, "available");
        assert_eq!(f.store.get_all_skills().unwrap().len(), 1);
        assert!(!f.agent.join("c").exists());
    }

    #[test]
    fn deleted_skills_move_to_orphaned_and_keep_working() {
        let f = flow("symlink");
        f.add();
        let id = f.pick("remote", "skills/a");
        git(&f.work, &["rm", "-r", "-q", "skills/a"]);
        f.push("delete a");
        let out = f.refresh("remote");
        assert_eq!(out.removed_upstream, 1);

        let skill = f.store.get_skill_by_id(&id).unwrap().unwrap();
        let orphan = central_repo::synced_repo_dir("remote").join("orphaned/a");
        assert_eq!(skill.central_path, orphan.to_string_lossy());
        assert!(orphan.join("SKILL.md").is_file());
        assert_eq!(std::fs::read_to_string(f.agent.join("a/SKILL.md")).unwrap(), SKILL_A);
        let row = f.store.get_synced_repo_skill("remote", "skills/a").unwrap().unwrap();
        assert_eq!(row.state, "removed_upstream");

        // Keep as a personal skill.
        resolve_removed(&f.store, "remote", "skills/a", RemovedAction::Keep).unwrap();
        let skill = f.store.get_skill_by_id(&id).unwrap().unwrap();
        assert_eq!(skill.source_type, "local");
        assert_eq!(skill.central_path, central_repo::skills_dir().join("a").to_string_lossy());
        assert!(!orphan.exists());
        assert_eq!(std::fs::read_to_string(f.agent.join("a/SKILL.md")).unwrap(), SKILL_A);
        assert!(f.store.get_synced_repo_skill("remote", "skills/a").unwrap().is_none());
    }

    #[test]
    fn removed_skill_can_be_dropped() {
        let f = flow("symlink");
        f.add();
        let id = f.pick("remote", "skills/b");
        git(&f.work, &["rm", "-r", "-q", "skills/b"]);
        f.push("delete b");
        f.refresh("remote");
        resolve_removed(&f.store, "remote", "skills/b", RemovedAction::Remove).unwrap();
        assert!(f.store.get_skill_by_id(&id).unwrap().is_none());
        assert!(std::fs::symlink_metadata(f.agent.join("b")).is_err());
        assert!(!central_repo::synced_repo_dir("remote").join("orphaned/b").exists());
    }

    #[test]
    fn renames_are_followed() {
        let f = flow("symlink");
        f.add();
        let id = f.pick("remote", "skills/a");
        git(&f.work, &["mv", "skills/a", "skills/renamed"]);
        f.push("rename");
        let out = f.refresh("remote");
        assert_eq!(out.renamed_skills, 1);
        assert_eq!(out.removed_upstream, 0);

        let skill = f.store.get_skill_by_id(&id).unwrap().unwrap();
        assert!(skill.central_path.ends_with("remote/current/skills/renamed"));
        assert_eq!(skill.source_subpath.as_deref(), Some("skills/renamed"));
        assert!(f.store.get_synced_repo_skill("remote", "skills/a").unwrap().is_none());
        let row = f.store.get_synced_repo_skill("remote", "skills/renamed").unwrap().unwrap();
        assert_eq!((row.state.as_str(), row.skill_id.as_deref()), ("picked", Some(id.as_str())));
        // Re-deployed under the new folder name and still readable.
        assert_eq!(std::fs::read_to_string(f.agent.join("renamed/SKILL.md")).unwrap(), SKILL_A);
        assert!(std::fs::symlink_metadata(f.agent.join("a")).is_err());
    }

    #[test]
    fn edits_in_the_clone_block_updates_until_discarded() {
        let f = flow("symlink");
        f.add();
        let id = f.pick("remote", "skills/a");
        let clone = clone_dir("remote");
        std::fs::write(clone.join("skills/a/SKILL.md"), "---\nname: a\n---\nlocal edit\n").unwrap();
        std::fs::write(f.work.join("skills/a/SKILL.md"), "---\nname: a\n---\nremote edit\n").unwrap();
        f.push("remote edit");

        let out = f.refresh("remote");
        assert_eq!(out.status, RefreshStatus::Blocked);
        assert_eq!(f.store.get_synced_repo("remote").unwrap().unwrap().state, "canonical_dirty");
        assert!(std::fs::read_to_string(f.agent.join("a/SKILL.md")).unwrap().contains("local edit"));

        resolve_canonical_dirty(&f.store, "remote", DirtyAction::Discard).unwrap();
        assert_eq!(f.refresh("remote").status, RefreshStatus::Updated);
        let repo = f.store.get_synced_repo("remote").unwrap().unwrap();
        assert_eq!(repo.state, "ok");
        assert!(std::fs::read_to_string(f.agent.join("a/SKILL.md")).unwrap().contains("remote edit"));
        assert!(f.store.get_skill_by_id(&id).unwrap().is_some());
    }

    fn personal_skill(f: &Flow, name: &str) -> String {
        let dir = central_repo::skills_dir().join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), format!("---\nname: {name}\n---\nmine\n")).unwrap();
        let id = format!("personal-{name}");
        f.store
            .insert_skill(&SkillRecord {
                id: id.clone(),
                name: name.into(),
                description: None,
                source_type: "local".into(),
                source_ref: None,
                source_ref_resolved: None,
                source_subpath: None,
                source_branch: None,
                source_revision: None,
                remote_revision: None,
                central_path: dir.to_string_lossy().into(),
                content_hash: None,
                enabled: true,
                created_at: 1,
                updated_at: 1,
                status: "ok".into(),
                update_status: "unknown".into(),
                last_checked_at: None,
                last_check_error: None,
            })
            .unwrap();
        id
    }

    #[test]
    fn name_clashes_with_personal_and_synced_skills() {
        let f = flow("symlink");
        let personal = personal_skill(&f, "a");
        f.add();

        let rows = list_skills(&f.store, "remote").unwrap();
        let clash = rows[0].clash.clone().unwrap();
        assert_eq!((clash.kind.as_str(), clash.skill_id.as_str()), ("personal", personal.as_str()));
        assert!(rows[1].clash.is_none());

        // Cancel: nothing changes, not even for the other selected skill.
        let err = pick(&f.store, "remote", &["skills/b".into(), "skills/a".into()], &[]).unwrap_err();
        assert!(err.to_string().contains("Name clash"), "{err}");
        assert_eq!(f.store.get_all_skills().unwrap().len(), 1);

        // Replace: the personal skill is deleted, the synced one takes over.
        let ids = pick(&f.store, "remote", &["skills/a".into()], &[personal.clone()]).unwrap();
        assert!(f.store.get_skill_by_id(&personal).unwrap().is_none());
        assert!(!central_repo::skills_dir().join("a").exists());
        assert_eq!(f.store.get_skill_by_id(&ids[0]).unwrap().unwrap().source_type, "synced_repo");

        // A second repo offering the same folder name clashes with the synced skill.
        std::fs::remove_dir_all(f.tmp.path().join("seed")).ok();
        let second = f.tmp.path().join("second");
        std::fs::create_dir_all(&second).unwrap();
        let (bare2, _) = seeded_remote(&second, &[("x/a/SKILL.md", SKILL_A)]);
        let req = AddRequest { url: bare2.to_string_lossy().into(), ..Default::default() };
        let repo2 = add_inner(&f.store, &req, None, true).unwrap();
        assert_eq!(repo2.slug, "remote-2");
        let rows = list_skills(&f.store, &repo2.slug).unwrap();
        assert_eq!(rows[0].clash.as_ref().unwrap().kind, "synced");
        let err = pick(&f.store, &repo2.slug, &["x/a".into()], &ids).unwrap_err();
        assert!(err.to_string().contains("unpick it first"), "{err}");
    }

    #[test]
    fn removing_a_repo_unpicks_everything_and_deletes_the_directory() {
        let f = flow("symlink");
        f.add();
        let id = f.pick("remote", "skills/a");
        remove_repo(&f.store, "remote").unwrap();
        assert!(f.store.get_skill_by_id(&id).unwrap().is_none());
        assert!(f.store.get_synced_repo("remote").unwrap().is_none());
        assert!(!central_repo::synced_repo_dir("remote").exists());
        assert!(std::fs::symlink_metadata(f.agent.join("a")).is_err());
    }

    // ───────────── dev mode ─────────────

    use crate::core::synced_repo_dev as dev;

    fn add_sub(f: &Flow) -> SyncedRepoRecord {
        let req = AddRequest { url: f.url.clone(), subpath: Some("skills".into()), ..Default::default() };
        add_inner(&f.store, &req, None, true).unwrap()
    }

    fn link_target(slug: &str) -> PathBuf {
        std::fs::read_link(central_repo::synced_repo_dir(slug).join("current")).unwrap()
    }

    fn dev_path(slug: &str, rel: &str) -> PathBuf {
        central_repo::synced_repo_dir(slug).join("dev").join(rel)
    }

    /// Fetch the dev branch into the seed clone and bring it into main.
    fn merge_branch_upstream(f: &Flow, branch: &str, squash: bool) {
        git(&f.work, &["fetch", "--quiet", "origin", branch]);
        if squash {
            git(&f.work, &["merge", "--squash", "FETCH_HEAD"]);
            git(&f.work, &["commit", "--quiet", "-m", "squashed"]);
        } else {
            git(&f.work, &["merge", "--quiet", "--no-edit", "FETCH_HEAD"]);
        }
        git(&f.work, &["push", "--quiet", "origin", "main"]);
    }

    #[test]
    fn dev_toggle_swaps_current_and_keeps_edits() {
        let f = flow("copy");
        add_sub(&f);
        let id = f.pick("remote", "a");
        let copy = f.agent.join("a/SKILL.md");

        let repo = dev::set_dev_mode(&f.store, "remote", true).unwrap();
        assert!(repo.dev_mode);
        let branch = repo.dev_branch.clone().unwrap();
        assert!(branch.starts_with("skills-manager/dev-"));
        assert_eq!(repo.dev_base_revision, repo.head_revision);
        assert_eq!(link_target("remote"), Path::new("dev"));
        assert!(dev_path("remote", "skills/a/SKILL.md").is_file());

        std::fs::write(dev_path("remote", "skills/a/SKILL.md"), "---\nname: a\n---\nmy edit\n").unwrap();
        // Off: the picked skill reads the repo version again, edits stay in dev/.
        let repo = dev::set_dev_mode(&f.store, "remote", false).unwrap();
        assert!(!repo.dev_mode);
        assert_eq!(link_target("remote"), Path::new("repo"));
        assert_eq!(std::fs::read_to_string(&copy).unwrap(), SKILL_A);
        assert!(std::fs::read_to_string(dev_path("remote", "skills/a/SKILL.md")).unwrap().contains("my edit"));

        // On again: same worktree and branch, edits picked up by the copy.
        let repo = dev::set_dev_mode(&f.store, "remote", true).unwrap();
        assert_eq!(repo.dev_branch.as_deref(), Some(branch.as_str()));
        assert_eq!(link_target("remote"), Path::new("dev"));
        assert!(std::fs::read_to_string(&copy).unwrap().contains("my edit"));
        assert!(f.store.get_skill_by_id(&id).unwrap().is_some());
    }

    #[test]
    fn refresh_keeps_updating_the_repo_version_in_dev_mode() {
        let f = flow("symlink");
        add_sub(&f);
        f.pick("remote", "a");
        dev::set_dev_mode(&f.store, "remote", true).unwrap();
        std::fs::write(f.work.join("skills/b/SKILL.md"), "---\nname: b\n---\nup\n").unwrap();
        f.push("upstream b");
        f.refresh("remote");
        let st = dev::status(&f.store, "remote").unwrap();
        assert!(st.dev_mode);
        assert_eq!((st.ahead_of_repo, st.repo_ahead_by), (0, 1));
        assert!(st.changed_skills.is_empty());
        // The dev tree is untouched until rebased.
        assert_eq!(std::fs::read_to_string(dev_path("remote", "skills/b/SKILL.md")).unwrap(), SKILL_B);
    }

    #[test]
    fn dev_diff_is_taken_against_the_merge_base() {
        let f = flow("symlink");
        add_sub(&f);
        f.pick("remote", "a");
        dev::set_dev_mode(&f.store, "remote", true).unwrap();

        // Upstream moves on in b; locally a is committed + extended, README is outside.
        std::fs::write(f.work.join("skills/b/SKILL.md"), "---\nname: b\n---\nupstream\n").unwrap();
        f.push("upstream b");
        f.refresh("remote");
        let dev_a = dev_path("remote", "skills/a");
        std::fs::write(dev_a.join("SKILL.md"), "---\nname: a\ndescription: Skill A\n---\nmine\n").unwrap();
        crate::core::synced_repo_git::commit_all_in(&dev_path("remote", ""), "mine").unwrap();
        std::fs::write(dev_a.join("extra.md"), "more\n").unwrap();
        std::fs::write(dev_path("remote", "README.md"), "changed\n").unwrap();

        let d = dev::diff(&f.store, "remote").unwrap();
        assert_eq!(d.skills.len(), 1, "upstream-only change in b must not show");
        assert_eq!(d.skills[0].rel_path, "a");
        let entries = &d.skills[0].entries;
        assert_eq!(entries.len(), 2);
        assert_eq!((entries[0].relative_path.as_str(), entries[0].status.as_str()), ("SKILL.md", "modified"));
        assert_eq!(entries[0].original_text.as_deref(), Some(SKILL_A));
        assert!(entries[0].updated_text.as_deref().unwrap().contains("mine"));
        assert_eq!((entries[1].relative_path.as_str(), entries[1].status.as_str()), ("extra.md", "added"));

        let st = dev::status(&f.store, "remote").unwrap();
        assert_eq!(st.changed_skills, vec![dev::ChangedSkill { rel_path: "a".into(), name: "a".into(), files_changed: 2 }]);
        assert_eq!(st.outside_subfolder_files, vec!["README.md".to_string()]);
        assert_eq!((st.ahead_of_repo, st.repo_ahead_by, st.unpushed_commits), (1, 1, 1));
        assert!(!st.merged);
    }

    #[test]
    fn push_creates_the_branch_and_reports_files_outside_the_subfolder() {
        let f = flow("symlink");
        add_sub(&f);
        dev::set_dev_mode(&f.store, "remote", true).unwrap();
        std::fs::write(dev_path("remote", "skills/a/SKILL.md"), "---\nname: a\n---\nmine\n").unwrap();
        std::fs::write(dev_path("remote", "README.md"), "outside\n").unwrap();

        let pushed = dev::commit_and_push(&f.store, "remote", "Improve a", "Because", None).unwrap();
        assert_eq!(pushed.outside_subfolder_files, vec!["README.md".to_string()]);
        assert_eq!(pushed.base_branch, "main");
        let bare = std::path::Path::new(&f.url);
        let remote_sha = git(bare, &["rev-parse", &format!("refs/heads/{}", pushed.branch)]);
        assert!(!remote_sha.is_empty());
        assert!(git(bare, &["show", &format!("{}:skills/a/SKILL.md", pushed.branch)]).contains("mine"));
        assert_eq!(git(bare, &["show", &format!("{}:README.md", pushed.branch)]), "hi", "outside file not committed");
        assert!(git(bare, &["log", "-1", "--format=%B", &pushed.branch]).contains("Improve a"));

        let st = dev::status(&f.store, "remote").unwrap();
        assert_eq!(st.unpushed_commits, 0);
        assert_eq!(st.outside_subfolder_files, vec!["README.md".to_string()]);

        // Nothing new: refuses; link building fails for a local remote after pushing.
        assert!(dev::commit_and_push(&f.store, "remote", "again", "", None).is_ok());
        let err = dev::open_pr(&f.store, "remote", "t", "", None).unwrap_err();
        assert!(err.to_string().contains("was pushed, but no pull request link"), "{err}");
    }

    #[test]
    fn rebase_clean_and_with_conflicts() {
        let f = flow("symlink");
        add_sub(&f);
        f.pick("remote", "a");
        dev::set_dev_mode(&f.store, "remote", true).unwrap();
        let a = dev_path("remote", "skills/a/SKILL.md");

        // Clean: my edit on a, upstream edit on b.
        std::fs::write(&a, "---\nname: a\n---\nmine\n").unwrap();
        std::fs::write(f.work.join("skills/b/SKILL.md"), "---\nname: b\n---\nup\n").unwrap();
        f.push("upstream b");
        let r = dev::rebase(&f.store, "remote", None).unwrap();
        assert_eq!((r.status.as_str(), r.conflicted_files.len()), ("ok", 0));
        assert!(std::fs::read_to_string(dev_path("remote", "skills/b/SKILL.md")).unwrap().contains("up"));
        assert!(std::fs::read_to_string(&a).unwrap().contains("mine"));
        let st = dev::status(&f.store, "remote").unwrap();
        assert_eq!((st.ahead_of_repo, st.repo_ahead_by), (1, 0));
        let repo = f.store.get_synced_repo("remote").unwrap().unwrap();
        assert_eq!(repo.dev_base_revision, repo.head_revision);

        // Conflict: an uncommitted edit of a meets an upstream edit of a.
        std::fs::write(&a, "---\nname: a\n---\nmine v2\n").unwrap();
        std::fs::write(f.work.join("skills/a/SKILL.md"), "---\nname: a\n---\ntheirs\n").unwrap();
        f.push("upstream a");
        let r = dev::rebase(&f.store, "remote", None).unwrap();
        assert_eq!(r.status, "conflict");
        assert_eq!(r.conflicted_files, vec!["skills/a/SKILL.md".to_string()]);
        // Aborted: no rebase in progress, my edit is still there, uncommitted.
        assert!(std::fs::read_to_string(&a).unwrap().contains("mine v2"));
        assert!(!dev_path("remote", "").join(".git").is_dir());
        let st = dev::status(&f.store, "remote").unwrap();
        assert_eq!((st.ahead_of_repo, st.repo_ahead_by), (1, 1));
        assert!(!crate::core::synced_repo_git::is_clean(&dev_path("remote", "")).unwrap());
    }

    #[test]
    fn discard_saves_a_patch_and_resets_to_the_repo_version() {
        let f = flow("symlink");
        add_sub(&f);
        dev::set_dev_mode(&f.store, "remote", true).unwrap();
        std::fs::write(dev_path("remote", "skills/a/SKILL.md"), "---\nname: a\n---\nmine\n").unwrap();
        std::fs::write(dev_path("remote", "skills/a/new.md"), "new\n").unwrap();
        let patch = dev::discard(&f.store, "remote").unwrap().unwrap();
        let text = std::fs::read_to_string(&patch).unwrap();
        assert!(text.contains("mine") && text.contains("new.md"));
        assert_eq!(std::fs::read_to_string(dev_path("remote", "skills/a/SKILL.md")).unwrap(), SKILL_A);
        assert!(!dev_path("remote", "skills/a/new.md").exists());
        assert!(dev::discard(&f.store, "remote").unwrap().is_none());
    }

    fn propose_and_finish_setup(f: &Flow) -> String {
        add_sub(f);
        f.pick("remote", "a");
        dev::set_dev_mode(&f.store, "remote", true).unwrap();
        std::fs::write(dev_path("remote", "skills/a/SKILL.md"), "---\nname: a\n---\nmine\n").unwrap();
        dev::commit_and_push(&f.store, "remote", "Improve a", "", None).unwrap().branch
    }

    fn assert_finished(f: &Flow, branch: &str) {
        assert!(!central_repo::synced_repo_dir("remote").join("dev").exists());
        assert!(!crate::core::synced_repo_git::branch_exists(&clone_dir("remote"), branch));
        assert_eq!(link_target("remote"), Path::new("repo"));
        let repo = f.store.get_synced_repo("remote").unwrap().unwrap();
        assert!(!repo.dev_mode && repo.dev_branch.is_none());
        // Picked skill now serves the merged repo version.
        assert!(std::fs::read_to_string(f.agent.join("a/SKILL.md")).unwrap().contains("mine"));
    }

    #[test]
    fn finish_after_a_merge() {
        let f = flow("symlink");
        let branch = propose_and_finish_setup(&f);
        // Not merged yet.
        assert!(dev::finish(&f.store, "remote", false, None).is_err());
        merge_branch_upstream(&f, &branch, false);
        let r = dev::finish(&f.store, "remote", false, None).unwrap();
        assert_eq!((r.status.as_str(), r.unpushed_commits), ("finished", 0));
        assert_finished(&f, &branch);
    }

    #[test]
    fn finish_after_a_squash_merge() {
        let f = flow("symlink");
        let branch = propose_and_finish_setup(&f);
        merge_branch_upstream(&f, &branch, true);
        f.refresh("remote");
        let st = dev::status(&f.store, "remote").unwrap();
        assert!(st.merged, "squash merge detected by content");
        let r = dev::finish(&f.store, "remote", false, None).unwrap();
        assert_eq!(r.status, "finished");
        assert_finished(&f, &branch);
    }

    #[test]
    fn finish_with_never_pushed_commits_needs_confirmation() {
        let f = flow("symlink");
        add_sub(&f);
        f.pick("remote", "a");
        dev::set_dev_mode(&f.store, "remote", true).unwrap();
        let edit = "---\nname: a\n---\nmine\n";
        std::fs::write(dev_path("remote", "skills/a/SKILL.md"), edit).unwrap();
        crate::core::synced_repo_git::commit_all_in(&dev_path("remote", ""), "local only").unwrap();
        // Someone lands the same content upstream by other means.
        std::fs::write(f.work.join("skills/a/SKILL.md"), edit).unwrap();
        f.push("same change");

        let r = dev::finish(&f.store, "remote", false, None).unwrap();
        assert_eq!((r.status.as_str(), r.unpushed_commits), ("needs_confirmation", 1));
        assert!(dev_path("remote", "skills/a").is_dir(), "nothing was removed");
        let r = dev::finish(&f.store, "remote", true, None).unwrap();
        assert_eq!(r.status, "finished");
        assert!(!central_repo::synced_repo_dir("remote").join("dev").exists());
    }

    #[test]
    fn stray_edits_move_into_dev_mode() {
        let f = flow("symlink");
        f.add();
        f.pick("remote", "skills/a");
        let clone = clone_dir("remote");
        std::fs::write(clone.join("skills/a/SKILL.md"), "---\nname: a\n---\nstray\n").unwrap();
        std::fs::write(f.work.join("README.md"), "upstream\n").unwrap();
        f.push("upstream readme");
        assert_eq!(f.refresh("remote").status, RefreshStatus::Blocked);

        resolve_canonical_dirty(&f.store, "remote", DirtyAction::MoveToDev).unwrap();
        let repo = f.store.get_synced_repo("remote").unwrap().unwrap();
        assert!(repo.dev_mode);
        assert_eq!(repo.state, "ok");
        assert!(crate::core::synced_repo_git::is_clean(&clone).unwrap());
        assert!(std::fs::read_to_string(dev_path("remote", "skills/a/SKILL.md")).unwrap().contains("stray"));
        assert!(std::fs::read_to_string(f.agent.join("a/SKILL.md")).unwrap().contains("stray"));
        // repo/ can follow upstream again; dev reports it is behind.
        f.refresh("remote");
        let st = dev::status(&f.store, "remote").unwrap();
        assert_eq!(st.repo_ahead_by, 1);
        assert_eq!(st.changed_skills.len(), 1);
    }
}
