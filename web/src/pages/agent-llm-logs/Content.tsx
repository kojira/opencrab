import { useState } from "react";
import type { ChatMessage, ToolDef, UsageInfo } from "./model";
import { formatNumber, truncate, tryParseJson } from "./model";

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

export { Collapsible, FinishBadge, MessageCard, RawJsonFallback, ToolsSection, UsageBar };
