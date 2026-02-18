import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, screen, waitFor } from '@testing-library/react';
import { MemoryRouter } from 'react-router-dom';

vi.mock('../api/agents', () => ({
  getAgents: vi.fn(),
}));
vi.mock('../api/sessions', () => ({
  getSessions: vi.fn(),
}));

import { getAgents } from '../api/agents';
import { getSessions } from '../api/sessions';
import Home from './Home';
import type { AgentSummary, SessionDto } from '../api/types';

const mockedGetAgents = vi.mocked(getAgents);
const mockedGetSessions = vi.mocked(getSessions);

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

const fakeSessions: SessionDto[] = [
  {
    id: 's1',
    mode: 'discussion',
    theme: 'AI',
    phase: 'main',
    turn_number: 3,
    status: 'active',
    participant_count: 2,
  },
  {
    id: 's2',
    mode: 'debate',
    theme: 'Ethics',
    phase: 'ended',
    turn_number: 10,
    status: 'completed',
    participant_count: 3,
  },
  {
    id: 's3',
    mode: 'discussion',
    theme: 'Science',
    phase: 'main',
    turn_number: 1,
    status: 'active',
    participant_count: 2,
  },
];

beforeEach(() => {
  mockedGetAgents.mockReset();
  mockedGetSessions.mockReset();
});

function renderHome() {
  return render(
    <MemoryRouter>
      <Home />
    </MemoryRouter>,
  );
}

describe('Home', () => {
  it('renders the Dashboard heading', () => {
    mockedGetAgents.mockResolvedValue([]);
    mockedGetSessions.mockResolvedValue([]);
    renderHome();
    expect(screen.getByText('Dashboard')).toBeInTheDocument();
  });

  it('displays agent and session counts after loading', async () => {
    mockedGetAgents.mockResolvedValue(fakeAgents);
    mockedGetSessions.mockResolvedValue(fakeSessions);
    renderHome();

    await waitFor(() => {
      expect(screen.getByText('Total Agents').parentElement).toHaveTextContent('2');
    });
    expect(screen.getByText('Total Sessions').parentElement).toHaveTextContent('3');
    expect(screen.getByText('Active Sessions').parentElement).toHaveTextContent('2');
  });

  it('renders 4 quick action links', () => {
    mockedGetAgents.mockResolvedValue([]);
    mockedGetSessions.mockResolvedValue([]);
    renderHome();

    expect(screen.getByText('Agent Management')).toBeInTheDocument();
    expect(screen.getByText('Session Monitor')).toBeInTheDocument();
    expect(screen.getByText('Memory Explorer')).toBeInTheDocument();
    expect(screen.getByText('Analytics & Metrics')).toBeInTheDocument();
  });

  it('renders stat labels', () => {
    mockedGetAgents.mockResolvedValue([]);
    mockedGetSessions.mockResolvedValue([]);
    renderHome();

    expect(screen.getByText('Total Agents')).toBeInTheDocument();
    expect(screen.getByText('Total Sessions')).toBeInTheDocument();
    expect(screen.getByText('Active Sessions')).toBeInTheDocument();
  });
});
