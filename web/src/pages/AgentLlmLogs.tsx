import { useState, useEffect, useCallback } from "react";
import { useAgentContext } from "../hooks/useAgentContext";

// ── Types ──────────────────────────────────────────────────────────

interface LlmLog {
  id: string;
  agent_id: string;
  session_id: string | null;
  model: string | null;
  prompt: string;
  response: string;
  tool_calls: string | null;
  latency_ms: number | null;
  prompt_tokens: number | null;
  completion_tokens: number | null;
  total_tokens: number | null;
  error_code: string | null;
  error_body: string | null;
  requested_at: string | null;
  trigger_message_id: string | null;
  cache_read_tokens: number | null;
  cache_creation_tokens: number | null;
  is_bot_iteration: boolean;
  created_at: string;
}

interface LlmLogStat {
  date: string;
  count: number;
  total_tokens: number;
  prompt_tokens: number;
  completion_tokens: number;
  avg_latency_ms: number;
  error_count: number;
  cache_read_tokens: number;
  cache_creation_tokens: number;
}

interface ToolCallEntry {
  id: string;
  name: string;
  arguments: Record<string, unknown> | string;
}

interface ChatMessage {
  role: "system" | "user" | "assistant" | "tool";
  content: string;
  tool_call_id: string | null;
  tool_calls: ToolCallEntry[];
  content_parts: unknown[];
}

interface ToolDef {
  name: string;
  description?: string;
  parameters?: unknown;
}

interface ChatRequestSimple {
  model: string;
  messages: ChatMessage[];
  tools?: ToolDef[];
  temperature?: number;
  max_tokens?: number;
}

interface UsageInfo {
  prompt_tokens: number;
  completion_tokens: number;
  total_tokens: number;
  cache_read_input_tokens?: number;
  cache_creation_input_tokens?: number;
}

interface ChatResponseSimple {
  content: string;
  tool_calls: ToolCallEntry[];
  finish_reason: string;
  usage: UsageInfo;
}

// ── API ────────────────────────────────────────────────────────────

async function fetchLlmLogs(agentId: string, limit = 20): Promise<LlmLog[]> {
  const res = await fetch(`/api/agents/${agentId}/llm-logs?limit=${limit}`);
  if (!res.ok) throw new Error("Failed to fetch LLM logs");
  return res.json();
}

async function fetchLlmLogStats(agentId: string): Promise<LlmLogStat[]> {
  const res = await fetch(`/api/agents/${agentId}/llm-logs/stats`);
  if (!res.ok) throw new Error("Failed to fetch stats");
  return res.json();
}

// ── Helpers ────────────────────────────────────────────────────────

function tryParseJson<T>(str: string): T | null {
  try {
    return JSON.parse(str) as T;
  } catch {
    return null;
  }
}

function truncate(s: string, max: number): string {
  if (s.length <= max) return s;
  return s.slice(0, max) + "…";
}

function formatNumber(n: number): string {
  return n.toLocaleString();
}

// ── Sub-components ─────────────────────────────────────────────────

function Collapsible({
  title,
  icon,
  defaultOpen = false,
  badge,
  children,
}: {
  title: string;
  icon?: string;
  defaultOpen?: boolean;
  badge?: React.ReactNode;
  children: React.ReactNode;
}) {
  const [open, setOpen] = useState(defaultOpen);
  return (
    <div className="border border-outline-variant rounded-lg overflow-hidden">
      <button
        onClick={() => setOpen(!open)}
        className="w-full flex items-center gap-2 px-3 py-2 bg-surface-container-high text-left hover:bg-surface-container transition-colors"
      >
        <span className="material-symbols-outlined text-base text-on-surface-variant">
          {open ? "expand_less" : "expand_more"}
        </span>
        {icon && (
          <span className="text-base select-none">{icon}</span>
        )}
        <span className="text-label-lg font-medium text-on-surface flex-1">
          {title}
        </span>
        {badge}
      </button>
      {open && <div className="bg-surface">{children}</div>}
    </div>
  );
}

function RawJsonFallback({ content }: { content: string }) {
  const parsed = tryParseJson(content);
  return (
    <pre className="p-3 text-xs text-on-surface-variant whitespace-pre-wrap break-all font-mono">
      {parsed ? JSON.stringify(parsed, null, 2) : content}
    </pre>
  );
}

function CollapsibleText({
  text,
  threshold = 500,
}: {
  text: string;
  threshold?: number;
}) {
  const [expanded, setExpanded] = useState(text.length <= threshold);
  if (text.length <= threshold) {
    return (
      <pre className="text-body-sm whitespace-pre-wrap break-words font-mono">
        {text}
      </pre>
    );
  }
  return (
    <div>
      <pre className="text-body-sm whitespace-pre-wrap break-words font-mono">
        {expanded ? text : text.slice(0, threshold) + "…"}
      </pre>
      <button
        onClick={() => setExpanded(!expanded)}
        className="btn-text text-label-sm mt-1"
      >
        <span className="material-symbols-outlined text-sm mr-0.5">
          {expanded ? "unfold_less" : "unfold_more"}
        </span>
        {expanded ? "折りたたむ" : "全文を表示"}
      </button>
    </div>
  );
}

// ── Role style config ──────────────────────────────────────────────

const ROLE_STYLES: Record<
  string,
  { bg: string; badge: string; badgeText: string; label: string; icon: string }
> = {
  system: {
    bg: "bg-purple-50 dark:bg-purple-950/30 border-purple-200 dark:border-purple-800",
    badge: "bg-purple-100 dark:bg-purple-900 text-purple-800 dark:text-purple-200",
    badgeText: "SYSTEM",
    label: "System prompt",
    icon: "settings",
  },
  user: {
    bg: "bg-blue-50 dark:bg-blue-950/30 border-blue-200 dark:border-blue-800",
    badge: "bg-blue-100 dark:bg-blue-900 text-blue-800 dark:text-blue-200",
    badgeText: "USER",
    label: "User message",
    icon: "person",
  },
  assistant: {
    bg: "bg-green-50 dark:bg-green-950/30 border-green-200 dark:border-green-800",
    badge: "bg-green-100 dark:bg-green-900 text-green-800 dark:text-green-200",
    badgeText: "ASSISTANT",
    label: "Assistant",
    icon: "smart_toy",
  },
  tool: {
    bg: "bg-gray-50 dark:bg-gray-900/30 border-gray-200 dark:border-gray-700",
    badge: "bg-gray-100 dark:bg-gray-800 text-gray-700 dark:text-gray-300",
    badgeText: "TOOL",
    label: "Tool result",
    icon: "build",
  },
};

function getStyle(role: string) {
  return ROLE_STYLES[role] ?? ROLE_STYLES.tool;
}

// ── Message card ───────────────────────────────────────────────────

function MessageCard({ msg, index }: { msg: ChatMessage; index: number }) {
  const style = getStyle(msg.role);
  const hasToolCalls = msg.tool_calls && msg.tool_calls.length > 0;

  return (
    <div className={`border rounded-lg ${style.bg} overflow-hidden`}>
      {/* Header */}
      <div className="flex items-center gap-2 px-3 py-1.5">
        <span className="material-symbols-outlined text-sm opacity-70">
          {style.icon}
        </span>
        <span
          className={`inline-flex items-center px-2 py-0.5 rounded text-label-sm font-semibold ${style.badge}`}
        >
          {style.badgeText}
        </span>
        <span className="text-label-sm text-on-surface-variant">#{index + 1}</span>
        {msg.tool_call_id && (
          <span className="text-label-sm text-on-surface-variant font-mono ml-auto">
            call_id: {truncate(msg.tool_call_id, 20)}
          </span>
        )}
      </div>

      {/* Content */}
      {msg.content && (
        <div className="px-3 pb-2">
          <CollapsibleText
            text={msg.content}
            threshold={msg.role === "system" ? 500 : 1000}
          />
        </div>
      )}

      {/* Tool calls (for assistant messages) */}
      {hasToolCalls && (
        <div className="px-3 pb-2 space-y-1.5">
          <span className="text-label-sm font-medium text-on-surface-variant">
            Tool Calls:
          </span>
          {msg.tool_calls.map((tc, i) => (
            <div
              key={tc.id || i}
              className="bg-surface/60 border border-outline-variant rounded p-2"
            >
              <div className="flex items-center gap-2 mb-1">
                <span className="material-symbols-outlined text-sm text-on-surface-variant">
                  build
                </span>
                <span className="text-label-sm font-semibold text-on-surface">
                  {tc.name}
                </span>
                <span className="text-label-sm text-on-surface-variant font-mono">
                  {truncate(tc.id, 16)}
                </span>
              </div>
              <pre className="text-xs text-on-surface-variant whitespace-pre-wrap break-all font-mono">
                {typeof tc.arguments === "string"
                  ? tc.arguments
                  : JSON.stringify(tc.arguments, null, 2)}
              </pre>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

// ── Tools section ──────────────────────────────────────────────────

function ToolsSection({ tools }: { tools: ToolDef[] }) {
  return (
    <Collapsible
      title="利用可能なツール"
      icon="🔧"
      badge={
        <span className="badge-neutral text-label-sm">
          {tools.length} tools
        </span>
      }
    >
      <div className="p-3 space-y-1.5 max-h-64 overflow-y-auto">
        {tools.map((tool, i) => (
          <div
            key={i}
            className="flex items-start gap-2 px-2 py-1.5 rounded bg-surface-container"
          >
            <span className="material-symbols-outlined text-sm text-on-surface-variant mt-0.5">
              handyman
            </span>
            <div className="min-w-0">
              <span className="text-label-sm font-semibold text-on-surface">
                {tool.name}
              </span>
              {tool.description && (
                <p className="text-body-sm text-on-surface-variant truncate">
                  {tool.description}
                </p>
              )}
            </div>
          </div>
        ))}
      </div>
    </Collapsible>
  );
}

// ── Usage bar ──────────────────────────────────────────────────────

function UsageBar({ usage }: { usage: UsageInfo }) {
  const promptPct =
    usage.total_tokens > 0
      ? Math.round((usage.prompt_tokens / usage.total_tokens) * 100)
      : 0;
  return (
    <div className="space-y-1.5">
      <div className="flex items-center gap-4 text-label-sm">
        <span className="flex items-center gap-1 text-on-surface-variant">
          <span className="material-symbols-outlined text-sm">upload</span>
          Prompt: <strong className="text-on-surface">{formatNumber(usage.prompt_tokens)}</strong>
        </span>
        <span className="flex items-center gap-1 text-on-surface-variant">
          <span className="material-symbols-outlined text-sm">download</span>
          Completion: <strong className="text-on-surface">{formatNumber(usage.completion_tokens)}</strong>
        </span>
        <span className="flex items-center gap-1 text-on-surface-variant">
          <span className="material-symbols-outlined text-sm">data_usage</span>
          Total: <strong className="text-on-surface">{formatNumber(usage.total_tokens)}</strong>
        </span>
        {(usage.cache_read_input_tokens || 0) > 0 && (
          <span className="flex items-center gap-1 text-green-600 dark:text-green-400">
            <span className="material-symbols-outlined text-sm">cached</span>
            Cache hit: <strong>{formatNumber(usage.cache_read_input_tokens!)}</strong>
          </span>
        )}
        {(usage.cache_creation_input_tokens || 0) > 0 && (
          <span className="flex items-center gap-1 text-yellow-600 dark:text-yellow-400">
            <span className="material-symbols-outlined text-sm">save</span>
            Cache write: <strong>{formatNumber(usage.cache_creation_input_tokens!)}</strong>
          </span>
        )}
      </div>
      <div className="w-full h-2 bg-surface-container-high rounded-full overflow-hidden">
        <div
          className="h-full bg-primary rounded-full transition-all"
          style={{ width: `${promptPct}%` }}
          title={`Prompt: ${promptPct}%`}
        />
      </div>
    </div>
  );
}

// ── Finish reason badge ────────────────────────────────────────────

function FinishBadge({ reason }: { reason: string }) {
  const cls =
    reason === "stop"
      ? "badge-success"
      : reason === "tool_calls"
        ? "badge-info"
        : reason === "length"
          ? "badge-warning"
          : "badge-neutral";
  return (
    <span className={cls}>
      <span className="material-symbols-outlined text-sm mr-0.5">
        {reason === "stop"
          ? "check_circle"
          : reason === "tool_calls"
            ? "build"
            : reason === "length"
              ? "warning"
              : "info"}
      </span>
      {reason}
    </span>
  );
}

// ── Detail view ────────────────────────────────────────────────────

function LogDetail({ log }: { log: LlmLog }) {
  // Parse prompt: could be ChatRequestSimple or a plain array of messages (old format)
  const parsedPrompt = tryParseJson<ChatRequestSimple | ChatMessage[]>(log.prompt);
  const parsedResponse = tryParseJson<ChatResponseSimple>(log.response);

  let request: ChatRequestSimple | null = null;
  let messages: ChatMessage[] | null = null;

  if (parsedPrompt) {
    if (Array.isArray(parsedPrompt)) {
      // Old format: just an array of messages
      messages = parsedPrompt as ChatMessage[];
    } else if (
      typeof parsedPrompt === "object" &&
      "messages" in parsedPrompt
    ) {
      request = parsedPrompt as ChatRequestSimple;
      messages = request.messages;
    }
  }

  return (
    <div className="space-y-3 pt-3 border-t border-outline-variant">
      {/* ── Meta info ── */}
      <div className="flex flex-wrap gap-3 items-center text-label-sm px-1">
        {log.latency_ms != null && (
          <span className="inline-flex items-center gap-1 px-2 py-1 rounded-full bg-tertiary-container text-on-tertiary-container font-medium">
            ⚡ {formatNumber(log.latency_ms)}ms
          </span>
        )}
        {log.requested_at && (
          <span className="inline-flex items-center gap-1 text-on-surface-variant">
            <span className="material-symbols-outlined text-sm">schedule</span>
            リクエスト: {new Date(log.requested_at).toLocaleString()}
          </span>
        )}
        {log.trigger_message_id && (
          <span className="inline-flex items-center gap-1 px-2 py-1 rounded-full bg-indigo-100 dark:bg-indigo-900 text-indigo-800 dark:text-indigo-200 font-medium">
            {"\uD83D\uDCAC"} trigger_message_id: {log.trigger_message_id}
          </span>
        )}
        {log.is_bot_iteration && (
          <span className="inline-flex items-center gap-1 px-2 py-1 rounded-full bg-gray-100 dark:bg-gray-800 text-gray-600 dark:text-gray-400 font-medium">
            {"\uD83D\uDD04"} Bot iteration (tool call follow-up)
          </span>
        )}
      </div>

      {/* ── Error section ── */}
      {log.error_code && (
        <div className="card-outlined border-error bg-error-container/30 p-3 space-y-1">
          <div className="flex items-center gap-2">
            <span className="material-symbols-outlined text-error">error</span>
            <span className="text-label-lg font-semibold text-error">
              エラー: {log.error_code}
            </span>
          </div>
          {log.error_body && (
            <pre className="text-body-sm whitespace-pre-wrap break-words font-mono text-on-error-container">
              {log.error_body}
            </pre>
          )}
        </div>
      )}

      {/* ── Request section ── */}
      <Collapsible title="LLMリクエスト" icon="📤" defaultOpen>
        <div className="p-3 space-y-3">
          {/* Summary row */}
          {request && (
            <div className="flex flex-wrap gap-3 items-center text-label-sm pb-2 border-b border-outline-variant">
              <span className="flex items-center gap-1 text-on-surface-variant">
                <span className="material-symbols-outlined text-sm">model_training</span>
                <strong className="text-on-surface">{request.model}</strong>
              </span>
              {request.temperature != null && (
                <span className="flex items-center gap-1 text-on-surface-variant">
                  <span className="material-symbols-outlined text-sm">thermostat</span>
                  temp: <strong className="text-on-surface">{request.temperature}</strong>
                </span>
              )}
              {request.max_tokens != null && (
                <span className="flex items-center gap-1 text-on-surface-variant">
                  <span className="material-symbols-outlined text-sm">straighten</span>
                  max_tokens: <strong className="text-on-surface">{formatNumber(request.max_tokens)}</strong>
                </span>
              )}
            </div>
          )}

          {/* Messages */}
          {messages ? (
            <div className="space-y-2">
              <span className="text-label-sm font-medium text-on-surface-variant">
                Messages ({messages.length})
              </span>
              {messages.map((msg, i) => (
                <MessageCard key={i} msg={msg} index={i} />
              ))}
            </div>
          ) : (
            <RawJsonFallback content={log.prompt} />
          )}

          {/* Tools */}
          {request?.tools && request.tools.length > 0 && (
            <ToolsSection tools={request.tools} />
          )}
        </div>
      </Collapsible>

      {/* ── Response section ── */}
      <Collapsible title="LLMレスポンス" icon="📥" defaultOpen>
        <div className="p-3 space-y-3">
          {parsedResponse ? (
            <>
              {/* Usage */}
              {parsedResponse.usage && (
                <UsageBar usage={parsedResponse.usage} />
              )}

              {/* Finish reason */}
              <div className="flex items-center gap-2">
                <span className="text-label-sm text-on-surface-variant">
                  Finish reason:
                </span>
                <FinishBadge reason={parsedResponse.finish_reason} />
              </div>

              {/* Response content */}
              {parsedResponse.content && (
                <div className="bg-surface-container rounded-lg p-3">
                  <pre className="text-body-sm whitespace-pre-wrap break-words font-mono text-on-surface">
                    {parsedResponse.content}
                  </pre>
                </div>
              )}

              {/* Response tool calls */}
              {parsedResponse.tool_calls && parsedResponse.tool_calls.length > 0 && (
                <div className="space-y-2">
                  <span className="text-label-sm font-medium text-on-surface-variant">
                    Tool Calls ({parsedResponse.tool_calls.length})
                  </span>
                  {parsedResponse.tool_calls.map((tc, i) => (
                    <div
                      key={tc.id || i}
                      className="border border-outline-variant rounded-lg p-3 bg-surface-container"
                    >
                      <div className="flex items-center gap-2 mb-1.5">
                        <span className="material-symbols-outlined text-sm text-tertiary">
                          build
                        </span>
                        <span className="text-label-sm font-semibold text-on-surface">
                          {tc.name}
                        </span>
                        <span className="text-label-sm text-on-surface-variant font-mono">
                          {tc.id}
                        </span>
                      </div>
                      <pre className="text-xs text-on-surface-variant whitespace-pre-wrap break-all font-mono">
                        {typeof tc.arguments === "string"
                          ? tc.arguments
                          : JSON.stringify(tc.arguments, null, 2)}
                      </pre>
                    </div>
                  ))}
                </div>
              )}
            </>
          ) : (
            <RawJsonFallback content={log.response} />
          )}
        </div>
      </Collapsible>
    </div>
  );
}

// ── Stats section ─────────────────────────────────────────────────

function StatsSection({ stats }: { stats: LlmLogStat[] }) {
  if (stats.length === 0) return null;

  const totalCalls = stats.reduce((s, d) => s + d.count, 0);
  const totalTokens = stats.reduce((s, d) => s + d.total_tokens, 0);
  const totalErrors = stats.reduce((s, d) => s + d.error_count, 0);
  const totalCacheRead = stats.reduce((s, d) => s + d.cache_read_tokens, 0);
  const totalCacheCreation = stats.reduce((s, d) => s + d.cache_creation_tokens, 0);
  const weightedLatency = stats.reduce((s, d) => s + d.avg_latency_ms * d.count, 0);
  const avgLatency = totalCalls > 0 ? Math.round(weightedLatency / totalCalls) : 0;
  const maxDayTokens = Math.max(...stats.map((d) => d.total_tokens), 1);

  return (
    <div className="card-elevated space-y-3">
      <h3 className="text-title-md text-on-surface font-semibold flex items-center gap-2">
        <span className="material-symbols-outlined text-primary">analytics</span>
        過去30日の統計
      </h3>

      {/* Summary row */}
      <div className="flex flex-wrap gap-4 text-label-sm">
        <span className="inline-flex items-center gap-1 px-2 py-1 rounded-full bg-primary-container text-on-primary-container">
          <span className="material-symbols-outlined text-sm">call_made</span>
          合計呼び出し: <strong>{formatNumber(totalCalls)}</strong>
        </span>
        <span className="inline-flex items-center gap-1 px-2 py-1 rounded-full bg-secondary-container text-on-secondary-container">
          <span className="material-symbols-outlined text-sm">data_usage</span>
          合計トークン: <strong>{formatNumber(totalTokens)}</strong>
        </span>
        <span className="inline-flex items-center gap-1 px-2 py-1 rounded-full bg-tertiary-container text-on-tertiary-container">
          <span className="material-symbols-outlined text-sm">speed</span>
          平均レイテンシ: <strong>{formatNumber(avgLatency)}ms</strong>
        </span>
        {totalErrors > 0 && (
          <span className="inline-flex items-center gap-1 px-2 py-1 rounded-full bg-error-container text-on-error-container">
            <span className="material-symbols-outlined text-sm">error</span>
            エラー: <strong>{formatNumber(totalErrors)}</strong>
          </span>
        )}
        {totalCacheRead > 0 && (
          <span className="inline-flex items-center gap-1 px-2 py-1 rounded-full bg-green-100 dark:bg-green-900 text-green-800 dark:text-green-200">
            <span className="material-symbols-outlined text-sm">cached</span>
            キャッシュヒット: <strong>{formatNumber(totalCacheRead)}</strong>
          </span>
        )}
        {totalCacheCreation > 0 && (
          <span className="inline-flex items-center gap-1 px-2 py-1 rounded-full bg-yellow-100 dark:bg-yellow-900 text-yellow-800 dark:text-yellow-200">
            <span className="material-symbols-outlined text-sm">save</span>
            キャッシュ書込: <strong>{formatNumber(totalCacheCreation)}</strong>
          </span>
        )}
      </div>

      {/* Bar chart */}
      <Collapsible title="日別トークン使用量" icon="📊" defaultOpen={false}>
        <div className="p-3 space-y-1">
          {stats.map((day) => (
            <div key={day.date} className="flex items-center gap-2 text-label-sm">
              <span className="w-20 text-on-surface-variant shrink-0 font-mono">
                {day.date.slice(5)}
              </span>
              <div className="flex-1 h-4 bg-surface-container-high rounded overflow-hidden">
                <div
                  className="h-full bg-primary rounded transition-all"
                  style={{ width: `${Math.max((day.total_tokens / maxDayTokens) * 100, 1)}%` }}
                  title={`${formatNumber(day.total_tokens)} tokens`}
                />
              </div>
              <span className="w-20 text-right text-on-surface-variant shrink-0">
                {formatNumber(day.total_tokens)}
              </span>
              {day.error_count > 0 && (
                <span className="text-error text-label-sm">({day.error_count} err)</span>
              )}
            </div>
          ))}
        </div>
      </Collapsible>
    </div>
  );
}

// ── Log card (compact) ─────────────────────────────────────────────

function LogCard({ log }: { log: LlmLog }) {
  const [expanded, setExpanded] = useState(false);

  const parsedResponse = tryParseJson<ChatResponseSimple>(log.response);
  const parsedPrompt = tryParseJson<ChatRequestSimple | ChatMessage[]>(log.prompt);

  // Extract model from request if not in log.model
  const model =
    log.model ??
    (parsedPrompt && !Array.isArray(parsedPrompt)
      ? (parsedPrompt as ChatRequestSimple).model
      : null);

  // Response preview
  const responsePreview = parsedResponse?.content
    ? truncate(parsedResponse.content.replace(/\n/g, " "), 80)
    : null;

  const hasResponseToolCalls =
    parsedResponse?.tool_calls && parsedResponse.tool_calls.length > 0;

  return (
    <div className="card-elevated">
      {/* Compact header */}
      <button
        onClick={() => setExpanded(!expanded)}
        className="w-full text-left"
      >
        <div className="flex items-center justify-between flex-wrap gap-2">
          <div className="flex items-center gap-2 flex-wrap min-w-0">
            {model && (
              <span className="inline-flex items-center gap-1 px-2 py-0.5 rounded-full bg-primary-container text-on-primary-container text-label-sm font-medium">
                <span className="material-symbols-outlined text-sm">
                  model_training
                </span>
                {model}
              </span>
            )}
            {log.session_id && (
              <span className="inline-flex items-center gap-1 px-2 py-0.5 rounded-full bg-surface-container-high text-on-surface-variant text-label-sm">
                <span className="material-symbols-outlined text-sm">forum</span>
                {log.session_id.slice(0, 8)}…
              </span>
            )}
            {hasResponseToolCalls && (
              <span className="inline-flex items-center gap-1 px-2 py-0.5 rounded-full bg-tertiary-container text-on-tertiary-container text-label-sm">
                <span className="material-symbols-outlined text-sm">build</span>
                tool calls
              </span>
            )}
            {log.error_code && (
              <span className="inline-flex items-center gap-1 px-2 py-0.5 rounded-full bg-error-container text-on-error-container text-label-sm font-medium">
                <span className="material-symbols-outlined text-sm">error</span>
                {log.error_code}
              </span>
            )}
            {log.trigger_message_id && (
              <span className="inline-flex items-center gap-1 px-2 py-0.5 rounded-full bg-indigo-100 dark:bg-indigo-900 text-indigo-800 dark:text-indigo-200 text-label-sm">
                {"\uD83D\uDCAC"} {log.trigger_message_id}
              </span>
            )}
            {log.is_bot_iteration && (
              <span className="inline-flex items-center gap-1 px-2 py-0.5 rounded-full bg-gray-100 dark:bg-gray-800 text-gray-600 dark:text-gray-400 text-label-sm">
                {"\uD83D\uDD04"} bot iter
              </span>
            )}
          </div>
          <div className="flex items-center gap-3">
            <span className="text-label-sm text-on-surface-variant">
              {new Date(log.requested_at ?? log.created_at).toLocaleString()}
            </span>
            <span className="material-symbols-outlined text-base text-on-surface-variant">
              {expanded ? "expand_less" : "expand_more"}
            </span>
          </div>
        </div>

        {/* Token usage row */}
        {(parsedResponse?.usage || log.latency_ms != null) && (
          <div className="flex items-center gap-3 mt-1.5 text-label-sm text-on-surface-variant">
            {parsedResponse?.usage && (
              <>
                <span className="flex items-center gap-0.5">
                  <span className="material-symbols-outlined text-xs">upload</span>
                  {formatNumber(parsedResponse.usage.prompt_tokens)}
                </span>
                <span className="text-outline">/</span>
                <span className="flex items-center gap-0.5">
                  <span className="material-symbols-outlined text-xs">download</span>
                  {formatNumber(parsedResponse.usage.completion_tokens)}
                </span>
                <span className="text-outline">/</span>
                <span className="flex items-center gap-0.5">
                  <span className="material-symbols-outlined text-xs">data_usage</span>
                  {formatNumber(parsedResponse.usage.total_tokens)}
                </span>
              </>
            )}
            {log.latency_ms != null && (
              <>
                <span className="text-outline">·</span>
                <span className="flex items-center gap-0.5">
                  ⚡ {formatNumber(log.latency_ms)}ms
                </span>
              </>
            )}
            {parsedResponse?.finish_reason && (
              <>
                <span className="text-outline">·</span>
                <FinishBadge reason={parsedResponse.finish_reason} />
              </>
            )}
          </div>
        )}

        {/* Response preview */}
        {responsePreview && (
          <p className="mt-1 text-body-sm text-on-surface-variant truncate">
            {responsePreview}
          </p>
        )}
      </button>

      {/* Expanded detail */}
      {expanded && <LogDetail log={log} />}
    </div>
  );
}

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
