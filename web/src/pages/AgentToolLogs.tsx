import { useState, useEffect, useCallback } from "react";
import { useTranslation } from "react-i18next";
import { useAgentContext } from "../hooks/useAgentContext";

interface ToolLog {
  id: number;
  agent_id: string;
  session_id: string | null;
  tool_name: string;
  args_json: string;
  outcome: string;
  result_text: string;
  started_at: string | null;
  created_at: string;
  latency_ms: number | null;
  iteration: number | null;
}

async function fetchToolLogs(agentId: string, limit = 20): Promise<ToolLog[]> {
  const res = await fetch(`/api/agents/${agentId}/tool-logs?limit=${limit}`);
  if (!res.ok) throw new Error("Failed to fetch tool logs");
  return res.json();
}

function truncate(s: string, max: number): string {
  if (s.length <= max) return s;
  return s.slice(0, max) + "…";
}

function outcomeClass(outcome: string): string {
  if (outcome === "done") return "badge-success";
  if (outcome === "failed") return "badge-warning";
  if (outcome === "refused") return "badge-neutral";
  return "badge-info";
}

function LogCard({ log }: { log: ToolLog }) {
  const [expanded, setExpanded] = useState(false);
  return (
    <div className="card-elevated">
      <button onClick={() => setExpanded(!expanded)} className="w-full text-left">
        <div className="flex items-center justify-between flex-wrap gap-2">
          <div className="flex items-center gap-2 flex-wrap min-w-0">
            <span className="inline-flex items-center gap-1 px-2 py-0.5 rounded-full bg-primary-container text-on-primary-container text-label-sm font-medium">
              <span className="material-symbols-outlined text-sm">build</span>
              {log.tool_name}
            </span>
            <span className={outcomeClass(log.outcome)}>{log.outcome}</span>
            {log.session_id && (
              <span className="inline-flex items-center gap-1 px-2 py-0.5 rounded-full bg-surface-container-high text-on-surface-variant text-label-sm">
                <span className="material-symbols-outlined text-sm">forum</span>
                {log.session_id.slice(0, 8)}…
              </span>
            )}
          </div>
          <div className="flex items-center gap-3">
            {log.latency_ms != null && (
              <span className="text-label-sm text-on-surface-variant">
                ⚡ {log.latency_ms.toLocaleString()}ms
              </span>
            )}
            <span className="text-label-sm text-on-surface-variant">
              {new Date(log.started_at ?? log.created_at).toLocaleString()}
            </span>
            <span className="material-symbols-outlined text-base text-on-surface-variant">
              {expanded ? "expand_less" : "expand_more"}
            </span>
          </div>
        </div>
        {!expanded && log.result_text && (
          <p className="mt-1 text-body-sm text-on-surface-variant truncate">
            {truncate(log.result_text.replace(/\n/g, " "), 80)}
          </p>
        )}
      </button>
      {expanded && (
        <div className="space-y-3 pt-3 border-t border-outline-variant">
          <div>
            <span className="text-label-sm font-medium text-on-surface-variant">args</span>
            <pre className="mt-1 p-3 text-xs text-on-surface-variant whitespace-pre-wrap break-all font-mono bg-surface-container rounded-lg">
              {log.args_json}
            </pre>
          </div>
          <div>
            <span className="text-label-sm font-medium text-on-surface-variant">result</span>
            <pre className="mt-1 p-3 text-xs text-on-surface-variant whitespace-pre-wrap break-all font-mono bg-surface-container rounded-lg">
              {log.result_text}
            </pre>
          </div>
        </div>
      )}
    </div>
  );
}

export default function AgentToolLogs() {
  const { t } = useTranslation();
  const { agentId } = useAgentContext();
  const [logs, setLogs] = useState<ToolLog[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [limit, setLimit] = useState(20);

  const load = useCallback(() => {
    setLoading(true);
    setError(null);
    fetchToolLogs(agentId, limit)
      .then(setLogs)
      .catch((e: Error) => setError(e.message))
      .finally(() => setLoading(false));
  }, [agentId, limit]);

  useEffect(() => {
    load();
  }, [load]);

  if (loading) {
    return (
      <div className="empty-state">
        <p className="text-body-lg text-on-surface-variant">{t("common.loading")}</p>
      </div>
    );
  }

  if (error) {
    return (
      <div className="card-outlined border-error bg-error-container/30 p-4">
        <div className="flex items-center gap-2">
          <span className="material-symbols-outlined text-error">error</span>
          <p className="text-body-lg text-error">{error}</p>
        </div>
      </div>
    );
  }

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between">
        <h2 className="text-title-lg text-on-surface font-semibold flex items-center gap-2">
          <span className="material-symbols-outlined text-primary">handyman</span>
          {t("toolLogs.title")}
        </h2>
        <div className="flex items-center gap-2">
          <button onClick={load} className="btn-text" title={t("toolLogs.refresh")}>
            <span className="material-symbols-outlined text-lg">refresh</span>
          </button>
          <select
            className="border border-outline rounded-lg px-3 py-1.5 text-body-sm bg-surface text-on-surface"
            value={limit}
            onChange={(e) => setLimit(Number(e.target.value))}
          >
            <option value={10}>10</option>
            <option value={20}>20</option>
            <option value={50}>50</option>
            <option value={100}>100</option>
          </select>
        </div>
      </div>

      {logs.length === 0 ? (
        <div className="empty-state">
          <span className="material-symbols-outlined text-4xl text-on-surface-variant">
            inbox
          </span>
          <p className="text-body-lg text-on-surface-variant">{t("toolLogs.empty")}</p>
        </div>
      ) : (
        <div className="space-y-3">
          {logs.map((log) => (
            <LogCard key={log.id} log={log} />
          ))}
        </div>
      )}
    </div>
  );
}
