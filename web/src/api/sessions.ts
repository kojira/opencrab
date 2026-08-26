import { api } from './client';
import type { SessionRow, SessionDto, SessionLogRow } from './types';

export type LoadKind = 'idle' | 'loading' | 'loaded-empty' | 'loaded' | 'error';

function toSessionDto(s: SessionRow): SessionDto {
  const agentIds = Array.isArray(s.agent_ids) ? s.agent_ids : [];
  return {
    id: s.id,
    mode: s.mode,
    theme: s.theme,
    phase: s.phase,
    turn_number: s.turn_number,
    status: s.status,
    participant_count: agentIds.length,
    agent_ids: agentIds,
    metadata_json: s.metadata_json,
    gateway_bound: s.gateway_bound === true,
    web_binding_state: s.web_binding_state,
  };
}

export type CreatedConversation = {
  conversation_id: string;
  session_id: string;
  binding_id: string;
  name: string | null;
  state: 'ready' | 'provisioning';
  httpStatus: 201 | 202;
};

export class ConversationCreateError extends Error {
  readonly status: number;
  readonly code: string;
  constructor(status: number, code: string, message: string) {
    super(message);
    this.status = status;
    this.code = code;
  }
}

/** 新規 Web 会話の唯一の API client。browser は ID を採番しない。 */
export async function createWebConversation(
  agentId: string,
  name?: string,
): Promise<CreatedConversation> {
  const res = await fetch(`/api/agents/${agentId}/web-conversations`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: name === undefined ? '{}' : JSON.stringify({ name }),
  });
  const body = (await res.json().catch(() => ({}))) as {
    conversation_id?: string;
    session_id?: string;
    binding_id?: string;
    name?: string | null;
    state?: string;
    error?: string | { code?: string };
  };
  if (
    (res.status === 201 || res.status === 202) &&
    (body.state === 'ready' || body.state === 'provisioning') &&
    typeof body.conversation_id === 'string' &&
    typeof body.session_id === 'string' &&
    typeof body.binding_id === 'string'
  ) {
    return {
      conversation_id: body.conversation_id,
      session_id: body.session_id,
      binding_id: body.binding_id,
      name: body.name ?? null,
      state: body.state,
      httpStatus: res.status as 201 | 202,
    };
  }
  const code =
    typeof body.error === 'string'
      ? body.error
      : (body.error?.code ?? String(res.status));
  throw new ConversationCreateError(res.status, code, code);
}

export async function getSessions(
  opts: { limit?: number; before?: string; signal?: AbortSignal } = {},
): Promise<SessionDto[]> {
  const q = new URLSearchParams();
  q.set('limit', String(opts.limit ?? 100));
  if (opts.before) q.set('before', opts.before);
  const rows = await api.get<SessionRow[]>(`/sessions?${q}`, { signal: opts.signal });
  return rows.map(toSessionDto);
}

export async function getSession(id: string, signal?: AbortSignal): Promise<SessionDto> {
  const row = await api.get<SessionRow>(`/sessions/${id}`, { signal });
  return toSessionDto(row);
}

export function getSessionLogs(
  id: string,
  opts: { limit?: number; before?: string; signal?: AbortSignal } = {},
): Promise<SessionLogRow[]> {
  const q = new URLSearchParams();
  q.set('limit', String(opts.limit ?? 100));
  if (opts.before) q.set('before', opts.before);
  return api.get<SessionLogRow[]>(`/sessions/${id}/logs?${q}`, { signal: opts.signal });
}

/** 非 web セッションの従来 owner 指示。web-conversation POST/SSE には使わない。 */
export function sendOwnerInstruction(
  sessionId: string,
  content: string,
): Promise<{ id: number }> {
  return api.post(`/sessions/${sessionId}/owner`, { content });
}

export type SendAccepted = {
  client_message_id: string;
  origin: string;
  seq: number;
  state: 'accepted';
};

export class ConversationSendError extends Error {
  readonly status: number;
  readonly code: string;
  constructor(status: number, code: string, message: string) {
    super(message);
    this.status = status;
    this.code = code;
  }
}

export async function sendWebMessage(
  sessionId: string,
  clientMessageId: string,
  text: string,
): Promise<SendAccepted> {
  const res = await fetch(`/api/web-conversations/${sessionId}/messages`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({
      client_message_id: clientMessageId,
      text,
      attachments: [],
    }),
  });
  const body = (await res.json().catch(() => ({}))) as {
    state?: string;
    error?: { code?: string };
    client_message_id?: string;
    origin?: string;
    seq?: number;
  };
  if (
    res.status === 202 &&
    body.state === 'accepted' &&
    typeof body.client_message_id === 'string' &&
    typeof body.origin === 'string' &&
    typeof body.seq === 'number'
  ) {
    return {
      client_message_id: body.client_message_id,
      origin: body.origin,
      seq: body.seq,
      state: 'accepted',
    };
  }
  const code = body.error?.code ?? body.state ?? String(res.status);
  throw new ConversationSendError(res.status, code, code);
}

export function conversationEventsUrl(sessionId: string): string {
  return `/api/web-conversations/${sessionId}/events`;
}
