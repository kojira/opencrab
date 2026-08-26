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
    image_url: null,
    status: 'active',
    skill_count: 3,
    session_count: 2,
  },
  {
    id: 'a2',
    name: 'Bob',
    persona_name: 'Analytical',
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
    agent_ids: ['a1', 'a2'],
    metadata_json: null,
    gateway_bound: false,
  },
  {
    id: 's2',
    mode: 'debate',
    theme: 'Ethics',
    phase: 'ended',
    turn_number: 10,
    status: 'completed',
    participant_count: 3,
    agent_ids: ['a1', 'a2', 'a3'],
    metadata_json: null,
    gateway_bound: false,
  },
  {
    id: 's3',
    mode: 'discussion',
    theme: 'Science',
    phase: 'main',
    turn_number: 1,
    status: 'active',
    participant_count: 2,
    agent_ids: ['a1', 'a3'],
    metadata_json: null,
    gateway_bound: false,
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
    expect(screen.getByText('home.title')).toBeInTheDocument();
  });

  // 現在の Home はスタットバー（ラベル+コロン+数値）+ エージェント/セッション
  // プレビューという構成。旧クイックリンク UI を前提とした期待値から更新した。
  it('displays agent and session counts after loading', async () => {
    mockedGetAgents.mockResolvedValue(fakeAgents);
    mockedGetSessions.mockResolvedValue(fakeSessions);
    renderHome();

    await waitFor(() => {
      expect(screen.getByText('home.totalAgents:').parentElement).toHaveTextContent('2');
    });
    expect(screen.getByText('home.totalSessions:').parentElement).toHaveTextContent('3');
    expect(screen.getByText('home.activeSessions:').parentElement).toHaveTextContent('2');
  });

  it('renders agent preview and recent sessions after loading', async () => {
    mockedGetAgents.mockResolvedValue(fakeAgents);
    mockedGetSessions.mockResolvedValue(fakeSessions);
    renderHome();

    await waitFor(() => {
      expect(screen.getByText('Alice')).toBeInTheDocument();
    });
    expect(screen.getByText('Bob')).toBeInTheDocument();
    expect(screen.getByText('agents.newAgent')).toBeInTheDocument();
    // 直近セッションのプレビュー（SessionCard がテーマを表示する）
    expect(screen.getByText('AI')).toBeInTheDocument();
    expect(screen.getByText('Ethics')).toBeInTheDocument();
  });

  it('renders stat labels', () => {
    mockedGetAgents.mockResolvedValue([]);
    mockedGetSessions.mockResolvedValue([]);
    renderHome();

    expect(screen.getByText('home.totalAgents:')).toBeInTheDocument();
    expect(screen.getByText('home.totalSessions:')).toBeInTheDocument();
    expect(screen.getByText('home.activeSessions:')).toBeInTheDocument();
  });
});
