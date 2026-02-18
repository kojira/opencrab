import { describe, it, expect, vi } from 'vitest';

vi.mock('./client', () => ({
  api: {
    get: vi.fn(),
    post: vi.fn(),
  },
}));

import { api } from './client';
import { getSessions, getSession } from './sessions';
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
    });
  });
});
