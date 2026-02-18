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
import {
  getAgent,
  getAgents,
  getDiscordConfig,
  updateDiscordConfig,
  deleteDiscordConfig,
  startDiscordGateway,
  stopDiscordGateway,
} from './agents';
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

describe('getDiscordConfig', () => {
  it('calls GET /agents/:id/discord', async () => {
    const config = {
      configured: true,
      enabled: true,
      token_masked: 'OTk1MTYx...',
      owner_discord_id: '390123',
      running: true,
    };
    mockedApi.get.mockResolvedValue(config);

    const result = await getDiscordConfig('a1');
    expect(mockedApi.get).toHaveBeenCalledWith('/agents/a1/discord');
    expect(result).toEqual(config);
  });

  it('returns configured: false when not set up', async () => {
    mockedApi.get.mockResolvedValue({ configured: false });

    const result = await getDiscordConfig('a2');
    expect(result.configured).toBe(false);
  });
});

describe('updateDiscordConfig', () => {
  it('calls PUT /agents/:id/discord with token and owner', async () => {
    mockedApi.put.mockResolvedValue({ ok: true, message: 'Discord bot started.' });

    const result = await updateDiscordConfig('a1', {
      bot_token: 'BOT_TOKEN_123',
      owner_discord_id: '390123',
    });

    expect(mockedApi.put).toHaveBeenCalledWith('/agents/a1/discord', {
      bot_token: 'BOT_TOKEN_123',
      owner_discord_id: '390123',
    });
    expect(result.ok).toBe(true);
  });
});

describe('deleteDiscordConfig', () => {
  it('calls DELETE /agents/:id/discord', async () => {
    mockedApi.del.mockResolvedValue({ deleted: true });

    const result = await deleteDiscordConfig('a1');
    expect(mockedApi.del).toHaveBeenCalledWith('/agents/a1/discord');
    expect(result.deleted).toBe(true);
  });
});

describe('startDiscordGateway', () => {
  it('calls POST /agents/:id/discord/start', async () => {
    mockedApi.post.mockResolvedValue({ ok: true });

    const result = await startDiscordGateway('a1');
    expect(mockedApi.post).toHaveBeenCalledWith('/agents/a1/discord/start', {});
    expect(result.ok).toBe(true);
  });
});

describe('stopDiscordGateway', () => {
  it('calls POST /agents/:id/discord/stop', async () => {
    mockedApi.post.mockResolvedValue({ ok: true });

    const result = await stopDiscordGateway('a1');
    expect(mockedApi.post).toHaveBeenCalledWith('/agents/a1/discord/stop', {});
    expect(result.ok).toBe(true);
  });
});
