import { useState, useEffect, useCallback } from "react";
import { useAgentContext } from "../hooks/useAgentContext";
import type { LlmLog, LlmLogStat } from "./agent-llm-logs/model";
import { fetchLlmLogs, fetchLlmLogStats } from "./agent-llm-logs/model";
import { LogCard } from "./agent-llm-logs/LogCard";
import { StatsSection } from "./agent-llm-logs/StatsSection";

// ── Main component ─────────────────────────────────────────────────

export default function AgentLlmLogs() {
  const { agentId } = useAgentContext();
  const [logs, setLogs] = useState<LlmLog[]>([]);
  const [stats, setStats] = useState<LlmLogStat[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [limit, setLimit] = useState(20);

  const load = useCallback(() => {
    setLoading(true);
    setError(null);
    fetchLlmLogs(agentId, limit)
      .then(setLogs)
      .catch((e: Error) => setError(e.message))
      .finally(() => setLoading(false));
  }, [agentId, limit]);

  useEffect(() => {
    load();
  }, [load]);

  useEffect(() => {
    fetchLlmLogStats(agentId)
      .then(setStats)
      .catch(() => setStats([]));
  }, [agentId]);

  if (loading) {
    return (
      <div className="empty-state">
        <p className="text-body-lg text-on-surface-variant">Loading...</p>
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
      {/* Header */}
      <div className="flex items-center justify-between">
        <h2 className="text-title-lg text-on-surface font-semibold flex items-center gap-2">
          <span className="material-symbols-outlined text-primary">
            receipt_long
          </span>
          LLMログ
        </h2>
        <div className="flex items-center gap-2">
          <button onClick={load} className="btn-text" title="更新">
            <span className="material-symbols-outlined text-lg">refresh</span>
          </button>
          <select
            className="border border-outline rounded-lg px-3 py-1.5 text-body-sm bg-surface text-on-surface"
            value={limit}
            onChange={(e) => setLimit(Number(e.target.value))}
          >
            <option value={10}>10件</option>
            <option value={20}>20件</option>
            <option value={50}>50件</option>
            <option value={100}>100件</option>
          </select>
        </div>
      </div>

      {/* Stats */}
      <StatsSection stats={stats} />

      {/* Log list */}
      {logs.length === 0 ? (
        <div className="empty-state">
          <span className="material-symbols-outlined text-4xl text-on-surface-variant">
            inbox
          </span>
          <p className="text-body-lg text-on-surface-variant">
            LLMログがありません
          </p>
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
