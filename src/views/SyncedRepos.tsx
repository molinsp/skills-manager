import { useCallback, useEffect, useMemo, useState, type ReactNode } from "react";
import {
  AlertTriangle,
  ChevronDown,
  ChevronRight,
  GitBranch,
  Loader2,
  Plus,
  RefreshCw,
  Trash2,
  X,
} from "lucide-react";
import { listen } from "@tauri-apps/api/event";
import { openUrl } from "@tauri-apps/plugin-opener";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import { cn } from "../utils";
import { ToggleSwitch } from "../components/ToggleSwitch";
import { ConfirmDialog } from "../components/ConfirmDialog";
import { SyncedRepoDevPanel } from "../components/SyncedRepoDevPanel";
import { SkillPickerRow } from "../components/SkillPickerRow";
import { useApp } from "../context/AppContext";
import { getErrorMessage } from "../lib/error";
import * as api from "../lib/tauri";
import type { SyncedRepo, SyncedRepoSkill } from "../lib/tauri";
import type { PickerStatus } from "../lib/skillPickerStatus";

const INPUT_CLASS =
  "w-full rounded-lg border border-border-subtle bg-background px-3 py-2 text-[13px] text-secondary placeholder-faint transition-all focus:border-border focus:outline-none";

function formatFetched(ms: number | null, never: string) {
  if (!ms) return never;
  return new Date(ms).toLocaleString();
}

export function SyncedRepos() {
  const { t } = useTranslation();
  const reportRefresh = (results: api.SyncedRepoRefreshResult[]) => reportRefreshResults(results, t);
  const { refreshManagedSkills } = useApp();
  const [repos, setRepos] = useState<SyncedRepo[]>([]);
  const [loading, setLoading] = useState(true);
  const [addOpen, setAddOpen] = useState(false);
  const [refreshingAll, setRefreshingAll] = useState(false);

  const load = useCallback(async () => {
    try {
      setRepos(await api.syncedRepoList());
    } catch (error: unknown) {
      toast.error(getErrorMessage(error, t("common.error")));
    } finally {
      setLoading(false);
    }
  }, [t]);

  useEffect(() => {
    void load();
    const unlistenPromise = listen("synced-repos-updated", () => {
      void load();
      void refreshManagedSkills();
    });
    return () => {
      void unlistenPromise.then((unlisten) => unlisten()).catch(() => {});
    };
  }, [load, refreshManagedSkills]);

  const handleRefreshAll = async () => {
    setRefreshingAll(true);
    try {
      reportRefresh(await api.syncedRepoRefresh());
      await load();
    } catch (error: unknown) {
      toast.error(getErrorMessage(error, t("common.error")));
    } finally {
      setRefreshingAll(false);
    }
  };

  return (
    <div className="app-page">
      <div className="app-page-header flex items-center justify-between gap-3 pb-4 pr-2">
        <div className="min-w-0">
          <h1 className="app-page-title">{t("syncedRepos.title")}</h1>
          <p className="app-page-subtitle">{t("syncedRepos.subtitle")}</p>
        </div>
        <div className="flex shrink-0 items-center gap-2">
          {repos.length > 0 && (
            <button
              onClick={handleRefreshAll}
              disabled={refreshingAll}
              className="app-button-secondary"
            >
              <RefreshCw className={cn("h-3.5 w-3.5", refreshingAll && "animate-spin")} />
              {t("syncedRepos.refreshAll")}
            </button>
          )}
          <button onClick={() => setAddOpen(true)} className="app-button-primary">
            <Plus className="h-3.5 w-3.5" />
            {t("syncedRepos.addRepo")}
          </button>
        </div>
      </div>

      {loading ? (
        <div className="flex items-center gap-2 text-[13px] text-muted">
          <Loader2 className="h-3.5 w-3.5 animate-spin" />
          {t("common.loading")}
        </div>
      ) : repos.length === 0 ? (
        <div className="rounded-xl border border-dashed border-border-subtle px-6 py-10 text-center">
          <div className="text-[14px] font-medium text-secondary">{t("syncedRepos.emptyTitle")}</div>
          <p className="mx-auto mt-1 max-w-[480px] text-[13px] text-muted">
            {t("syncedRepos.emptyDescription")}
          </p>
        </div>
      ) : (
        <div className="flex flex-col gap-4">
          {repos.map((repo) => (
            <RepoCard
              key={repo.slug}
              repo={repo}
              onChanged={async () => {
                await Promise.all([load(), refreshManagedSkills()]);
              }}
            />
          ))}
        </div>
      )}

      {addOpen && (
        <AddRepoDialog
          onClose={() => setAddOpen(false)}
          onAdded={async () => {
            setAddOpen(false);
            await Promise.all([load(), refreshManagedSkills()]);
          }}
        />
      )}
    </div>
  );
}

function AddRepoDialog({
  onClose,
  onAdded,
}: {
  onClose: () => void;
  onAdded: () => Promise<void>;
}) {
  const { t } = useTranslation();
  const [url, setUrl] = useState("");
  const [branch, setBranch] = useState("");
  const [subpath, setSubpath] = useState("");
  const [name, setName] = useState("");
  const [busy, setBusy] = useState(false);

  const submit = async () => {
    if (!url.trim() || busy) return;
    setBusy(true);
    try {
      await api.syncedRepoAdd({
        url: url.trim(),
        branch: branch.trim() || null,
        subpath: subpath.trim() || null,
        name: name.trim() || null,
      });
      toast.success(t("syncedRepos.added"));
      await onAdded();
    } catch (error: unknown) {
      toast.error(getErrorMessage(error, t("common.error")));
      setBusy(false);
    }
  };

  const field = (
    label: string,
    value: string,
    set: (v: string) => void,
    placeholder: string,
    hint?: string,
  ) => (
    <label className="block">
      <span className="mb-1 block text-[12px] font-medium text-muted">{label}</span>
      <input
        value={value}
        onChange={(e) => set(e.target.value)}
        placeholder={placeholder}
        disabled={busy}
        className={INPUT_CLASS}
      />
      {hint && <span className="mt-1 block text-[12px] text-faint">{hint}</span>}
    </label>
  );

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center">
      <div className="absolute inset-0 bg-black/70 backdrop-blur-sm" onClick={busy ? undefined : onClose} />
      <div className="relative w-full max-w-[480px] rounded-xl border border-border bg-surface p-5 shadow-2xl">
        <div className="mb-4 flex items-center justify-between">
          <h2 className="text-[14px] font-semibold text-primary">{t("syncedRepos.addTitle")}</h2>
          <button
            onClick={onClose}
            disabled={busy}
            className="rounded-md p-1 text-muted transition-colors hover:text-secondary"
          >
            <X className="h-4 w-4" />
          </button>
        </div>
        <div className="flex flex-col gap-3">
          {field(t("syncedRepos.url"), url, setUrl, "https://github.com/org/skills.git")}
          {field(t("syncedRepos.branch"), branch, setBranch, "main", t("syncedRepos.branchHint"))}
          {field(t("syncedRepos.subpath"), subpath, setSubpath, "skills", t("syncedRepos.subpathHint"))}
          {field(t("syncedRepos.name"), name, setName, "", t("syncedRepos.nameHint"))}
        </div>
        <div className="mt-5 flex justify-end gap-2">
          <button onClick={onClose} disabled={busy} className="app-button-secondary">
            {t("common.cancel")}
          </button>
          <button onClick={submit} disabled={busy || !url.trim()} className="app-button-primary">
            {busy && <Loader2 className="h-3.5 w-3.5 animate-spin" />}
            {busy ? t("syncedRepos.adding") : t("syncedRepos.addRepo")}
          </button>
        </div>
      </div>
    </div>
  );
}

function reportRefreshResults(
  results: api.SyncedRepoRefreshResult[],
  t: (key: string, opts?: Record<string, unknown>) => string,
) {
  for (const r of results) {
    if (r.status === "error") {
      toast.error(t("syncedRepos.refreshError", { slug: r.slug, error: r.error ?? "" }));
    } else if (r.status === "blocked") {
      toast.warning(t("syncedRepos.refreshBlocked", { slug: r.slug }));
    }
  }
}

function RepoCard({ repo, onChanged }: { repo: SyncedRepo; onChanged: () => Promise<void> }) {
  const { t } = useTranslation();
  const [refreshing, setRefreshing] = useState(false);
  const [expanded, setExpanded] = useState(false);
  const [confirmRemove, setConfirmRemove] = useState(false);
  const [confirmDiscard, setConfirmDiscard] = useState(false);
  const [togglingAuto, setTogglingAuto] = useState(false);
  const [togglingDev, setTogglingDev] = useState(false);
  const reportRefresh = (results: api.SyncedRepoRefreshResult[]) => reportRefreshResults(results, t);

  const run = async (fn: () => Promise<unknown>, successKey?: string) => {
    try {
      await fn();
      if (successKey) toast.success(t(successKey));
    } catch (error: unknown) {
      toast.error(getErrorMessage(error, t("common.error")));
    }
  };

  const handleRefresh = async () => {
    setRefreshing(true);
    await run(async () => reportRefresh(await api.syncedRepoRefresh(repo.slug)));
    await onChanged();
    setRefreshing(false);
  };

  const handleAuto = async (next: boolean) => {
    setTogglingAuto(true);
    await run(() => api.syncedRepoUpdate({ slug: repo.slug, autoUpdate: next }));
    await onChanged();
    setTogglingAuto(false);
  };

  const handleDev = async (next: boolean) => {
    setTogglingDev(true);
    await run(
      () => api.syncedRepoSetDevMode(repo.slug, next),
      next ? "syncedRepos.dev.enabled" : "syncedRepos.dev.disabled",
    );
    await onChanged();
    setTogglingDev(false);
  };

  const branchLabel = repo.branch ?? repo.resolvedBranch;

  const stateMessage: { text: string; tone: "warn" | "error" } | null =
    repo.state === "canonical_dirty"
      ? { text: t("syncedRepos.state.canonicalDirty"), tone: "warn" }
      : repo.state === "missing_clone"
        ? { text: t("syncedRepos.state.missingClone"), tone: "error" }
        : repo.state === "auth_required"
          ? { text: t("syncedRepos.state.authRequired"), tone: "error" }
          : repo.lastError
            ? { text: t("syncedRepos.state.fetchFailed", { error: repo.lastError }), tone: "error" }
            : null;

  return (
    <div className="rounded-xl border border-border-subtle bg-surface">
      <div className="flex flex-wrap items-start justify-between gap-3 px-4 py-3.5">
        <div className="min-w-0 flex-1">
          <div className="flex min-w-0 items-center gap-2">
            <span className="truncate text-[14px] font-semibold text-primary">{repo.name}</span>
            {repo.devMode && (
              <span className="shrink-0 rounded-full bg-amber-500/12 px-2 py-0.5 text-[11px] font-medium text-amber-600 dark:text-amber-400">
                {t("syncedRepos.devMode")}
              </span>
            )}
          </div>
          <div className="mt-1 truncate font-mono text-[12px] text-muted" title={repo.url}>
            {repo.url}
          </div>
          <div className="mt-1 flex flex-wrap items-center gap-x-3 gap-y-1 text-[12px] text-muted">
            {branchLabel && (
              <span className="inline-flex items-center gap-1">
                <GitBranch className="h-3 w-3" />
                {branchLabel}
              </span>
            )}
            {repo.subpath && <span className="font-mono">{repo.subpath}</span>}
            <span>
              {t("syncedRepos.lastFetched", {
                time: formatFetched(repo.lastFetchedAt, t("syncedRepos.never")),
              })}
            </span>
            <span>
              {t("syncedRepos.counts", { picked: repo.pickedCount, available: repo.availableCount })}
            </span>
          </div>
        </div>
        <div className="flex shrink-0 items-center gap-3">
          <label className="flex items-center gap-2 text-[12px] text-muted">
            {t("syncedRepos.autoUpdate")}
            <ToggleSwitch
              checked={repo.autoUpdate}
              loading={togglingAuto}
              onChange={() => void handleAuto(!repo.autoUpdate)}
              title={t("syncedRepos.autoUpdateHint")}
            />
          </label>
          <label className="flex items-center gap-2 text-[12px] text-muted">
            {t("syncedRepos.devMode")}
            <ToggleSwitch
              checked={repo.devMode}
              loading={togglingDev}
              onChange={() => void handleDev(!repo.devMode)}
              title={t("syncedRepos.devModeHint")}
            />
          </label>
          <button onClick={handleRefresh} disabled={refreshing} className="app-button-secondary !px-3 !py-1.5">
            <RefreshCw className={cn("h-3.5 w-3.5", refreshing && "animate-spin")} />
            {t("syncedRepos.refresh")}
          </button>
          <button
            onClick={() => setConfirmRemove(true)}
            title={t("syncedRepos.remove")}
            className="rounded-md p-1.5 text-muted transition-colors hover:bg-surface-hover hover:text-red-500"
          >
            <Trash2 className="h-4 w-4" />
          </button>
        </div>
      </div>

      {stateMessage && (
        <div
          className={cn(
            "mx-4 mb-3 flex flex-wrap items-center gap-2 rounded-lg px-3 py-2 text-[12px]",
            stateMessage.tone === "warn"
              ? "bg-amber-500/10 text-amber-600 dark:text-amber-400"
              : "bg-red-500/10 text-red-600 dark:text-red-300",
          )}
        >
          <AlertTriangle className="h-3.5 w-3.5 shrink-0" />
          <span className="min-w-0 flex-1">{stateMessage.text}</span>
          {repo.state === "canonical_dirty" && (
            <>
              <button
                onClick={async () => {
                  await run(
                    () => api.syncedRepoResolveCanonicalDirty(repo.slug, "dev"),
                    "syncedRepos.resolved",
                  );
                  await onChanged();
                }}
                className="rounded-full border border-current px-2 py-0.5 font-medium transition-colors hover:bg-surface-hover"
              >
                {t("syncedRepos.moveToDev")}
              </button>
              <button
                onClick={() => setConfirmDiscard(true)}
                className="rounded-full border border-current px-2 py-0.5 font-medium transition-colors hover:bg-surface-hover"
              >
                {t("syncedRepos.discardChanges")}
              </button>
            </>
          )}
        </div>
      )}

      {repo.prUrl && (
        <div className="mx-4 mb-3 truncate text-[12px] text-muted">
          {t("syncedRepos.pullRequest")}:{" "}
          <button
            onClick={() => void openUrl(repo.prUrl as string).catch(() => {})}
            className="font-mono text-accent hover:underline"
          >
            {repo.prUrl}
          </button>
        </div>
      )}

      {repo.devMode && <SyncedRepoDevPanel repo={repo} onChanged={onChanged} />}

      <button
        onClick={() => setExpanded((v) => !v)}
        aria-expanded={expanded}
        className="flex w-full items-center gap-1.5 border-t border-border-faint px-4 py-2.5 text-left text-[12px] font-medium text-secondary transition-colors hover:bg-surface-hover"
      >
        {expanded ? <ChevronDown className="h-3.5 w-3.5" /> : <ChevronRight className="h-3.5 w-3.5" />}
        {t("syncedRepos.skills")}
      </button>
      {expanded && <RepoSkills repo={repo} onChanged={onChanged} />}

      <ConfirmDialog
        open={confirmRemove}
        title={t("syncedRepos.remove")}
        message={t("syncedRepos.removeConfirm", { name: repo.name })}
        onClose={() => setConfirmRemove(false)}
        onConfirm={async () => {
          await run(() => api.syncedRepoRemove(repo.slug), "syncedRepos.removed");
          await onChanged();
        }}
      />
      <ConfirmDialog
        open={confirmDiscard}
        title={t("syncedRepos.discardChanges")}
        message={t("syncedRepos.discardConfirm")}
        onClose={() => setConfirmDiscard(false)}
        onConfirm={async () => {
          await run(
            () => api.syncedRepoResolveCanonicalDirty(repo.slug, "discard"),
            "syncedRepos.resolved",
          );
          await onChanged();
        }}
      />
    </div>
  );
}

function RepoSkills({ repo, onChanged }: { repo: SyncedRepo; onChanged: () => Promise<void> }) {
  const { t } = useTranslation();
  const [skills, setSkills] = useState<SyncedRepoSkill[] | null>(null);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [busyPath, setBusyPath] = useState<string | null>(null);
  const [picking, setPicking] = useState(false);
  const [clashPrompt, setClashPrompt] = useState<SyncedRepoSkill[] | null>(null);

  const load = useCallback(async () => {
    try {
      setSkills(await api.syncedRepoListSkills(repo.slug));
    } catch (error: unknown) {
      setSkills([]);
      toast.error(getErrorMessage(error, t("common.error")));
    }
  }, [repo.slug, t]);

  // Reload whenever the repo changes underneath us (refresh, pick, resolve).
  useEffect(() => {
    void load();
  }, [load, repo.headRevision, repo.pickedCount, repo.availableCount, repo.lastFetchedAt]);

  const afterChange = async () => {
    await Promise.all([load(), onChanged()]);
  };

  const toggle = (relPath: string) =>
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(relPath)) next.delete(relPath);
      else next.add(relPath);
      return next;
    });

  const doPick = async (relPaths: string[], replaceSkillIds: string[]) => {
    setPicking(true);
    try {
      await api.syncedRepoPickSkills(repo.slug, relPaths, replaceSkillIds);
      toast.success(t("syncedRepos.picked", { count: relPaths.length }));
      setSelected(new Set());
      await afterChange();
    } catch (error: unknown) {
      toast.error(getErrorMessage(error, t("common.error")));
    } finally {
      setPicking(false);
      setClashPrompt(null);
    }
  };

  const handlePick = () => {
    const chosen = (skills ?? []).filter((s) => selected.has(s.relPath));
    if (chosen.length === 0) return;
    const clashes = chosen.filter((s) => s.clash?.kind === "personal");
    if (clashes.length > 0) {
      setClashPrompt(clashes);
      return;
    }
    void doPick(chosen.map((s) => s.relPath), []);
  };

  const rowAction = async (relPath: string, fn: () => Promise<unknown>) => {
    setBusyPath(relPath);
    try {
      await fn();
      await afterChange();
    } catch (error: unknown) {
      toast.error(getErrorMessage(error, t("common.error")));
    } finally {
      setBusyPath(null);
    }
  };

  const smallBtn =
    "shrink-0 rounded-full border border-border-subtle px-2 py-0.5 text-[12px] font-medium text-secondary transition-colors hover:bg-surface-hover disabled:opacity-50";

  const allTags = useMemo(() => [] as string[], []);

  if (skills === null) {
    return (
      <div className="flex items-center gap-2 border-t border-border-faint px-4 py-4 text-[13px] text-muted">
        <Loader2 className="h-3.5 w-3.5 animate-spin" />
        {t("common.loading")}
      </div>
    );
  }

  return (
    <div className="border-t border-border-faint">
      {skills.length === 0 ? (
        <div className="px-4 py-4 text-[13px] text-muted">{t("syncedRepos.noSkills")}</div>
      ) : (
        <div className="divide-y divide-border-faint">
          {skills.map((skill) => {
            const personalClash = skill.clash?.kind === "personal";
            const syncedClash = skill.clash?.kind === "synced";
            let status: PickerStatus = "available";
            let label: string | undefined;
            let tooltip: string | undefined;
            let trailing: ReactNode = null;

            if (skill.state === "picked") {
              status = "installed";
              label = t("syncedRepos.chip.picked");
              trailing = skill.skillId ? (
                <button
                  className={smallBtn}
                  disabled={busyPath === skill.relPath}
                  onClick={() =>
                    void rowAction(skill.relPath, () =>
                      api.syncedRepoUnpickSkills([skill.skillId as string]),
                    )
                  }
                >
                  {t("syncedRepos.unsubscribe")}
                </button>
              ) : null;
            } else if (skill.state === "removed_upstream") {
              status = "unavailable";
              label = t("syncedRepos.chip.removedUpstream");
              tooltip = t("syncedRepos.removedUpstreamHint");
              trailing = (
                <div className="flex shrink-0 items-center gap-1">
                  <button
                    className={smallBtn}
                    disabled={busyPath === skill.relPath}
                    onClick={() =>
                      void rowAction(skill.relPath, () =>
                        api.syncedRepoResolveRemoved(repo.slug, skill.relPath, "remove"),
                      )
                    }
                  >
                    {t("syncedRepos.removeSkill")}
                  </button>
                  <button
                    className={smallBtn}
                    disabled={busyPath === skill.relPath}
                    onClick={() =>
                      void rowAction(skill.relPath, () =>
                        api.syncedRepoResolveRemoved(repo.slug, skill.relPath, "keep"),
                      )
                    }
                  >
                    {t("syncedRepos.keepSkill")}
                  </button>
                </div>
              );
            } else if (syncedClash) {
              status = "conflict";
              label = t("syncedRepos.chip.clash");
              tooltip = t("syncedRepos.clashSyncedHint", { name: skill.clash?.skillName ?? "" });
            } else if (personalClash) {
              label = undefined;
              tooltip = t("syncedRepos.clashPersonalHint", { name: skill.clash?.skillName ?? "" });
              trailing = (
                <span className="shrink-0 rounded-full bg-rose-500/10 px-2 py-0.5 text-[11px] font-medium text-rose-600 dark:text-rose-400">
                  {t("syncedRepos.chip.clash")}
                </span>
              );
            } else {
              trailing = (
                <span className="shrink-0 rounded-full bg-accent-bg px-2 py-0.5 text-[11px] font-medium text-accent">
                  {t("syncedRepos.chip.available")}
                </span>
              );
            }

            return (
              <SkillPickerRow
                key={skill.relPath}
                skill={{ name: skill.name, description: skill.description, tags: [] }}
                status={status}
                allTags={allTags}
                sourceLabel={skill.relPath}
                selected={selected.has(skill.relPath)}
                onToggle={() => toggle(skill.relPath)}
                busy={busyPath === skill.relPath}
                statusLabel={label}
                tooltip={tooltip}
                trailing={trailing}
              />
            );
          })}
        </div>
      )}
      {selected.size > 0 && (
        <div className="flex items-center justify-between gap-3 border-t border-border-faint px-4 py-2.5">
          <span className="text-[12px] text-muted">
            {t("syncedRepos.selectedCount", { count: selected.size })}
          </span>
          <button onClick={handlePick} disabled={picking} className="app-button-primary !px-3 !py-1.5">
            {picking && <Loader2 className="h-3.5 w-3.5 animate-spin" />}
            {t("syncedRepos.subscribe")}
          </button>
        </div>
      )}

      {clashPrompt && (
        <div className="fixed inset-0 z-50 flex items-center justify-center">
          <div className="absolute inset-0 bg-black/70 backdrop-blur-sm" onClick={() => setClashPrompt(null)} />
          <div className="relative w-full max-w-[480px] rounded-xl border border-border bg-surface p-5 shadow-2xl">
            <h2 className="text-[14px] font-semibold text-primary">{t("syncedRepos.clashTitle")}</h2>
            <p className="mt-2 text-[13px] text-muted">{t("syncedRepos.clashMessage")}</p>
            <ul className="mt-3 flex flex-col gap-1 text-[13px] text-secondary">
              {clashPrompt.map((s) => (
                <li key={s.relPath} className="truncate">
                  {s.name} → {s.clash?.skillName}
                </li>
              ))}
            </ul>
            <div className="mt-5 flex justify-end gap-2">
              <button onClick={() => setClashPrompt(null)} className="app-button-secondary">
                {t("common.cancel")}
              </button>
              <button
                disabled={picking}
                onClick={() => {
                  const chosen = (skills ?? []).filter((s) => selected.has(s.relPath));
                  void doPick(
                    chosen.map((s) => s.relPath),
                    clashPrompt.map((s) => s.clash?.skillId as string),
                  );
                }}
                className="app-button-primary"
              >
                {picking && <Loader2 className="h-3.5 w-3.5 animate-spin" />}
                {t("syncedRepos.replacePersonal")}
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
