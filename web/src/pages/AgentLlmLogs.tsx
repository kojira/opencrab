import { useState, useEffect } from "react";
import { useAgentContext } from "../hooks/useAgentContext";

interface LlmLog {
  id: string;
  agent_id: string;
  session_id: string | null;
  model: string | null;
  prompt: string;
  response: string;
  tool_calls: string | null;
  created_at: string;
}

async function fetchLlmLogs(agentId: string, limit = 20): Promise<LlmLog[]> {
  const res = await fetch(`/api/agents/${agentId}/llm-logs?limit=${limit}`);
  if (!res.ok) throw new Error("Failed to fetch LLM logs");
  return res.json();
}

function CollapsibleSection({
  title,
  content,
  defaultOpen = false,
}: {
  title: string;
  content: string;
  defaultOpen?: boolean;
}) {
  const [open, setOpen] = useState(defaultOpen);
  let parsed: unknown;
  try {
    parsed = JSON.parse(content);
  } catch {
    parsed = null;
  }

  return (
    <div className="border border-outline-variant rounded-lg overflow-hidden">
      <button
        onClick={() => setOpen(!open)}
        className="w-full flex items-center justify-between px-3 py-2 bg-surface-container-high text-left"
      >
        <span className="text-label-lg font-medium text-on-surface">{title}</span>
        <span className="material-symbols-outlined text-sm text-on-surface-variant">
          {open ? "expand_less" : "expand_more"}
        </span>
      </button>
      {open && (
        <div className="p-3 bg-surface overflow-x-auto">
          <pre className="text-xs text-on-surface-variant whitespace-pre-wrap break-all">
            {parsed ? JSON.stringify(parsed, null, 2) : content}
          </pre>
        </div>
      )}
    </div>
  );
}

export default function AgentLlmLogs() {
  const { agentId } = useAgentContext();
  const [logs, setLogs] = useState<LlmLog[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [limit, setLimit] = useState(20);

  useEffect(() => {
    setLoading(true);
    fetchLlmLogs(agentId, limit)
      .then(setLogs)
      .catch((e: Error) => setError(e.message))
      .finally(() => setLoading(false));
  }, [agentId, limit]);

  if (loading) return <div className="empty-state"><p className="text-body-lg text-on-surface-variant">Loading...</p></div>;
  if (error) return <div className="card-outlined border-error p-4"><p className="text-error">{error}</p></div>;

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between">
        <h2 className="text-title-lg text-on-surface font-semibold flex items-center gap-2">
          <span className="material-symbols-outlined text-primary">receipt_long</span>
          LLMログ
        </h2>
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

      {logs.length === 0 ? (
        <div className="empty-state">
          <span className="material-symbols-outlined text-4xl text-on-surface-variant">inbox</span>
          <p className="text-body-lg text-on-surface-variant">LLMログがありません</p>
        </div>
      ) : (
        <div className="space-y-3">
          {logs.map((log) => (
            <div key={log.id} className="card-elevated space-y-2">
              <div className="flex items-center justify-between flex-wrap gap-2">
                <div className="flex items-center gap-2 flex-wrap">
                  {log.model && (
                    <span className="inline-flex items-center gap-1 px-2 py-0.5 rounded-full bg-primary-container text-on-primary-container text-label-sm">
                      <span className="material-symbols-outlined text-sm">model_training</span>
                      {log.model}
                    </span>
                  )}
                  {log.session_id && (
                    <span className="inline-flex items-center gap-1 px-2 py-0.5 rounded-full bg-surface-container-high text-on-surface-variant text-label-sm">
                      <span className="material-symbols-outlined text-sm">forum</span>
                      {log.session_id.slice(0, 8)}...
                    </span>
                  )}
                  {log.tool_calls && (
                    <span className="inline-flex items-center gap-1 px-2 py-0.5 rounded-full bg-tertiary-container text-on-tertiary-container text-label-sm">
                      <span className="material-symbols-outlined text-sm">build</span>
                      tool calls
                    </span>
                  )}
                </div>
                <span className="text-label-sm text-on-surface-variant">
                  {new Date(log.created_at).toLocaleString()}
                </span>
              </div>
              <CollapsibleSection title="Prompt (入力)" content={log.prompt} />
              <CollapsibleSection title="Response (出力)" content={log.response} defaultOpen />
              {log.tool_calls && (
                <CollapsibleSection title="Tool Calls" content={log.tool_calls} />
              )}
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
