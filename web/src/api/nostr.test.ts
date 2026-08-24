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
import { getNostrConfig, updateNostrConfig } from './nostr';

const mockedApi = vi.mocked(api);

describe('getNostrConfig', () => {
  it('calls GET /agents/:id/nostr and keeps owner_pubkey', async () => {
    const config = {
      configured: true,
      enabled: true,
      running: false,
      has_secret_key: true,
      secret_key_masked: '••••••••',
      owner_pubkey: 'aa'.repeat(32),
      relays: [],
      filter: { authors: [], keywords: [], kinds: [] },
    };
    mockedApi.get.mockResolvedValue(config);

    const result = await getNostrConfig('1');
    expect(mockedApi.get).toHaveBeenCalledWith('/agents/1/nostr');
    expect(result.owner_pubkey).toBe('aa'.repeat(32));
  });
});

describe('updateNostrConfig', () => {
  it('calls PUT /agents/:id/nostr with owner_pubkey', async () => {
    mockedApi.put.mockResolvedValue({ updated: true, enabled: true });

    const body = {
      relays: ['wss://relay.example.test'],
      authors: [],
      keywords: [],
      kinds: [1],
      enabled: true,
      owner_pubkey: 'npub1fixtureownerpubkeyvalue000000000000000000000000',
    };
    const result = await updateNostrConfig('1', body);

    expect(mockedApi.put).toHaveBeenCalledWith('/agents/1/nostr', body);
    expect(result.updated).toBe(true);
  });
});
