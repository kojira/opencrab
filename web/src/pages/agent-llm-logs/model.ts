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

export type {
  ChatMessage,
  ChatRequestSimple,
  ChatResponseSimple,
  LlmLog,
  LlmLogStat,
  ToolDef,
  UsageInfo,
};
export { fetchLlmLogs, fetchLlmLogStats, formatNumber, truncate, tryParseJson };
