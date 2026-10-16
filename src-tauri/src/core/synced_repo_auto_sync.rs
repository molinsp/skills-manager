//! Background timer that pulls synced repos. Modeled on `skill_auto_updater`:
//! 60 s after launch, then wakes every 15 minutes and refreshes repos whose
//! last fetch is older than the configured interval (default 1 h).

use std::sync::Arc;
use std::time::Duration;

use tauri::{AppHandle, Emitter, Runtime};

use super::skill_store::SkillStore;
use super::synced_repos;

pub const SETTING_INTERVAL: &str = "synced_repos_sync_interval";
pub const EVENT_UPDATED: &str = "synced-repos-updated";

const INITIAL_DELAY: Duration = Duration::from_secs(60);
const POLL_INTERVAL: Duration = Duration::from_secs(15 * 60);
const DEFAULT_INTERVAL: Duration = Duration::from_secs(60 * 60);

pub fn start<R: Runtime>(app: AppHandle<R>, store: Arc<SkillStore>) {
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(INITIAL_DELAY).await;
        loop {
            if let Some(interval) = read_interval(&store) {
                let store_for_task = store.clone();
                let app_for_task = app.clone();
                let result = tauri::async_runtime::spawn_blocking(move || {
                    run_round(&app_for_task, &store_for_task, interval)
                })
                .await;
                if let Err(err) = result {
                    log::warn!("synced repos auto-sync: round panicked: {err}");
                }
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    });
}

fn run_round<R: Runtime>(app: &AppHandle<R>, store: &SkillStore, interval: Duration) {
    let Ok(repos) = store.list_synced_repos() else { return };
    let proxy = store.proxy_url();
    let emit = |slug: &str| {
        if let Err(err) = app.emit(EVENT_UPDATED, slug.to_string()) {
            log::debug!("synced repos auto-sync: emit failed: {err}");
        }
    };
    let now = chrono::Utc::now().timestamp_millis();
    for repo in repos {
        if !repo.auto_update || !is_due(repo.last_fetched_at, now, interval) {
            continue;
        }
        if let Err(err) = synced_repos::refresh(store, &repo.slug, proxy.as_deref(), true, Some(&emit)) {
            log::warn!("synced repo {}: auto refresh failed: {err:#}", repo.slug);
            // Surface the new last_error / state to the UI.
            emit(&repo.slug);
        }
        // Let a waiting foreground operation win the lock between repos.
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Unset means the default (1 h); `off`/`manual` disable the timer.
fn read_interval(store: &SkillStore) -> Option<Duration> {
    match store.get_setting(SETTING_INTERVAL).ok().flatten() {
        None => Some(DEFAULT_INTERVAL),
        Some(raw) => parse_interval(raw.trim()),
    }
}

pub(crate) fn parse_interval(raw: &str) -> Option<Duration> {
    match raw.to_ascii_lowercase().as_str() {
        "" => Some(DEFAULT_INTERVAL),
        "off" | "manual" | "disabled" => None,
        "15m" => Some(Duration::from_secs(15 * 60)),
        "30m" => Some(Duration::from_secs(30 * 60)),
        "1h" | "hourly" => Some(Duration::from_secs(60 * 60)),
        "6h" => Some(Duration::from_secs(6 * 60 * 60)),
        "24h" | "1d" | "daily" => Some(Duration::from_secs(24 * 60 * 60)),
        _ => Some(DEFAULT_INTERVAL),
    }
}

fn is_due(last_fetched_ms: Option<i64>, now_ms: i64, interval: Duration) -> bool {
    match last_fetched_ms {
        None => true,
        Some(last) => now_ms.saturating_sub(last) >= interval.as_millis() as i64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_parsing_and_due() {
        assert_eq!(parse_interval(""), Some(DEFAULT_INTERVAL));
        assert_eq!(parse_interval("off"), None);
        assert_eq!(parse_interval("6h"), Some(Duration::from_secs(21600)));
        assert!(is_due(None, 1000, DEFAULT_INTERVAL));
        assert!(!is_due(Some(1000), 2000, DEFAULT_INTERVAL));
        assert!(is_due(Some(0), 3_600_000, DEFAULT_INTERVAL));
    }
}
