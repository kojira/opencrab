import { useState } from "react";
import type { ChatMessage, ChatRequestSimple, ChatResponseSimple, LlmLog } from "./model";
import { formatNumber, truncate, tryParseJson } from "./model";
import { FinishBadge } from "./Content";
import { LogDetail } from "./LogDetail";

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

export { LogCard };
