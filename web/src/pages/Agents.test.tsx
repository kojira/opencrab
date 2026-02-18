import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, screen, waitFor } from '@testing-library/react';
import { MemoryRouter } from 'react-router-dom';

vi.mock('../api/agents', () => ({
  getAgents: vi.fn(),
}));

import { getAgents } from '../api/agents';
import Agents from './Agents';
import type { AgentSummary } from '../api/types';

const mockedGetAgents = vi.mocked(getAgents);

beforeEach(() => {
  mockedGetAgents.mockReset();
});

function renderAgents() {
  return render(
    <MemoryRouter>
      <Agents />
    </MemoryRouter>,
  );
}

const fakeAgents: AgentSummary[] = [
  {
    id: 'a1',
    name: 'Alice',
    persona_name: 'Curious',
    role: 'discussant',
    image_url: null,
    status: 'active',
    skill_count: 3,
    session_count: 2,
  },
  {
    id: 'a2',
    name: 'Bob',
    persona_name: 'Analytical',
    role: 'facilitator',
    image_url: null,
    status: 'idle',
    skill_count: 1,
    session_count: 0,
  },
];

describe('Agents', () => {
  it('shows loading state initially', () => {
    mockedGetAgents.mockReturnValue(new Promise(() => {}));
    renderAgents();
    expect(screen.getByText('common.loading')).toBeInTheDocument();
  });

  it('renders agent cards after loading', async () => {
    mockedGetAgents.mockResolvedValue(fakeAgents);
    renderAgents();

    await waitFor(() => {
      expect(screen.getByText('Alice')).toBeInTheDocument();
    });
    expect(screen.getByText('Bob')).toBeInTheDocument();
  });

  it('shows empty state when no agents exist', async () => {
    mockedGetAgents.mockResolvedValue([]);
    renderAgents();

    await waitFor(() => {
      expect(screen.getByText('agents.noAgents')).toBeInTheDocument();
    });
    expect(
      screen.getByText('agents.createFirstAgent'),
    ).toBeInTheDocument();
  });

  it('shows error message on failure', async () => {
    mockedGetAgents.mockRejectedValue(new Error('Server unreachable'));
    renderAgents();

    await waitFor(() => {
      expect(screen.getByText('common.error')).toBeInTheDocument();
    });
  });

  it('renders the New Agent button', () => {
    mockedGetAgents.mockReturnValue(new Promise(() => {}));
    renderAgents();
    expect(screen.getByText('agents.newAgent')).toBeInTheDocument();
  });

  it('renders the page title', () => {
    mockedGetAgents.mockReturnValue(new Promise(() => {}));
    renderAgents();
    expect(screen.getByText('agents.title')).toBeInTheDocument();
  });
});
