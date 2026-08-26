import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, screen, waitFor, act } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { MemoryRouter, Route, Routes } from 'react-router-dom';
import type { SessionDto } from '../api/types';
import SessionDetail, { BINDING_POLL_MAX, BINDING_POLL_MS } from './SessionDetail';

const getSession = vi.fn();
const getSessionLogs = vi.fn();
const sendWebMessage = vi.fn();

vi.mock('../api/sessions', async () => {
  const actual = await vi.importActual<typeof import('../api/sessions')>('../api/sessions');
  return {
    ...actual,
    getSession: (...args: unknown[]) => getSession(...args),
    getSessionLogs: (...args: unknown[]) => getSessionLogs(...args),
    sendWebMessage: (...args: unknown[]) => sendWebMessage(...args),
  };
});

class FakeEventSource {
  static instances: FakeEventSource[] = [];
  onerror: (() => void) | null = null;
  constructor(public url: string) {
    FakeEventSource.instances.push(this);
  }
  addEventListener() {}
  close() {}
}

const SESSION_ID = 'web-agent-1-cccccccc-cccc-4ccc-8ccc-cccccccccccc';

function dto(state: SessionDto['web_binding_state']): SessionDto {
  return {
    id: SESSION_ID,
    mode: 'solo',
    theme: SESSION_ID,
    phase: 'main',
    turn_number: 0,
    status: 'active',
    participant_count: 1,
    agent_ids: ['agent-1'],
    metadata_json: null,
    gateway_bound: true,
    web_binding_state: state,
  };
}

function renderDetail() {
  return render(
    <MemoryRouter initialEntries={[`/sessions/${SESSION_ID}`]}>
      <Routes>
        <Route path="/sessions/:id" element={<SessionDetail />} />
      </Routes>
    </MemoryRouter>,
  );
}

beforeEach(() => {
  getSession.mockReset();
  getSessionLogs.mockReset();
  sendWebMessage.mockReset();
  FakeEventSource.instances = [];
  vi.stubGlobal('EventSource', FakeEventSource);
});

afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

describe('SessionDetail web conversation', () => {
  it('shows unnamed title and disables composer while provisioning', async () => {
    getSession.mockResolvedValue(dto('provisioning'));
    getSessionLogs.mockResolvedValue([]);
    renderDetail();
    await waitFor(() => {
      expect(screen.getByRole('heading', { name: 'sessions.newConversation' })).toBeInTheDocument();
    });
    expect(screen.getByRole('status')).toHaveTextContent('sessionDetail.bindingPreparing');
    expect(screen.queryByPlaceholderText('sessionDetail.ownerPlaceholder')).not.toBeInTheDocument();
    expect(FakeEventSource.instances).toHaveLength(0);
  });

  it('enables composer after a poll reaches ready', async () => {
    vi.useFakeTimers();
    getSession.mockResolvedValueOnce(dto('provisioning')).mockResolvedValue(dto('ready'));
    getSessionLogs.mockResolvedValue([]);
    renderDetail();
    await act(async () => {
      await Promise.resolve();
    });
    expect(screen.getByRole('status')).toHaveTextContent('sessionDetail.bindingPreparing');
    await act(async () => {
      await vi.advanceTimersByTimeAsync(BINDING_POLL_MS);
    });
    expect(screen.getByPlaceholderText('sessionDetail.ownerPlaceholder')).toBeEnabled();
    expect(getSession.mock.calls.length).toBeGreaterThanOrEqual(2);
    expect(sendWebMessage).not.toHaveBeenCalled();
  });

  it('shows retry after 60s timeout without confusing empty/ready', async () => {
    vi.useFakeTimers();
    getSession.mockResolvedValue(dto('provisioning'));
    getSessionLogs.mockResolvedValue([]);
    renderDetail();
    await act(async () => {
      await Promise.resolve();
    });
    await act(async () => {
      await vi.advanceTimersByTimeAsync(BINDING_POLL_MS * (BINDING_POLL_MAX + 1));
    });
    expect(screen.getByText('sessionDetail.bindingTimeout')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'common.retry' })).toBeInTheDocument();
    expect(screen.queryByPlaceholderText('sessionDetail.ownerPlaceholder')).not.toBeInTheDocument();
  });

  it('shows retry on detail poll error and does not treat it as empty', async () => {
    vi.useFakeTimers();
    getSession
      .mockResolvedValueOnce(dto('provisioning'))
      .mockRejectedValueOnce(new Error('detail-read-failed'));
    getSessionLogs.mockResolvedValue([]);
    renderDetail();
    await act(async () => {
      await Promise.resolve();
    });
    await act(async () => {
      await vi.advanceTimersByTimeAsync(BINDING_POLL_MS);
    });
    expect(screen.getByRole('alert')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'common.retry' })).toBeInTheDocument();
    expect(screen.getByRole('status')).toHaveTextContent('sessionDetail.bindingPreparing');
    expect(screen.queryByPlaceholderText('sessionDetail.ownerPlaceholder')).not.toBeInTheDocument();
  });

  it('does not auto-create a conversation from the detail page', async () => {
    getSession.mockResolvedValue(dto('provisioning'));
    getSessionLogs.mockResolvedValue([]);
    const fetchMock = vi.fn();
    vi.stubGlobal('fetch', fetchMock);
    renderDetail();
    await waitFor(() => {
      expect(screen.getByRole('status')).toBeInTheDocument();
    });
    expect(fetchMock).not.toHaveBeenCalled();
  });
});
