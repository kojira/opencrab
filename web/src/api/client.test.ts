import { describe, it, expect, vi, beforeEach } from 'vitest';
import { api } from './client';

const mockFetch = vi.fn();
vi.stubGlobal('fetch', mockFetch);

beforeEach(() => {
  mockFetch.mockReset();
});

describe('api.get', () => {
  it('returns parsed JSON on success', async () => {
    mockFetch.mockResolvedValue({
      ok: true,
      json: () => Promise.resolve({ id: '1', name: 'Alice' }),
    });

    const result = await api.get<{ id: string; name: string }>('/agents');

    expect(mockFetch).toHaveBeenCalledWith('/api/agents', {
      headers: { 'Content-Type': 'application/json' },
    });
    expect(result).toEqual({ id: '1', name: 'Alice' });
  });

  it('throws an error on non-ok response', async () => {
    mockFetch.mockResolvedValue({
      ok: false,
      status: 404,
      statusText: 'Not Found',
    });

    await expect(api.get('/agents/missing')).rejects.toThrow('404 Not Found');
  });
});

describe('api.post', () => {
  it('sends JSON body with POST method', async () => {
    mockFetch.mockResolvedValue({
      ok: true,
      json: () => Promise.resolve({ id: '2' }),
    });

    const body = { name: 'Bob', role: 'facilitator' };
    const result = await api.post<{ id: string }>('/agents', body);

    expect(mockFetch).toHaveBeenCalledWith('/api/agents', {
      headers: { 'Content-Type': 'application/json' },
      method: 'POST',
      body: JSON.stringify(body),
    });
    expect(result).toEqual({ id: '2' });
  });

  it('sends no body when body is undefined', async () => {
    mockFetch.mockResolvedValue({
      ok: true,
      json: () => Promise.resolve({}),
    });

    await api.post('/agents/1/action');

    expect(mockFetch).toHaveBeenCalledWith('/api/agents/1/action', {
      headers: { 'Content-Type': 'application/json' },
      method: 'POST',
      body: undefined,
    });
  });
});

describe('api.put', () => {
  it('sends JSON body with PUT method', async () => {
    mockFetch.mockResolvedValue({
      ok: true,
      json: () => Promise.resolve({ updated: true }),
    });

    const body = { name: 'Updated' };
    await api.put('/agents/1/identity', body);

    expect(mockFetch).toHaveBeenCalledWith('/api/agents/1/identity', {
      headers: { 'Content-Type': 'application/json' },
      method: 'PUT',
      body: JSON.stringify(body),
    });
  });
});

describe('api.del', () => {
  it('sends DELETE method', async () => {
    mockFetch.mockResolvedValue({
      ok: true,
      json: () => Promise.resolve({ deleted: true }),
    });

    await api.del('/agents/1');

    expect(mockFetch).toHaveBeenCalledWith('/api/agents/1', {
      headers: { 'Content-Type': 'application/json' },
      method: 'DELETE',
    });
  });
});
