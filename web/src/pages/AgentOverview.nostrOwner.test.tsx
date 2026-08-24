import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, screen, waitFor, fireEvent } from '@testing-library/react';

vi.mock('../api/nostr', () => ({
  getNostrConfig: vi.fn(),
  updateNostrConfig: vi.fn(),
  deleteNostrConfig: vi.fn(),
  generateNostrKey: vi.fn(),
}));

import { getNostrConfig, updateNostrConfig } from '../api/nostr';
import { NostrSection } from './AgentOverview';

const mockedGet = vi.mocked(getNostrConfig);
const mockedUpdate = vi.mocked(updateNostrConfig);

const HEX = 'ab'.repeat(32);

beforeEach(() => {
  mockedGet.mockReset();
  mockedUpdate.mockReset();
});

describe('NostrSection owner_pubkey restore', () => {
  it('loads owner_pubkey and sends it on the existing PUT', async () => {
    mockedGet.mockResolvedValue({
      configured: true,
      enabled: false,
      running: false,
      has_secret_key: false,
      secret_key_masked: '',
      owner_pubkey: HEX,
      relays: [],
      filter: { authors: [], keywords: [], kinds: [] },
    });
    mockedUpdate.mockResolvedValue({ updated: true, enabled: true });

    render(<NostrSection agentId="1" />);
    await waitFor(() => expect(mockedGet).toHaveBeenCalled());
    expect(screen.getByDisplayValue(HEX)).toBeInTheDocument();

    fireEvent.click(screen.getByRole('button', { name: 'agentDetail.nostrEnable' }));
    await waitFor(() => expect(mockedUpdate).toHaveBeenCalled());
    const [agentId, body] = mockedUpdate.mock.calls[0];
    expect(agentId).toBe('1');
    expect(body.owner_pubkey).toBe(HEX);
  });

  it('sends empty owner_pubkey to clear', async () => {
    mockedGet.mockResolvedValue({
      configured: true,
      enabled: false,
      running: false,
      has_secret_key: false,
      secret_key_masked: '',
      owner_pubkey: HEX,
      relays: [],
      filter: { authors: [], keywords: [], kinds: [] },
    });
    mockedUpdate.mockResolvedValue({ updated: true, enabled: true });

    render(<NostrSection agentId="1" />);
    await waitFor(() => expect(screen.getByDisplayValue(HEX)).toBeInTheDocument());
    fireEvent.change(screen.getByDisplayValue(HEX), { target: { value: '' } });
    fireEvent.click(screen.getByRole('button', { name: 'agentDetail.nostrEnable' }));
    await waitFor(() => expect(mockedUpdate).toHaveBeenCalled());
    expect(mockedUpdate.mock.calls[0][1].owner_pubkey).toBe('');
  });
});
