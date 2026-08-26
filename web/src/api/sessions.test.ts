import { describe, it, expect, vi } from 'vitest';

vi.mock('./client', () => ({
  api: {
    get: vi.fn(),
    post: vi.fn(),
  },
}));

import { api } from './client';
import {
  getSessions,
  getSession,
  sendWebMessage,
  conversationEventsUrl,
  createWebConversation,
} from './sessions';
import { conversationTitle } from '../lib/conversationTitle';
import type { SessionRow } from './types';

const mockedApi = vi.mocked(api);

function makeRow(overrides: Partial<SessionRow> = {}): SessionRow {
  return {
    id: 's1',
    mode: 'discussion',
    theme: 'AI Ethics',
    phase: 'main',
    turn_number: 3,
    status: 'active',
    participant_ids_json: '["a1","a2","a3"]',
    facilitator_id: 'a1',
    done_count: 1,
    max_turns: 10,
    metadata_json: null,
    ...overrides,
  };
}

describe('getSessions', () => {
  it('converts participant_ids_json to participant_count', async () => {
    mockedApi.get.mockResolvedValue([
      makeRow({ id: 's1', participant_ids_json: '["a1","a2"]' }),
      makeRow({ id: 's2', participant_ids_json: '["a1"]' }),
    ]);

    const result = await getSessions();

    expect(result).toEqual([
      expect.objectContaining({ id: 's1', participant_count: 2 }),
      expect.objectContaining({ id: 's2', participant_count: 1 }),
    ]);
    expect(result[0]).not.toHaveProperty('participant_ids_json');
    expect(mockedApi.get).toHaveBeenCalledWith('/sessions?limit=100', expect.anything());
  });

  it('handles invalid JSON in participant_ids_json gracefully', async () => {
    mockedApi.get.mockResolvedValue([
      makeRow({ participant_ids_json: 'not-json' }),
    ]);

    const result = await getSessions();
    expect(result[0].participant_count).toBe(0);
  });
});

describe('getSession', () => {
  it('converts a single session row to DTO', async () => {
    mockedApi.get.mockResolvedValue(
      makeRow({ id: 's5', participant_ids_json: '["a1","a2","a3","a4"]' }),
    );

    const result = await getSession('s5');

    expect(result).toEqual({
      id: 's5',
      mode: 'discussion',
      theme: 'AI Ethics',
      phase: 'main',
      turn_number: 3,
      status: 'active',
      participant_count: 4,
      agent_ids: ['a1', 'a2', 'a3', 'a4'],
      metadata_json: null,
      gateway_bound: false,
      web_binding_state: undefined,
    });
  });
});

describe('createWebConversation', () => {
  it('is the only create client and distinguishes 201 from 202', async () => {
    const fetchMock = vi.fn().mockResolvedValue({
      status: 202,
      json: async () => ({
        conversation_id: 'cccccccc-cccc-4ccc-8ccc-cccccccccccc',
        session_id: 'web-agent-1-cccccccc-cccc-4ccc-8ccc-cccccccccccc',
        binding_id: 'bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb',
        name: null,
        state: 'provisioning',
      }),
    });
    vi.stubGlobal('fetch', fetchMock);

    const created = await createWebConversation('agent-1');

    expect(created.httpStatus).toBe(202);
    expect(created.state).toBe('provisioning');
    expect(fetchMock).toHaveBeenCalledTimes(1);
    expect(fetchMock).toHaveBeenCalledWith(
      '/api/agents/agent-1/web-conversations',
      expect.objectContaining({
        method: 'POST',
        body: '{}',
      }),
    );
    vi.unstubAllGlobals();
  });

  it('rejects error bodies as ConversationCreateError', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue({
      status: 409,
      json: async () => ({ error: 'web_instance_unavailable' }),
    }));
    await expect(createWebConversation('agent-1', 'n')).rejects.toMatchObject({
      status: 409,
      code: 'web_instance_unavailable',
    });
    vi.unstubAllGlobals();
  });
});

describe('conversationTitle', () => {
  it('does not use the id as a display name', () => {
    expect(conversationTitle('web-a-1', 'web-a-1', '新しい会話')).toBe('新しい会話');
    expect(conversationTitle('web-a-1', 'Lunch', '新しい会話')).toBe('Lunch');
  });
});

describe('sendWebMessage', () => {
  it('posts exact body to the new gateway path', async () => {
    const fetchMock = vi.fn().mockResolvedValue({
      status: 202,
      json: async () => ({
        client_message_id: 'aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa',
        origin: 'web:aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa',
        seq: 1,
        state: 'accepted',
      }),
    });
    vi.stubGlobal('fetch', fetchMock);

    const accepted = await sendWebMessage(
      'web-agent-1-conv',
      'aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa',
      'hello',
    );

    expect(accepted.state).toBe('accepted');
    expect(fetchMock).toHaveBeenCalledWith(
      '/api/web-conversations/web-agent-1-conv/messages',
      expect.objectContaining({
        method: 'POST',
        body: JSON.stringify({
          client_message_id: 'aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa',
          text: 'hello',
          attachments: [],
        }),
      }),
    );
    expect(conversationEventsUrl('web-agent-1-conv')).toBe(
      '/api/web-conversations/web-agent-1-conv/events',
    );
    vi.unstubAllGlobals();
  });
});
