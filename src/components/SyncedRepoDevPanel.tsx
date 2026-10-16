import { useCallback, useEffect, useState } from "react";
import { AlertTriangle, ChevronDown, ChevronRight, FolderOpen, GitPullRequest, Loader2, X } from "lucide-react";
import { openUrl } from "@tauri-apps/plugin-opener";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import { cn } from "../utils";
import { ConfirmDialog } from "./ConfirmDialog";
import { SkillSourceDiffViewer } from "./SkillSourceDiffViewer";
import { getErrorMessage } from "../lib/error";
import * as api from "../lib/tauri";
import type { SyncedRepo, SyncedRepoDevDiff, SyncedRepoDevStatus } from "../lib/tauri";

const INPUT_CLASS =
  "w-full rounded-lg border border-border-subtle bg-background px-3 py-2 text-[13px] text-secondary placeholder-faint transition-all focus:border-border focus:outline-none";

interface Props {
  repo: SyncedRepo;
  onChanged: () => Promise<void>;
}

export function SyncedRepoDevPanel({ repo, onChanged }: Props) {
  const { t } = useTranslation();
  const [status, setStatus] = useState<SyncedRepoDevStatus | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [conflicts, setConflicts] = useState<string[] | null>(null);
  const [diffOpen, setDiffOpen] = useState(false);
  const [diff, setDiff] = useState<SyncedRepoDevDiff | null>(null);
  const [prOpen, setPrOpen] = useState(false);
  const [confirmDiscard, setConfirmDiscard] = useState(false);
  const [finishUnpushed, setFinishUnpushed] = useState<number | null>(null);

  const loadStatus = useCallback(async () => {
    try {
      setStatus(await api.syncedRepoDevStatus(repo.slug));
    } catch (error: unknown) {
      toast.error(getErrorMessage(error, t("common.error")));
    }
  }, [repo.slug, t]);

  const loadDiff = useCallback(async () => {
    try {
      setDiff(await api.syncedRepoDevDiff(repo.slug));
    } catch (error: unknown) {
      setDiff({ skills: [] });
      toast.error(getErrorMessage(error, t("common.error")));
    }
  }, [repo.slug, t]);

  const reload = useCallback(async () => {
    await loadStatus();
    if (diffOpen) await loadDiff();
  }, [loadStatus, loadDiff, diffOpen]);

  useEffect(() => {
    void reload();
  }, [reload, repo.headRevision, repo.lastFetchedAt, repo.prUrl]);

  useEffect(() => {
    const onFocus = () => void reload();
    window.addEventListener("focus", onFocus);
    return () => window.removeEventListener("focus", onFocus);
  }, [reload]);

  const act = async (key: string, fn: () => Promise<void>) => {
    setBusy(key);
    try {
      await fn();
    } catch (error: unknown) {
      toast.error(getErrorMessage(error, t("common.error")));
    } finally {
      setBusy(null);
    }
  };

  const handleRebase = () =>
    act("rebase", async () => {
      const result = await api.syncedRepoDevRebase(repo.slug);
      if (result.status === "conflict") {
        setConflicts(result.conflictedFiles);
      } else {
        setConflicts(null);
        toast.success(t("syncedRepos.dev.rebased"));
      }
      await Promise.all([reload(), onChanged()]);
    });

  const handleOpenFolder = () =>
    act("folder", () => api.syncedRepoOpenDevFolder(repo.slug));

  const handleFinish = (force: boolean) =>
    act("finish", async () => {
      const result = await api.syncedRepoDevFinish(repo.slug, force);
      if (result.status === "needs_confirmation") {
        setFinishUnpushed(result.unpushedCommits);
        return;
      }
      setFinishUnpushed(null);
      toast.success(t("syncedRepos.dev.finished"));
      await onChanged();
    });

  const smallBtn =
    "inline-flex shrink-0 items-center gap-1 rounded-full border border-current px-2 py-0.5 text-[12px] font-medium transition-colors hover:bg-surface-hover disabled:opacity-50";

  if (!status) {
    return (
      <div className="flex items-center gap-2 border-t border-border-faint px-4 py-3 text-[13px] text-muted">
        <Loader2 className="h-3.5 w-3.5 animate-spin" />
        {t("common.loading")}
      </div>
    );
  }

  const changedCount = status.changedSkills.length;

  return (
    <div className="flex flex-col gap-3 border-t border-border-faint px-4 py-3">
      <div className="rounded-lg bg-accent-bg px-3 py-2 text-[12px] text-accent">
        {t("syncedRepos.dev.restartHint")}
      </div>

      {status.merged && (
        <div className="flex flex-wrap items-center gap-2 rounded-lg bg-emerald-500/10 px-3 py-2 text-[12px] text-emerald-600 dark:text-emerald-400">
          <span className="min-w-0 flex-1">{t("syncedRepos.dev.mergedBanner")}</span>
          <button onClick={() => void handleFinish(false)} disabled={busy !== null} className={smallBtn}>
            {busy === "finish" && <Loader2 className="h-3 w-3 animate-spin" />}
            {t("syncedRepos.dev.finish")}
          </button>
        </div>
      )}

      {status.repoAheadBy > 0 && (
        <div className="flex flex-wrap items-center gap-2 rounded-lg bg-amber-500/10 px-3 py-2 text-[12px] text-amber-600 dark:text-amber-400">
          <AlertTriangle className="h-3.5 w-3.5 shrink-0" />
          <span className="min-w-0 flex-1">
            {t("syncedRepos.dev.repoAhead", { count: status.repoAheadBy })}
          </span>
          <button onClick={() => void handleRebase()} disabled={busy !== null} className={smallBtn}>
            {busy === "rebase" && <Loader2 className="h-3 w-3 animate-spin" />}
            {t("syncedRepos.dev.rebase")}
          </button>
        </div>
      )}

      {conflicts && (
        <div className="rounded-lg bg-red-500/10 px-3 py-2 text-[12px] text-red-600 dark:text-red-300">
          <div className="flex flex-wrap items-center gap-2">
            <AlertTriangle className="h-3.5 w-3.5 shrink-0" />
            <span className="min-w-0 flex-1 font-medium">{t("syncedRepos.dev.conflictTitle")}</span>
            <button onClick={() => void handleOpenFolder()} disabled={busy !== null} className={smallBtn}>
              {t("syncedRepos.dev.openFolder")}
            </button>
            <button onClick={() => setConfirmDiscard(true)} disabled={busy !== null} className={smallBtn}>
              {t("syncedRepos.dev.resetToRepo")}
            </button>
          </div>
          <ul className="mt-1.5 flex flex-col gap-0.5 font-mono">
            {conflicts.map((f) => (
              <li key={f} className="truncate">{f}</li>
            ))}
          </ul>
        </div>
      )}

      <div className="flex flex-wrap items-center gap-x-3 gap-y-1 text-[12px] text-muted">
        {status.branch && <span className="font-mono">{status.branch}</span>}
        <span>{t("syncedRepos.dev.changedCount", { count: changedCount })}</span>
        {status.aheadOfRepo > 0 && (
          <span>{t("syncedRepos.dev.aheadOfRepo", { count: status.aheadOfRepo })}</span>
        )}
        {status.unpushedCommits > 0 && (
          <span>{t("syncedRepos.dev.unpushed", { count: status.unpushedCommits })}</span>
        )}
      </div>

      {changedCount > 0 && (
        <div className="flex flex-col gap-1">
          <button
            onClick={() => {
              const next = !diffOpen;
              setDiffOpen(next);
              if (next) void loadDiff();
            }}
            aria-expanded={diffOpen}
            className="flex items-center gap-1.5 text-left text-[12px] font-medium text-secondary"
          >
            {diffOpen ? <ChevronDown className="h-3.5 w-3.5" /> : <ChevronRight className="h-3.5 w-3.5" />}
            {t("syncedRepos.dev.viewChanges")}
          </button>
          <ul className="flex flex-col gap-0.5 text-[12px] text-muted">
            {status.changedSkills.map((s) => (
              <li key={s.relPath} className="flex items-center gap-2">
                <span className="truncate text-secondary">{s.name}</span>
                <span className="shrink-0">{t("syncedRepos.dev.filesChanged", { count: s.filesChanged })}</span>
              </li>
            ))}
          </ul>
          {diffOpen &&
            (diff === null ? (
              <div className="flex items-center gap-2 py-2 text-[13px] text-muted">
                <Loader2 className="h-3.5 w-3.5 animate-spin" />
                {t("common.loading")}
              </div>
            ) : (
              <div className="mt-2 flex flex-col gap-4">
                {diff.skills.map((s) => (
                  <div key={s.relPath}>
                    <div className="mb-1.5 text-[13px] font-medium text-secondary">{s.name}</div>
                    <SkillSourceDiffViewer entries={s.entries} />
                  </div>
                ))}
              </div>
            ))}
        </div>
      )}

      {status.outsideSubfolderFiles.length > 0 && (
        <div className="rounded-lg bg-amber-500/10 px-3 py-2 text-[12px] text-amber-600 dark:text-amber-400">
          {t("syncedRepos.dev.outsideWarning", { count: status.outsideSubfolderFiles.length })}
        </div>
      )}

      <div className="flex flex-wrap items-center gap-2">
        <button
          onClick={() => setPrOpen(true)}
          disabled={busy !== null || (changedCount === 0 && status.unpushedCommits === 0 && status.aheadOfRepo === 0)}
          className="app-button-primary !px-3 !py-1.5"
        >
          <GitPullRequest className="h-3.5 w-3.5" />
          {t("syncedRepos.dev.openPr")}
        </button>
        <button
          onClick={() => void handleOpenFolder()}
          disabled={busy !== null}
          className="app-button-secondary !px-3 !py-1.5"
        >
          <FolderOpen className="h-3.5 w-3.5" />
          {t("syncedRepos.dev.openFolder")}
        </button>
        <button
          onClick={() => void handleRebase()}
          disabled={busy !== null}
          className="app-button-secondary !px-3 !py-1.5"
        >
          {busy === "rebase" && <Loader2 className="h-3.5 w-3.5 animate-spin" />}
          {t("syncedRepos.dev.rebase")}
        </button>
        <button
          onClick={() => setConfirmDiscard(true)}
          disabled={busy !== null}
          className="app-button-secondary !px-3 !py-1.5"
        >
          {t("syncedRepos.dev.discard")}
        </button>
      </div>

      {prOpen && (
        <PrDialog
          repo={repo}
          outsideFiles={status.outsideSubfolderFiles}
          onClose={() => setPrOpen(false)}
          onDone={async () => {
            setPrOpen(false);
            await Promise.all([reload(), onChanged()]);
          }}
        />
      )}

      <ConfirmDialog
        open={confirmDiscard}
        title={t("syncedRepos.dev.discard")}
        message={t("syncedRepos.dev.discardConfirm")}
        onClose={() => setConfirmDiscard(false)}
        onConfirm={async () => {
          try {
            await api.syncedRepoDevDiscard(repo.slug);
            setConflicts(null);
            toast.success(t("syncedRepos.dev.discarded"));
            await Promise.all([reload(), onChanged()]);
          } catch (error: unknown) {
            toast.error(getErrorMessage(error, t("common.error")));
          }
        }}
      />
      <ConfirmDialog
        open={finishUnpushed !== null}
        title={t("syncedRepos.dev.finish")}
        message={t("syncedRepos.dev.finishConfirm", { count: finishUnpushed ?? 0 })}
        tone="warning"
        onClose={() => setFinishUnpushed(null)}
        onConfirm={async () => {
          await handleFinish(true);
        }}
      />
    </div>
  );
}

function PrDialog({
  repo,
  outsideFiles,
  onClose,
  onDone,
}: {
  repo: SyncedRepo;
  outsideFiles: string[];
  onClose: () => void;
  onDone: () => Promise<void>;
}) {
  const { t } = useTranslation();
  const [title, setTitle] = useState("");
  const [description, setDescription] = useState("");
  const [busy, setBusy] = useState(false);

  const submit = async () => {
    if (!title.trim() || busy) return;
    setBusy(true);
    try {
      const result = await api.syncedRepoOpenPr(repo.slug, title.trim(), description.trim());
      await openUrl(result.url).catch(() => {});
      toast.success(t(`syncedRepos.dev.prKind.${result.kind}`));
      await onDone();
    } catch (error: unknown) {
      toast.error(getErrorMessage(error, t("common.error")));
      setBusy(false);
    }
  };

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center">
      <div className="absolute inset-0 bg-black/70 backdrop-blur-sm" onClick={busy ? undefined : onClose} />
      <div className="relative w-full max-w-[520px] rounded-xl border border-border bg-surface p-5 shadow-2xl">
        <div className="mb-4 flex items-center justify-between">
          <h2 className="text-[14px] font-semibold text-primary">{t("syncedRepos.dev.prTitle")}</h2>
          <button
            onClick={onClose}
            disabled={busy}
            className="rounded-md p-1 text-muted transition-colors hover:text-secondary"
          >
            <X className="h-4 w-4" />
          </button>
        </div>
        <div className="flex flex-col gap-3">
          {outsideFiles.length > 0 && (
            <div className={cn("rounded-lg bg-amber-500/10 px-3 py-2 text-[12px] text-amber-600 dark:text-amber-400")}>
              <div>{t("syncedRepos.dev.outsideWarning", { count: outsideFiles.length })}</div>
              <ul className="mt-1 font-mono">
                {outsideFiles.slice(0, 8).map((f) => (
                  <li key={f} className="truncate">{f}</li>
                ))}
              </ul>
            </div>
          )}
          <label className="block">
            <span className="mb-1 block text-[12px] font-medium text-muted">{t("syncedRepos.dev.prTitleField")}</span>
            <input
              value={title}
              onChange={(e) => setTitle(e.target.value)}
              disabled={busy}
              className={INPUT_CLASS}
              autoFocus
            />
          </label>
          <label className="block">
            <span className="mb-1 block text-[12px] font-medium text-muted">{t("syncedRepos.dev.prDescription")}</span>
            <textarea
              value={description}
              onChange={(e) => setDescription(e.target.value)}
              disabled={busy}
              rows={5}
              className={INPUT_CLASS}
            />
          </label>
        </div>
        <div className="mt-5 flex justify-end gap-2">
          <button onClick={onClose} disabled={busy} className="app-button-secondary">
            {t("common.cancel")}
          </button>
          <button onClick={submit} disabled={busy || !title.trim()} className="app-button-primary">
            {busy && <Loader2 className="h-3.5 w-3.5 animate-spin" />}
            {t("syncedRepos.dev.openPr")}
          </button>
        </div>
      </div>
    </div>
  );
}
