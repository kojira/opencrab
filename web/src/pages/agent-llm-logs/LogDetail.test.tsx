import { afterEach, describe, expect, it, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import { LogDetail } from "./LogDetail";
import type { LlmLog } from "./model";

const log: LlmLog = {
  id: "llm-967",
  agent_id: "agent-967",
  session_id: "session-967",
  model: "chatgpt:gpt-5.6-sol",
  prompt: JSON.stringify({ model: "gpt-5.6-sol", messages: [] }),
  response: "{}",
  tool_calls: null,
  latency_ms: 1,
  prompt_tokens: null,
  completion_tokens: null,
  total_tokens: null,
  error_code: null,
  error_body: null,
  requested_at: null,
  trigger_message_id: null,
  cache_read_tokens: null,
  cache_creation_tokens: null,
  provider_tool_history: {
    state: "not_requested",
    provider: null,
    calls: [],
    citations: [],
  },
  is_bot_iteration: true,
  created_at: "2026-09-08T00:00:00Z",
};

afterEach(() => vi.restoreAllMocks());

describe("LogDetail tool history", () => {
  it("shows local call correlation and provider-native search", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({
        ok: true,
        json: async () => ({
          entries: [
            {
              source_memory_log_id: 113104,
              call: {
                id: "call-967",
                function: {
                  name: "execute_shell",
                  arguments: '{"command":"sleep 1"}',
                },
              },
              result: '{"status":"spawned","subtask_id":"sub-967"}',
              subtask_id: "sub-967",
              completion: { type: "subtask_completed", result: "done" },
            },
          ],
          provider_tool_history: {
            state: "captured",
            provider: "chatgpt",
            calls: [
              {
                id: "ws-967",
                status: "completed",
                action: { type: "search", query: "Hokkaido weather" },
              },
            ],
            citations: [
              { url: "https://example.test/weather", title: "Weather" },
            ],
          },
        }),
      }),
    );

    render(<LogDetail log={log} />);

    expect(await screen.findByText("execute_shell")).toBeInTheDocument();
    expect(screen.getByText("subtask: sub-967")).toBeInTheDocument();
    expect(screen.getByText(/Hokkaido weather/)).toBeInTheDocument();
    expect(screen.getByRole("link", { name: "Weather" })).toHaveAttribute(
      "href",
      "https://example.test/weather",
    );
  });
});
