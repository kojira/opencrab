import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, screen, waitFor, act, fireEvent } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { MemoryRouter, Route, Routes } from 'react-router-dom';
import type { SessionDto } from '../api/types';
import SessionDetail, { BINDING_POLL_MAX, BINDING_POLL_MS } from './SessionDetail';

const getSession = vi.fn();
const getSessionLogs = vi.fn();
const getWebConversationState = vi.fn();
const sendWebMessage = vi.fn();
const sendOwnerInstruction = vi.fn();
const getAgent = vi.fn();

vi.mock('../api/sessions', async () => {
  const actual = await vi.importActual<typeof import('../api/sessions')>('../api/sessions');
  return {
    ...actual,
    getSession: (...args: unknown[]) => getSession(...args),
    getSessionLogs: (...args: unknown[]) => getSessionLogs(...args),
    getWebConversationState: (...args: unknown[]) => getWebConversationState(...args),
    sendWebMessage: (...args: unknown[]) => sendWebMessage(...args),
    sendOwnerInstruction: (...args: unknown[]) => sendOwnerInstruction(...args),
  };
});

vi.mock('../api/agents', () => ({
  getAgent: (...args: unknown[]) => getAgent(...args),
}));

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

function dto(_state: 'ready' | 'provisioning' | 'unavailable', id = SESSION_ID): SessionDto {
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
  getWebConversationState.mockReset();
  getWebConversationState.mockResolvedValue('ready');
  sendWebMessage.mockReset();
  sendOwnerInstruction.mockReset();
  getAgent.mockReset();
  getAgent.mockResolvedValue({
    id: 'agent-1',
    name: 'Kurabu Agent',
    persona_name: 'くらぶ',
  });
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
  it('uses gateway ownership status instead of server session projection', async () => {
    getSession.mockResolvedValue({
      id: PHYSICAL_ID,
      mode: 'solo',
      theme: PHYSICAL_ID,
      phase: 'main',
      turn_number: 0,
      status: 'active',
      participant_count: 1,
      agent_ids: ['agent-1'],
      metadata_json: null,
    });
    getSessionLogs.mockResolvedValue([]);
    renderDetail(PHYSICAL_ID);

    await waitFor(() => {
      expect(screen.getByPlaceholderText('sessionDetail.ownerPlaceholder')).toBeEnabled();
    });
    expect(getWebConversationState).toHaveBeenCalledWith(PHYSICAL_ID);
    expect(FakeEventSource.instances[0].url).toBe(
      `/api/web-conversations/${PHYSICAL_ID}/events`,
    );
  });

  it('shows unnamed title and disables composer while provisioning', async () => {
    getWebConversationState.mockResolvedValue('provisioning');
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
    getWebConversationState
      .mockResolvedValueOnce('provisioning')
      .mockResolvedValue('ready');
    getSession.mockResolvedValue(dto('provisioning'));
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
    expect(getWebConversationState.mock.calls.length).toBeGreaterThanOrEqual(2);
    expect(sendWebMessage).not.toHaveBeenCalled();
  });

  it('shows retry after 60s timeout without confusing empty/ready', async () => {
    vi.useFakeTimers();
    getWebConversationState.mockResolvedValue('provisioning');
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
    getWebConversationState
      .mockResolvedValueOnce('provisioning')
      .mockRejectedValueOnce(new Error('detail-read-failed'));
    getSession.mockResolvedValue(dto('provisioning'));
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
    getWebConversationState.mockResolvedValue('provisioning');
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
    getWebConversationState.mockResolvedValue(null);
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
    getWebConversationState.mockResolvedValue(null);
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

  it('opens and posts with the gateway-owned physical session id', async () => {
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
      `/api/web-conversations/${PHYSICAL_ID}/events`,
    );
    const user = userEvent.setup();
    await user.type(screen.getByPlaceholderText('sessionDetail.ownerPlaceholder'), 'from-physical');
    await user.click(screen.getByRole('button', { name: /common.send/ }));
    await waitFor(() => {
      expect(sendWebMessage).toHaveBeenCalledWith(
        PHYSICAL_ID,
        expect.stringMatching(UUID_V4),
        'from-physical',
      );
    });
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

  it('does not clear optimistic speech when SSE wins the persistence race', async () => {
    getSession.mockResolvedValue(dto('ready'));
    getSessionLogs.mockResolvedValue([]);
    sendWebMessage.mockResolvedValue({
      client_message_id: 'aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa',
      origin: 'web:aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa',
      seq: 2,
      state: 'accepted',
    });
    renderDetail();
    await waitFor(() => {
      expect(screen.getByPlaceholderText('sessionDetail.ownerPlaceholder')).toBeEnabled();
    });
    const input = screen.getByPlaceholderText('sessionDetail.ownerPlaceholder');
    fireEvent.change(input, { target: { value: '消えない投稿' } });
    fireEvent.submit(input.closest('form')!);
    await waitFor(() => expect(sendWebMessage).toHaveBeenCalledTimes(1));

    await act(async () => {
      FakeEventSource.instances[0].emit('message', { text: '先に届いた応答' });
      await Promise.resolve();
    });

    expect(screen.getByText('消えない投稿')).toBeInTheDocument();
    expect(screen.getByText('先に届いた応答')).toBeInTheDocument();
  });

  it('keeps optimistic speech visible and settles from persisted logs when SSE is missed', async () => {
    vi.useFakeTimers();
    const userSpeech = {
      id: 10,
      agent_id: 'agent-1',
      session_id: SESSION_ID,
      log_type: 'speech',
      content: 'もしもし？',
      speaker_id: 'web-qc-human',
      turn_number: 2,
      metadata_json: '{"external_origin":"web:aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"}',
      created_at: '2026-09-13T05:47:14Z',
    };
    const agentSpeech = {
      id: 12,
      agent_id: 'agent-1',
      session_id: SESSION_ID,
      log_type: 'speech',
      content: 'はい、届いています。',
      speaker_id: 'agent-1',
      turn_number: 2,
      metadata_json: null,
      created_at: '2026-09-13T05:47:18Z',
    };
    getSession.mockResolvedValue(dto('ready'));
    getSessionLogs
      .mockResolvedValueOnce([])
      .mockResolvedValueOnce([userSpeech])
      .mockResolvedValue([userSpeech, agentSpeech]);
    sendWebMessage.mockResolvedValue({
      client_message_id: 'aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa',
      origin: 'web:aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa',
      seq: 2,
      state: 'accepted',
    });
    renderDetail();
    await act(async () => {
      await Promise.resolve();
    });
    const input = screen.getByPlaceholderText('sessionDetail.ownerPlaceholder');
    fireEvent.change(input, { target: { value: 'もしもし？' } });
    fireEvent.submit(input.closest('form')!);
    await act(async () => {
      await Promise.resolve();
    });
    expect(screen.getAllByText('もしもし？')).toHaveLength(1);
    expect(screen.getByTestId('session-pending-spinner')).toBeInTheDocument();

    await act(async () => {
      await vi.advanceTimersByTimeAsync(1000);
    });
    expect(getSessionLogs).toHaveBeenCalledTimes(2);
    expect(screen.getAllByText('もしもし？')).toHaveLength(1);
    expect(screen.getByTestId('session-pending-spinner')).toBeInTheDocument();
    expect(screen.queryByText('はい、届いています。')).not.toBeInTheDocument();

    await act(async () => {
      await vi.advanceTimersByTimeAsync(2000);
    });
    expect(getSessionLogs.mock.calls.length).toBeGreaterThanOrEqual(3);
    expect(screen.getAllByText('もしもし？')).toHaveLength(1);
    expect(screen.getByText('はい、届いています。')).toBeInTheDocument();
    expect(screen.queryByTestId('session-pending-spinner')).not.toBeInTheDocument();
  });

  it('does not expose web internals while gateway ownership is unresolved', async () => {
    let resolveOwnership!: (state: 'ready') => void;
    getWebConversationState.mockReturnValue(
      new Promise<'ready'>((resolve) => {
        resolveOwnership = resolve;
      }),
    );
    getSession.mockResolvedValue(dto('ready'));
    getSessionLogs.mockResolvedValue([
      {
        id: 1,
        agent_id: 'agent-1',
        session_id: SESSION_ID,
        log_type: 'speech',
        content: 'pending-user-message',
        speaker_id: 'web-qc-human',
        turn_number: 1,
        metadata_json: null,
        created_at: '2026-09-12T00:00:00Z',
      },
      {
        id: 2,
        agent_id: 'agent-1',
        session_id: SESSION_ID,
        log_type: 'system',
        content: '{"type":"turn_terminated","marker":"NO_REPLY"}',
        speaker_id: null,
        turn_number: null,
        metadata_json: null,
        created_at: '2026-09-12T00:00:01Z',
      },
      {
        id: 3,
        agent_id: 'agent-1',
        session_id: SESSION_ID,
        log_type: 'system',
        content: '{"type":"turn_exhausted","reason":"iteration_limit","iterations":31}',
        speaker_id: null,
        turn_number: null,
        metadata_json: null,
        created_at: '2026-09-12T00:00:02Z',
      },
    ]);

    renderDetail();

    await waitFor(() => expect(getSessionLogs).toHaveBeenCalled());
    expect(screen.queryByText('web-qc-human')).not.toBeInTheDocument();
    expect(screen.queryByText(/turn_terminated/)).not.toBeInTheDocument();
    expect(screen.queryByText(/NO_REPLY/)).not.toBeInTheDocument();
    expect(screen.queryByText(/turn_exhausted/)).not.toBeInTheDocument();
    expect(screen.queryByText(/iteration_limit/)).not.toBeInTheDocument();

    await act(async () => resolveOwnership('ready'));
    expect(await screen.findByText('sessionDetail.you')).toBeInTheDocument();
    expect(screen.getByText('pending-user-message')).toBeInTheDocument();
    expect(screen.getByRole('alert')).toHaveTextContent(
      'sessionDetail.responseIncomplete',
    );
  });

  it('hides normal termination internals and labels human and agent speech', async () => {
    getSession.mockResolvedValue(dto('ready'));
    getSessionLogs.mockResolvedValue([
      {
        id: 1,
        agent_id: 'agent-1',
        session_id: SESSION_ID,
        log_type: 'speech',
        content: 'テスト',
        speaker_id: 'web-qc-human',
        turn_number: 1,
        metadata_json: null,
        created_at: '2026-09-12T00:00:00Z',
      },
      {
        id: 2,
        agent_id: 'agent-1',
        session_id: SESSION_ID,
        log_type: 'speech',
        content: 'こんにちは！',
        speaker_id: 'agent-1',
        turn_number: 1,
        metadata_json: null,
        created_at: '2026-09-12T00:00:01Z',
      },
      {
        id: 3,
        agent_id: 'agent-1',
        session_id: SESSION_ID,
        log_type: 'system',
        content: '{"type":"turn_terminated","marker":"NO_REPLY"}',
        speaker_id: null,
        turn_number: null,
        metadata_json: null,
        created_at: '2026-09-12T00:00:02Z',
      },
    ]);

    renderDetail();

    expect(await screen.findByText('sessionDetail.you')).toBeInTheDocument();
    expect(await screen.findByText('くらぶ')).toBeInTheDocument();
    expect(screen.getByText('テスト')).toBeInTheDocument();
    expect(screen.getByText('こんにちは！')).toBeInTheDocument();
    expect(screen.queryByText('web-qc-human')).not.toBeInTheDocument();
    expect(screen.queryByText('agent-1')).not.toBeInTheDocument();
    expect(screen.queryByText(/turn_terminated/)).not.toBeInTheDocument();
    expect(screen.queryByText(/NO_REPLY/)).not.toBeInTheDocument();
  });

  it('collapses agent-owned tool logs by default and expands their preserved details', async () => {
    getSession.mockResolvedValue(dto('ready'));
    getSessionLogs.mockResolvedValue([
      {
        id: 4,
        agent_id: 'agent-1',
        session_id: SESSION_ID,
        log_type: 'tool_call',
        content: 'execute_shell',
        speaker_id: 'agent-1',
        turn_number: 1,
        metadata_json: null,
        created_at: '2026-09-12T00:00:03Z',
      },
      {
        id: 5,
        agent_id: 'agent-1',
        session_id: SESSION_ID,
        log_type: 'tool_result',
        content: '{"status":"spawned"}',
        speaker_id: 'agent-1',
        turn_number: 1,
        metadata_json: null,
        created_at: '2026-09-12T00:00:04Z',
      },
      {
        id: 6,
        agent_id: 'agent-1',
        session_id: SESSION_ID,
        log_type: 'speech',
        content: '完了しました',
        speaker_id: 'agent-1',
        turn_number: 1,
        metadata_json: null,
        created_at: '2026-09-12T00:00:05Z',
      },
    ]);

    renderDetail();

    expect(await screen.findAllByText('くらぶ')).toHaveLength(3);
    expect(screen.queryByText('agent-1')).not.toBeInTheDocument();
    const details = screen.getAllByTestId('session-tool-log');
    expect(details).toHaveLength(2);
    expect(details[0]).not.toHaveAttribute('open');
    expect(details[1]).not.toHaveAttribute('open');
    expect(screen.getByText('execute_shell')).not.toBeVisible();
    expect(screen.getByText('{"status":"spawned"}')).not.toBeVisible();
    expect(screen.getByText('tool_call')).toBeVisible();
    expect(screen.getByText('tool_result')).toBeVisible();

    const firstSummary = details[0].querySelector('summary');
    expect(firstSummary).not.toBeNull();
    await userEvent.click(firstSummary!);
    expect(details[0]).toHaveAttribute('open');
    expect(screen.getByText('execute_shell')).toBeVisible();
    await userEvent.click(firstSummary!);
    expect(details[0]).not.toHaveAttribute('open');
    expect(screen.getByText('execute_shell')).not.toBeVisible();

    expect(screen.getByText('完了しました').closest('details')).toBeNull();
  });

  it('renders turn exhaustion as one human-facing warning, not raw JSON', async () => {
    getSession.mockResolvedValue(dto('ready'));
    getSessionLogs.mockResolvedValue([
      {
        id: 4,
        agent_id: 'agent-1',
        session_id: SESSION_ID,
        log_type: 'system',
        content: '{"type":"turn_exhausted","reason":"iteration_limit","iterations":31}',
        speaker_id: null,
        turn_number: null,
        metadata_json: null,
        created_at: '2026-09-12T00:00:03Z',
      },
    ]);

    renderDetail();

    expect(await screen.findByRole('alert')).toHaveTextContent(
      'sessionDetail.responseIncomplete',
    );
    expect(screen.getAllByRole('alert')).toHaveLength(1);
    expect(screen.queryByText(/turn_exhausted/)).not.toBeInTheDocument();
    expect(screen.queryByText(/iteration_limit/)).not.toBeInTheDocument();
    expect(screen.queryByText(/31/)).not.toBeInTheDocument();
  });
});
