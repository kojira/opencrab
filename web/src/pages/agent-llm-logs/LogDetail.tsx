import { useEffect, useState } from "react";
import type {
  ChatMessage,
  ChatRequestSimple,
  ChatResponseSimple,
  LlmLog,
  ToolHistoryResponse,
} from "./model";
import { fetchLlmToolHistory, formatNumber, tryParseJson } from "./model";
import {
  Collapsible,
  FinishBadge,
  MessageCard,
  RawJsonFallback,
  ToolsSection,
  UsageBar,
} from "./Content";

// ── Detail view ────────────────────────────────────────────────────

const providerStateLabels = {
  legacy_unknown: "旧ログのため判定不能",
  not_requested: "未要求",
  not_used: "要求したが未使用",
  captured: "履歴取得済み",
  incomplete: "履歴解析失敗",
} as const;

function LogDetail({ log }: { log: LlmLog }) {
  // Parse prompt: could be ChatRequestSimple or a plain array of messages (old format)
  const parsedPrompt = tryParseJson<ChatRequestSimple | ChatMessage[]>(log.prompt);
  const parsedResponse = tryParseJson<ChatResponseSimple>(log.response);

  let request: ChatRequestSimple | null = null;
  let messages: ChatMessage[] | null = null;
  const [toolHistory, setToolHistory] = useState<ToolHistoryResponse | null>(null);
  const [toolHistoryError, setToolHistoryError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    setToolHistory(null);
    setToolHistoryError(null);
    fetchLlmToolHistory(log.agent_id, log.id)
      .then((history) => {
        if (!cancelled) setToolHistory(history);
      })
      .catch((error: unknown) => {
        if (!cancelled) {
          setToolHistoryError(error instanceof Error ? error.message : String(error));
        }
      });
    return () => {
      cancelled = true;
    };
  }, [log.agent_id, log.id]);

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

      {/* ── Tool history ── */}
      <Collapsible title="Tool history" icon="🔧" defaultOpen>
        <div className="p-3 space-y-3">
          {toolHistoryError && (
            <div className="text-body-sm text-error">{toolHistoryError}</div>
          )}
          {!toolHistory && !toolHistoryError && (
            <div className="text-body-sm text-on-surface-variant">読み込み中…</div>
          )}
          {toolHistory && (
            <>
              {toolHistory.entries.length === 0 ? (
                <div className="text-body-sm text-on-surface-variant">
                  local tool履歴はありません。
                </div>
              ) : (
                toolHistory.entries.map((entry, index) => {
                  const callId = typeof entry.call.id === "string" ? entry.call.id : "";
                  const fn = entry.call.function as
                    | { name?: string; arguments?: unknown }
                    | undefined;
                  return (
                    <div
                      key={`${callId}-${index}`}
                      className="border border-outline-variant rounded-lg p-3 bg-surface-container space-y-2"
                    >
                      <div className="flex flex-wrap gap-2 text-label-sm">
                        <strong>{fn?.name ?? "tool"}</strong>
                        {callId && <span className="font-mono">call: {callId}</span>}
                        {entry.source_memory_log_id != null && (
                          <span className="font-mono">log:{entry.source_memory_log_id}</span>
                        )}
                        {entry.subtask_id && (
                          <span className="font-mono">subtask: {entry.subtask_id}</span>
                        )}
                      </div>
                      <pre className="text-xs whitespace-pre-wrap break-all font-mono">
                        {typeof fn?.arguments === "string"
                          ? fn.arguments
                          : JSON.stringify(fn?.arguments ?? {}, null, 2)}
                      </pre>
                      {entry.result && (
                        <pre className="text-xs whitespace-pre-wrap break-all font-mono border-t border-outline-variant pt-2">
                          {entry.result}
                        </pre>
                      )}
                      {entry.completion && (
                        <pre className="text-xs whitespace-pre-wrap break-all font-mono border-t border-outline-variant pt-2">
                          {JSON.stringify(entry.completion, null, 2)}
                        </pre>
                      )}
                    </div>
                  );
                })
              )}

              <div className="border border-outline-variant rounded-lg p-3 space-y-2">
                <div className="text-label-sm font-semibold">
                  Provider-native: {providerStateLabels[toolHistory.provider_tool_history.state]}
                </div>
                {toolHistory.provider_tool_history.calls.map((call) => (
                  <div key={call.id} className="text-xs font-mono space-y-1">
                    <div>{call.id} · {call.status ?? "status unknown"}</div>
                    <pre className="whitespace-pre-wrap break-all">
                      {JSON.stringify(call.action, null, 2)}
                    </pre>
                  </div>
                ))}
                {toolHistory.provider_tool_history.citations.map((citation) => (
                  <a
                    key={citation.url}
                    className="block text-xs text-primary break-all underline"
                    href={citation.url}
                    target="_blank"
                    rel="noreferrer"
                  >
                    {citation.title ?? citation.url}
                  </a>
                ))}
              </div>
            </>
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

export { LogDetail };
