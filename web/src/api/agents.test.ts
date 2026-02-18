import { describe, it, expect, vi } from 'vitest';

vi.mock('./client', () => ({
  api: {
    get: vi.fn(),
    post: vi.fn(),
    put: vi.fn(),
    del: vi.fn(),
  },
}));

import { api } from './client';
import { getAgent, getAgents } from './agents';
import type { IdentityRow, SoulRow } from './types';

const mockedApi = vi.mocked(api);

describe('getAgents', () => {
  it('returns agent summaries from API', async () => {
    const agents = [{ id: '1', name: 'Alice', role: 'discussant' }];
    mockedApi.get.mockResolvedValue(agents);

    const result = await getAgents();
    expect(mockedApi.get).toHaveBeenCalledWith('/agents');
    expect(result).toEqual(agents);
  });
});

describe('getAgent', () => {
  it('flattens identity + soul into AgentDetail', async () => {
    const identity: IdentityRow = {
      agent_id: 'a1',
      name: 'Alice',
      role: 'discussant',
      job_title: 'Engineer',
      organization: 'Acme',
      image_url: 'https://example.com/alice.png',
      metadata_json: null,
    };
    const soul: SoulRow = {
      agent_id: 'a1',
      persona_name: 'Curious Alice',
      social_style_json: '{"style":"analytical"}',
      personality_json: '{"openness":0.8}',
      thinking_style_json: '{"primary":"logical"}',
      custom_traits_json: null,
    };

    mockedApi.get.mockResolvedValue({ identity, soul });

    const result = await getAgent('a1');

    expect(result).toEqual({
      id: 'a1',
      name: 'Alice',
      role: 'discussant',
      job_title: 'Engineer',
      organization: 'Acme',
      image_url: 'https://example.com/alice.png',
      persona_name: 'Curious Alice',
      social_style_json: '{"style":"analytical"}',
      personality_json: '{"openness":0.8}',
      thinking_style_json: '{"primary":"logical"}',
      custom_traits_json: null,
    });
  });

  it('uses defaults when identity and soul are null', async () => {
    mockedApi.get.mockResolvedValue({ identity: null, soul: null });

    const result = await getAgent('x1');

    expect(result).toEqual({
      id: 'x1',
      name: '',
      role: '',
      job_title: null,
      organization: null,
      image_url: null,
      persona_name: '',
      social_style_json: '{}',
      personality_json: '{}',
      thinking_style_json: '{}',
      custom_traits_json: null,
    });
  });
});
