import { describe, it, expect, vi } from 'vitest';

vi.mock('./client', () => ({
  api: {
    get: vi.fn(),
    post: vi.fn(),
    put: vi.fn(),
    del: vi.fn(),
    patch: vi.fn(),
  },
}));

import { api } from './client';
import { getAgent, getAgents } from './agents';

const mockedApi = vi.mocked(api);

describe('getAgents', () => {
  it('returns agent summaries from API', async () => {
    const agents = [{ id: '1', name: 'Alice' }];
    mockedApi.get.mockResolvedValue(agents);

    const result = await getAgents();
    expect(mockedApi.get).toHaveBeenCalledWith('/agents');
    expect(result).toEqual(agents);
  });
});

describe('getAgent', () => {
  it('maps flat agent row into AgentDetail', async () => {
    const agentRow = {
      agent_id: 'a1',
      name: 'Alice',
      job_title: 'Engineer',
      organization: 'Acme',
      image_url: 'https://example.com/alice.png',
      persona_name: 'Curious Alice',
      personality: '{"style":"analytical"}',
      instructions: 'Be helpful',
      model: 'anthropic:claude-sonnet-4-20250514',
      metadata_json: null,
    };

    mockedApi.get.mockResolvedValue(agentRow);

    const result = await getAgent('a1');

    expect(result).toEqual({
      id: 'a1',
      name: 'Alice',
      job_title: 'Engineer',
      organization: 'Acme',
      image_url: 'https://example.com/alice.png',
      persona_name: 'Curious Alice',
      personality: '{"style":"analytical"}',
      instructions: 'Be helpful',
      model: 'anthropic:claude-sonnet-4-20250514',
      reasoning_effort: null,
      web_search: null,
      metadata_json: null,
    });
  });

  it('uses defaults when response is null', async () => {
    mockedApi.get.mockResolvedValue(null);

    await expect(getAgent('x1')).rejects.toThrow('Agent not found');
  });
});
