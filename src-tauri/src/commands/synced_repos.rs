use std::sync::Arc;

use serde::Serialize;
use tauri::{AppHandle, Emitter, State};

use crate::core::error::AppError;
use crate::core::skill_store::{SkillStore, SyncedRepoRecord};
use crate::core::file_manager;
use crate::core::synced_repo_auto_sync::EVENT_UPDATED;
use crate::core::synced_repo_dev as dev;
use crate::core::synced_repos::{self, AddRequest, DirtyAction, RefreshStatus, RemovedAction, RepoUpdate};

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SyncedRepoDto {
    pub slug: String,
    pub name: String,
    pub url: String,
    pub branch: Option<String>,
    pub resolved_branch: Option<String>,
    pub subpath: Option<String>,
    pub head_revision: Option<String>,
    pub last_fetched_at: Option<i64>,
    pub last_error: Option<String>,
    pub auto_update: bool,
    pub state: String,
    pub dev_mode: bool,
    pub dev_branch: Option<String>,
    pub dev_base_revision: Option<String>,
    pub pr_url: Option<String>,
    pub picked_count: usize,
    pub available_count: usize,
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SyncedRepoClashDto {
    /// `personal` or `synced`.
    pub kind: String,
    pub skill_id: String,
    pub skill_name: String,
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SyncedRepoSkillDto {
    pub rel_path: String,
    pub name: String,
    pub description: Option<String>,
    /// `available` | `picked` | `removed_upstream`
    pub state: String,
    pub skill_id: Option<String>,
    pub clash: Option<SyncedRepoClashDto>,
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SyncedRepoRefreshDto {
    pub slug: String,
    /// `up_to_date` | `updated` | `blocked`
    pub status: String,
    pub updated_skills: usize,
    pub renamed_skills: usize,
    pub removed_upstream: usize,
    pub error: Option<String>,
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DevDiscardDto {
    /// Where the discarded edits were saved, if there were any.
    pub patch_path: Option<String>,
}

fn map_err(e: anyhow::Error) -> AppError {
    let message = format!("{e:#}");
    if message.contains("git command failed") || message.contains("busy") {
        AppError::classify_git_error(message)
    } else if message.contains("not found") {
        AppError::not_found(message)
    } else {
        AppError::invalid_input(message)
    }
}

fn repo_to_dto(store: &SkillStore, repo: SyncedRepoRecord) -> Result<SyncedRepoDto, AppError> {
    let rows = store.list_synced_repo_skills(&repo.slug).map_err(AppError::db)?;
    let picked_count = rows.iter().filter(|r| r.state == "picked" || r.state == "removed_upstream").count();
    let available_count = rows.iter().filter(|r| r.state == "available").count();
    Ok(SyncedRepoDto {
        slug: repo.slug,
        name: repo.name,
        url: repo.url,
        branch: repo.branch,
        resolved_branch: repo.resolved_branch,
        subpath: repo.subpath,
        head_revision: repo.head_revision,
        last_fetched_at: repo.last_fetched_at,
        last_error: repo.last_error,
        auto_update: repo.auto_update,
        state: repo.state,
        dev_mode: repo.dev_mode,
        dev_branch: repo.dev_branch,
        dev_base_revision: repo.dev_base_revision,
        pr_url: repo.pr_url,
        picked_count,
        available_count,
    })
}

fn emitter(app: &AppHandle) -> impl Fn(&str) + '_ {
    move |slug: &str| {
        if let Err(err) = app.emit(EVENT_UPDATED, slug.to_string()) {
            log::debug!("synced repos: emit failed: {err}");
        }
    }
}

fn outcome_dto(o: synced_repos::RefreshOutcome) -> SyncedRepoRefreshDto {
    SyncedRepoRefreshDto {
        slug: o.slug,
        status: match o.status {
            RefreshStatus::UpToDate => "up_to_date",
            RefreshStatus::Updated => "updated",
            RefreshStatus::Blocked => "blocked",
        }
        .to_string(),
        updated_skills: o.updated_skills,
        renamed_skills: o.renamed_skills,
        removed_upstream: o.removed_upstream,
        error: None,
    }
}

#[tauri::command]
pub async fn synced_repo_list(store: State<'_, Arc<SkillStore>>) -> Result<Vec<SyncedRepoDto>, AppError> {
    let store = store.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        store
            .list_synced_repos()
            .map_err(AppError::db)?
            .into_iter()
            .map(|r| repo_to_dto(&store, r))
            .collect()
    })
    .await?
}

#[tauri::command]
pub async fn synced_repo_add(
    url: String,
    branch: Option<String>,
    subpath: Option<String>,
    name: Option<String>,
    store: State<'_, Arc<SkillStore>>,
) -> Result<SyncedRepoDto, AppError> {
    let store = store.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let proxy = store.proxy_url();
        let repo = synced_repos::add(&store, &AddRequest { url, branch, subpath, name }, proxy.as_deref())
            .map_err(map_err)?;
        repo_to_dto(&store, repo)
    })
    .await?
}

#[tauri::command]
pub async fn synced_repo_update(
    slug: String,
    name: Option<String>,
    auto_update: Option<bool>,
    branch: Option<String>,
    subpath: Option<String>,
    store: State<'_, Arc<SkillStore>>,
) -> Result<SyncedRepoDto, AppError> {
    let store = store.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let proxy = store.proxy_url();
        let repo = synced_repos::update_repo(
            &store,
            &slug,
            &RepoUpdate { name, auto_update, branch, subpath },
            proxy.as_deref(),
        )
        .map_err(map_err)?;
        repo_to_dto(&store, repo)
    })
    .await?
}

#[tauri::command]
pub async fn synced_repo_remove(slug: String, store: State<'_, Arc<SkillStore>>) -> Result<(), AppError> {
    let store = store.inner().clone();
    tauri::async_runtime::spawn_blocking(move || synced_repos::remove_repo(&store, &slug).map_err(map_err)).await?
}

#[tauri::command]
pub async fn synced_repo_list_skills(
    slug: String,
    store: State<'_, Arc<SkillStore>>,
) -> Result<Vec<SyncedRepoSkillDto>, AppError> {
    let store = store.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        Ok(synced_repos::list_skills(&store, &slug)
            .map_err(map_err)?
            .into_iter()
            .map(|v| SyncedRepoSkillDto {
                rel_path: v.record.rel_path,
                name: v.record.name,
                description: v.record.description,
                state: v.record.state,
                skill_id: v.record.skill_id,
                clash: v.clash.map(|c| SyncedRepoClashDto {
                    kind: c.kind,
                    skill_id: c.skill_id,
                    skill_name: c.skill_name,
                }),
            })
            .collect())
    })
    .await?
}

#[tauri::command]
pub async fn synced_repo_pick_skills(
    slug: String,
    rel_paths: Vec<String>,
    replace_skill_ids: Option<Vec<String>>,
    store: State<'_, Arc<SkillStore>>,
) -> Result<Vec<String>, AppError> {
    let store = store.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        synced_repos::pick(&store, &slug, &rel_paths, &replace_skill_ids.unwrap_or_default()).map_err(map_err)
    })
    .await?
}

#[tauri::command]
pub async fn synced_repo_unpick_skills(
    skill_ids: Vec<String>,
    store: State<'_, Arc<SkillStore>>,
) -> Result<(), AppError> {
    let store = store.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        for id in &skill_ids {
            synced_repos::unpick(&store, id).map_err(map_err)?;
        }
        Ok(())
    })
    .await?
}

/// Refresh one repo (`slug` given, errors are returned) or all repos
/// (`slug` omitted, per-repo errors are reported in the result).
#[tauri::command]
pub async fn synced_repo_refresh(
    slug: Option<String>,
    app: AppHandle,
    store: State<'_, Arc<SkillStore>>,
) -> Result<Vec<SyncedRepoRefreshDto>, AppError> {
    let store = store.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let proxy = store.proxy_url();
        let emit = emitter(&app);
        if let Some(slug) = slug {
            let outcome = synced_repos::refresh(&store, &slug, proxy.as_deref(), false, Some(&emit))
                .map_err(map_err);
            // Failures change last_error/state; let other windows see them.
            if outcome.is_err() {
                emit(&slug);
            }
            return Ok(vec![outcome_dto(outcome?)]);
        }
        let mut results = Vec::new();
        for repo in store.list_synced_repos().map_err(AppError::db)? {
            match synced_repos::refresh(&store, &repo.slug, proxy.as_deref(), false, Some(&emit)) {
                Ok(o) => results.push(outcome_dto(o)),
                Err(e) => {
                    emit(&repo.slug);
                    results.push(SyncedRepoRefreshDto {
                        slug: repo.slug,
                        status: "error".to_string(),
                        updated_skills: 0,
                        renamed_skills: 0,
                        removed_upstream: 0,
                        error: Some(format!("{e:#}")),
                    });
                }
            }
        }
        Ok(results)
    })
    .await?
}

/// `action`: `"remove"` (unsubscribe, delete the orphaned copy) or `"keep"`
/// (turn it into a personal skill in the library).
#[tauri::command]
pub async fn synced_repo_resolve_removed(
    slug: String,
    rel_path: String,
    action: String,
    store: State<'_, Arc<SkillStore>>,
) -> Result<(), AppError> {
    let store = store.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let action = RemovedAction::parse(&action).map_err(map_err)?;
        synced_repos::resolve_removed(&store, &slug, &rel_path, action).map_err(map_err)
    })
    .await?
}

/// `action`: `"dev"` (move the edits into the dev worktree) or `"discard"`.
#[tauri::command]
pub async fn synced_repo_resolve_canonical_dirty(
    slug: String,
    action: String,
    store: State<'_, Arc<SkillStore>>,
) -> Result<(), AppError> {
    let store = store.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let action = DirtyAction::parse(&action).map_err(map_err)?;
        synced_repos::resolve_canonical_dirty(&store, &slug, action).map_err(map_err)
    })
    .await?
}

// ───────────────────────────── dev mode ─────────────────────────────

#[tauri::command]
pub async fn synced_repo_set_dev_mode(
    slug: String,
    enabled: bool,
    app: AppHandle,
    store: State<'_, Arc<SkillStore>>,
) -> Result<SyncedRepoDto, AppError> {
    let store = store.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let repo = dev::set_dev_mode(&store, &slug, enabled).map_err(map_err)?;
        emitter(&app)(&slug);
        repo_to_dto(&store, repo)
    })
    .await?
}

#[tauri::command]
pub async fn synced_repo_dev_status(
    slug: String,
    store: State<'_, Arc<SkillStore>>,
) -> Result<dev::DevStatus, AppError> {
    let store = store.inner().clone();
    tauri::async_runtime::spawn_blocking(move || dev::status(&store, &slug).map_err(map_err)).await?
}

#[tauri::command]
pub async fn synced_repo_dev_diff(
    slug: String,
    store: State<'_, Arc<SkillStore>>,
) -> Result<dev::DevDiff, AppError> {
    let store = store.inner().clone();
    tauri::async_runtime::spawn_blocking(move || dev::diff(&store, &slug).map_err(map_err)).await?
}

#[tauri::command]
pub async fn synced_repo_dev_rebase(
    slug: String,
    app: AppHandle,
    store: State<'_, Arc<SkillStore>>,
) -> Result<dev::RebaseResult, AppError> {
    let store = store.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let proxy = store.proxy_url();
        let result = dev::rebase(&store, &slug, proxy.as_deref()).map_err(map_err);
        emitter(&app)(&slug);
        result
    })
    .await?
}

#[tauri::command]
pub async fn synced_repo_dev_discard(
    slug: String,
    app: AppHandle,
    store: State<'_, Arc<SkillStore>>,
) -> Result<DevDiscardDto, AppError> {
    let store = store.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let patch = dev::discard(&store, &slug).map_err(map_err)?;
        emitter(&app)(&slug);
        Ok(DevDiscardDto { patch_path: patch.map(|p| p.to_string_lossy().to_string()) })
    })
    .await?
}

#[tauri::command]
pub async fn synced_repo_open_pr(
    slug: String,
    title: String,
    description: String,
    app: AppHandle,
    store: State<'_, Arc<SkillStore>>,
) -> Result<dev::PrResult, AppError> {
    let store = store.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let proxy = store.proxy_url();
        let result = dev::open_pr(&store, &slug, &title, &description, proxy.as_deref()).map_err(map_err);
        emitter(&app)(&slug);
        result
    })
    .await?
}

#[tauri::command]
pub async fn synced_repo_dev_finish(
    slug: String,
    force: Option<bool>,
    app: AppHandle,
    store: State<'_, Arc<SkillStore>>,
) -> Result<dev::FinishResult, AppError> {
    let store = store.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let proxy = store.proxy_url();
        let result = dev::finish(&store, &slug, force.unwrap_or(false), proxy.as_deref()).map_err(map_err);
        emitter(&app)(&slug);
        result
    })
    .await?
}

#[tauri::command]
pub async fn synced_repo_open_dev_folder(
    slug: String,
    store: State<'_, Arc<SkillStore>>,
) -> Result<(), AppError> {
    let store = store.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let path = dev::dev_folder(&store, &slug).map_err(map_err)?;
        let status = file_manager::open_dir(&path)
            .map_err(|e| AppError::io(format!("Failed to open folder: {e}")))?;
        if file_manager::exit_is_failure(status.success()) {
            return Err(AppError::io(format!("File manager exited with status: {status}")));
        }
        Ok(())
    })
    .await?
}
