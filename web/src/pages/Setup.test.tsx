import { describe, it, expect, vi, beforeEach } from 'vitest';
import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { MemoryRouter } from 'react-router-dom';
import { setupStatus } from '../test/setupStatus';

const getSetupStatus = vi.fn();
vi.mock('../api/setup', async () => {
  const actual = await vi.importActual<typeof import('../api/setup')>('../api/setup');
  return { ...actual, getSetupStatus: () => getSetupStatus() };
});
vi.mock('../api/providers', () => ({
  getLlmProviders: vi.fn().mockResolvedValue({ providers: [] }),
  updateLlmProvider: vi.fn(),
}));

import Setup from './Setup';

beforeEach(() => {
  getSetupStatus.mockReset();
});

function renderSetup() {
  return render(
    <MemoryRouter>
      <Setup />
    </MemoryRouter>,
  );
}

describe('Setup', () => {
  it('shows four steps and opens the gateway step with this core socket and agent id', async () => {
    getSetupStatus.mockResolvedValue(setupStatus());
    renderSetup();
    await waitFor(() => expect(screen.getByText('setup.gateway.title')).toBeInTheDocument());
    for (const k of ['llm_provider', 'agent', 'gateway', 'conversation']) {
      expect(screen.getByRole('button', { name: new RegExp(`setup.step.${k}`) })).toBeInTheDocument();
    }
    expect(screen.getByText('/tmp/example-gate.sock')).toBeInTheDocument();
    expect(screen.getByText('crab')).toBeInTheDocument();
    // The placement and run commands are filled with the same values.
    expect(screen.getByText(/"core_socket": "\/tmp\/example-gate.sock"/)).toBeInTheDocument();
    expect(screen.getByText(/--agent crab --new "Terminal chat" --mode repl$/m)).toBeInTheDocument();
    expect(screen.getByText('setup.gateway.waiting')).toBeInTheDocument();
  });

  it('lists the connected gateway instances once one is connected', async () => {
    getSetupStatus.mockResolvedValue(setupStatus({ gateway: true }));
    renderSetup();
    await waitFor(() => expect(screen.getByText('setup.conversation.title')).toBeInTheDocument());
    fireEvent.click(screen.getByRole('button', { name: /setup.step.gateway/ }));
    await waitFor(() =>
      expect(screen.getByText(/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa → crab/)).toBeInTheDocument(),
    );
  });

  it('links to the conversation once the agent has replied', async () => {
    getSetupStatus.mockResolvedValue(setupStatus({ gateway: true, conversation: true }));
    renderSetup();
    await waitFor(() => expect(screen.getByText('setup.complete.title')).toBeInTheDocument());
    fireEvent.click(screen.getByRole('button', { name: /setup.step.conversation/ }));
    await waitFor(() =>
      expect(screen.getByRole('link', { name: 'setup.conversation.openSession' })).toHaveAttribute(
        'href',
        '/sessions/extgate-session-1',
      ),
    );
  });
});
