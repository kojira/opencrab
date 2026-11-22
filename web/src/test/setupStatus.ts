import type { SetupStatus } from '../api/setup';

/** A setup status for tests: llm and agent done, the rest as given. */
export function setupStatus(overrides: Partial<Record<keyof SetupStatus['steps'], boolean>> = {}): SetupStatus {
  const done = { llm_provider: true, agent: true, gateway: false, conversation: false, ...overrides };
  const order = ['llm_provider', 'agent', 'gateway', 'conversation'] as const;
  const next = order.find((k) => !done[k]) ?? null;
  return {
    steps: {
      llm_provider: { done: done.llm_provider, default_provider: 'chatgpt' },
      agent: { done: done.agent, agent_ids: ['crab'] },
      gateway: {
        done: done.gateway,
        gate_socket: '/tmp/example-gate.sock',
        connected: done.gateway
          ? [{ instance_id: 'aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa', agent_id: 'crab' }]
          : [],
      },
      conversation: {
        done: done.conversation,
        agent_id: done.conversation ? 'crab' : null,
        session_id: done.conversation ? 'extgate-session-1' : null,
      },
    },
    complete: next === null,
    next_step: next,
  };
}
