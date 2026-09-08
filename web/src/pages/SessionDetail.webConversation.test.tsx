import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, screen, waitFor, act } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { MemoryRouter, Route, Routes } from 'react-router-dom';
import type { SessionDto } from '../api/types';
import SessionDetail, { BINDING_POLL_MAX, BINDING_POLL_MS } from './SessionDetail';

const getSession = vi.fn();
const getSessionLogs = vi.fn();
const sendWebMessage = vi.fn();
const sendOwnerInstruction = vi.fn();

vi.mock('../api/sessions', async () => {
  const actual = await vi.importActual<typeof import('../api/sessions')>('../api/sessions');
  return {
    ...actual,
    getSession: (...args: unknown[]) => getSession(...args),
    getSessionLogs: (...args: unknown[]) => getSessionLogs(...args),
    sendWebMessage: (...args: unknown[]) => sendWebMessage(...args),
    sendOwnerInstruction: (...args: unknown[]) => sendOwnerInstruction(...args),
  };
});

class FakeEventSource {
  static instances: FakeEventSource[] = [];
  onerror: (() => void) | null = null;
  private listeners = new Map<string, Array<(ev: MessageEvent) => void>>();
  constructor(public url: string) {
    FakeEventSource.instances.push(this);
  }
  addEventListener(type: string, handler: EventListenerOrEventListenerObject) {
    const fn =
      typeof handler === 'function' ? handler : handler.handleEvent.bind(handler);
    const list = this.listeners.get(type) ?? [];
    list.push(fn as (ev: MessageEvent) => void);
    this.listeners.set(type, list);
  }
  emit(type: string, data: unknown) {
    const ev = { data: JSON.stringify(data) } as MessageEvent;
    for (const h of this.listeners.get(type) ?? []) h(ev);
  }
  close() {}
}

const SESSION_ID = 'web-agent-1-cccccccc-cccc-4ccc-8ccc-cccccccccccc';
const PHYSICAL_ID = 'extgate-bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb';
const UUID_V4 = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;
const originalRandomUUID = crypto.randomUUID;

function dto(state: SessionDto['web_binding_state'], id = SESSION_ID): SessionDto {
  return {
    id,
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
    binding_address: SESSION_ID,
  };
}

function renderDetail(pathId = SESSION_ID) {
  return render(
    <MemoryRouter initialEntries={[`/sessions/${pathId}`]}>
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
  sendOwnerInstruction.mockReset();
  FakeEventSource.instances = [];
  vi.stubGlobal('EventSource', FakeEventSource);
});

afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
  Object.defineProperty(crypto, 'randomUUID', {
    configurable: true,
    writable: true,
    value: originalRandomUUID,
  });
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

  it('does not attach SSE or unbound chrome on intake sessions', async () => {
    getSession.mockResolvedValue({
      id: 'intake-1',
      mode: 'intake',
      theme: 'mail',
      phase: 'main',
      turn_number: 0,
      status: 'active',
      participant_count: 1,
      agent_ids: ['agent-1'],
      metadata_json: null,
      gateway_bound: false,
    });
    getSessionLogs.mockResolvedValue([]);
    render(
      <MemoryRouter initialEntries={['/sessions/intake-1']}>
        <Routes>
          <Route path="/sessions/:id" element={<SessionDetail />} />
        </Routes>
      </MemoryRouter>,
    );
    await waitFor(() => {
      expect(screen.getByPlaceholderText('sessionDetail.ownerPlaceholder')).toBeInTheDocument();
    });
    expect(screen.queryByText('sessionDetail.unbound')).not.toBeInTheDocument();
    expect(screen.queryByText('sessionDetail.bindingPreparing')).not.toBeInTheDocument();
    expect(screen.queryByText('sse_disconnected')).not.toBeInTheDocument();
    expect(FakeEventSource.instances).toHaveLength(0);
    expect(sendWebMessage).not.toHaveBeenCalled();
  });

  it('sends owner instruction on intake and never opens web-conversation SSE', async () => {
    getSession.mockResolvedValue({
      id: 'intake-1',
      mode: 'intake',
      theme: 'mail',
      phase: 'main',
      turn_number: 0,
      status: 'active',
      participant_count: 1,
      agent_ids: ['agent-1'],
      metadata_json: null,
      gateway_bound: false,
    });
    getSessionLogs.mockResolvedValue([]);
    sendOwnerInstruction.mockResolvedValue({ id: 1 });
    render(
      <MemoryRouter initialEntries={['/sessions/intake-1']}>
        <Routes>
          <Route path="/sessions/:id" element={<SessionDetail />} />
        </Routes>
      </MemoryRouter>,
    );
    await waitFor(() => {
      expect(screen.getByPlaceholderText('sessionDetail.ownerPlaceholder')).toBeInTheDocument();
    });
    const user = userEvent.setup();
    await user.type(screen.getByPlaceholderText('sessionDetail.ownerPlaceholder'), 'please look');
    await user.click(screen.getByRole('button', { name: /common.send/ }));
    await waitFor(() => {
      expect(sendOwnerInstruction).toHaveBeenCalledWith('intake-1', 'please look');
    });
    expect(sendWebMessage).not.toHaveBeenCalled();
    expect(FakeEventSource.instances).toHaveLength(0);
  });

  it('connects SSE only after a web conversation is ready', async () => {
    getSession.mockResolvedValue(dto('ready'));
    getSessionLogs.mockResolvedValue([]);
    renderDetail();
    await waitFor(() => {
      expect(screen.getByPlaceholderText('sessionDetail.ownerPlaceholder')).toBeEnabled();
    });
    expect(FakeEventSource.instances).toHaveLength(1);
    expect(FakeEventSource.instances[0].url).toBe(
      `/api/web-conversations/${SESSION_ID}/events`,
    );
  });

  it('sends successfully when randomUUID is undefined (non-secure context)', async () => {
    Object.defineProperty(crypto, 'randomUUID', {
      configurable: true,
      writable: true,
      value: undefined,
    });
    expect(crypto.randomUUID).toBeUndefined();
    getSession.mockResolvedValue(dto('ready'));
    getSessionLogs.mockResolvedValue([]);
    sendWebMessage.mockResolvedValue({
      client_message_id: 'aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa',
      origin: 'web:aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa',
      seq: 1,
      state: 'accepted',
    });
    renderDetail();
    await waitFor(() => {
      expect(screen.getByPlaceholderText('sessionDetail.ownerPlaceholder')).toBeEnabled();
    });
    const user = userEvent.setup();
    await user.type(screen.getByPlaceholderText('sessionDetail.ownerPlaceholder'), 'hello');
    await user.click(screen.getByRole('button', { name: /common.send/ }));
    await waitFor(() => {
      expect(sendWebMessage).toHaveBeenCalledTimes(1);
    });
    const [sessionId, clientId, text] = sendWebMessage.mock.calls[0] as [string, string, string];
    expect(sessionId).toBe(SESSION_ID);
    expect(clientId).toMatch(UUID_V4);
    expect(text).toBe('hello');
    expect(screen.queryByRole('alert')).not.toBeInTheDocument();
  });

  it('opens with physical id, posts to binding address, and shows say', async () => {
    getSession.mockResolvedValue(dto('ready', PHYSICAL_ID));
    getSessionLogs
      .mockResolvedValueOnce([])
      .mockResolvedValue([
        {
          id: 1,
          agent_id: 'agent-1',
          session_id: PHYSICAL_ID,
          log_type: 'speech',
          content: 'agent-say',
          speaker_id: 'agent',
          turn_number: 1,
          metadata_json: null,
          created_at: '2026-08-27T00:00:00Z',
        },
      ]);
    sendWebMessage.mockResolvedValue({
      client_message_id: 'aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa',
      origin: 'web:aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa',
      seq: 1,
      state: 'accepted',
    });
    renderDetail(PHYSICAL_ID);
    await waitFor(() => {
      expect(screen.getByPlaceholderText('sessionDetail.ownerPlaceholder')).toBeEnabled();
    });
    expect(FakeEventSource.instances).toHaveLength(1);
    expect(FakeEventSource.instances[0].url).toBe(
      `/api/web-conversations/${SESSION_ID}/events`,
    );
    const user = userEvent.setup();
    await user.type(screen.getByPlaceholderText('sessionDetail.ownerPlaceholder'), 'from-physical');
    await user.click(screen.getByRole('button', { name: /common.send/ }));
    await waitFor(() => {
      expect(sendWebMessage).toHaveBeenCalledWith(
        SESSION_ID,
        expect.stringMatching(UUID_V4),
        'from-physical',
      );
    });
    expect(sendWebMessage).not.toHaveBeenCalledWith(
      PHYSICAL_ID,
      expect.anything(),
      expect.anything(),
    );
    await act(async () => {
      FakeEventSource.instances[0].emit('message', { text: 'agent-say' });
    });
    await waitFor(() => {
      expect(screen.getByText('agent-say')).toBeInTheDocument();
    });
  });

  it('shows sendError when send throws synchronously', async () => {
    getSession.mockResolvedValue(dto('ready'));
    getSessionLogs.mockResolvedValue([]);
    sendWebMessage.mockImplementation(() => {
      throw new Error('sync-boom');
    });
    renderDetail();
    await waitFor(() => {
      expect(screen.getByPlaceholderText('sessionDetail.ownerPlaceholder')).toBeEnabled();
    });
    const user = userEvent.setup();
    await user.type(screen.getByPlaceholderText('sessionDetail.ownerPlaceholder'), 'will-fail');
    await user.click(screen.getByRole('button', { name: /common.send/ }));
    await waitFor(() => {
      expect(screen.getByRole('alert')).toBeInTheDocument();
    });
    expect(screen.getByText('will-fail')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'common.retry' })).toBeInTheDocument();
    expect(sendWebMessage).toHaveBeenCalled();
  });
});
