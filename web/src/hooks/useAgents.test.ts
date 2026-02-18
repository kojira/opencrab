import { describe, it, expect, vi, beforeEach } from 'vitest';
import { renderHook, waitFor } from '@testing-library/react';

vi.mock('../api/agents', () => ({
  getAgents: vi.fn(),
}));

import { getAgents } from '../api/agents';
import { useAgents } from './useAgents';
import type { AgentSummary } from '../api/types';

const mockedGetAgents = vi.mocked(getAgents);

beforeEach(() => {
  mockedGetAgents.mockReset();
});

const fakeAgents: AgentSummary[] = [
  {
    id: 'a1',
    name: 'Alice',
    persona_name: 'Curious',
    role: 'discussant',
    image_url: null,
    status: 'active',
    skill_count: 3,
    session_count: 5,
  },
];

describe('useAgents', () => {
  it('starts with loading=true', () => {
    mockedGetAgents.mockReturnValue(new Promise(() => {})); // never resolves

    const { result } = renderHook(() => useAgents());
    expect(result.current.loading).toBe(true);
    expect(result.current.agents).toEqual([]);
    expect(result.current.error).toBeNull();
  });

  it('returns agents on success', async () => {
    mockedGetAgents.mockResolvedValue(fakeAgents);

    const { result } = renderHook(() => useAgents());

    await waitFor(() => {
      expect(result.current.loading).toBe(false);
    });

    expect(result.current.agents).toEqual(fakeAgents);
    expect(result.current.error).toBeNull();
  });

  it('returns error message on failure', async () => {
    mockedGetAgents.mockRejectedValue(new Error('Network error'));

    const { result } = renderHook(() => useAgents());

    await waitFor(() => {
      expect(result.current.loading).toBe(false);
    });

    expect(result.current.error).toBe('Network error');
    expect(result.current.agents).toEqual([]);
  });
});
