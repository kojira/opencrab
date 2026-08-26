import { useEffect, useRef, useState, type FormEvent, type ReactNode } from 'react';
import { Link, useParams } from 'react-router-dom';
import { useTranslation } from 'react-i18next';
import {
  conversationEventsUrl,
  getSession,
  getSessionLogs,
  sendWebMessage,
  ConversationSendError,
} from '../api/sessions';
import type { SessionDto, SessionLogRow } from '../api/types';

type LoadKind = 'idle' | 'loading' | 'loaded-empty' | 'loaded' | 'error';
type SendPhase = 'idle' | 'submitting' | 'accepted' | 'responding';

interface LogMetadata {
  source?: string;
  user_name?: string;
  user_avatar_url?: string;
  [key: string]: unknown;
}

function parseLogMetadata(metadataJson: string | null): LogMetadata | null {
  if (!metadataJson) return null;
  try {
    return JSON.parse(metadataJson) as LogMetadata;
  } catch {
    return null;
  }
}

function SessionLogItem({
  logType,
  content,
  speakerId,
  metadataJson,
  pending,
}: {
  logType: string;
  content: string;
  speakerId: string | null;
  metadataJson: string | null;
  pending?: boolean;
}) {
  const [borderColor, icon, iconColor] = (() => {
    switch (logType) {
      case 'speech':
        return ['border-l-primary', 'chat_bubble', 'text-primary'];
      case 'inner_voice':
        return ['border-l-purple-500', 'psychology', 'text-purple-500'];
      case 'action':
        return ['border-l-tertiary', 'bolt', 'text-tertiary'];
      case 'system':
        return ['border-l-secondary', 'settings', 'text-secondary'];
      default:
        return ['border-l-outline', 'help', 'text-on-surface-variant'];
    }
  })();

  const meta = parseLogMetadata(metadataJson);
  const isDiscordUser = meta?.source === 'discord';
  const isDiscordResponse = meta?.source === 'discord_response';

  let speakerDisplay: ReactNode;
  if (isDiscordUser && meta) {
    speakerDisplay = (
      <div className="flex items-center gap-2">
        {meta.user_avatar_url ? (
          <img
            src={meta.user_avatar_url}
            alt={meta.user_name || ''}
            className="w-6 h-6 rounded-full"
          />
        ) : (
          <span className={`material-symbols-outlined text-lg ${iconColor}`}>{icon}</span>
        )}
        <span className="text-label-lg text-on-surface">
          {meta.user_name || speakerId || ''}
        </span>
      </div>
    );
  } else if (isDiscordResponse && speakerId) {
    const initial = speakerId.charAt(0).toUpperCase();
    speakerDisplay = (
      <div className="flex items-center gap-2">
        <div className="w-6 h-6 rounded-full bg-primary-container flex items-center justify-center">
          <span className="text-xs text-primary font-medium">{initial}</span>
        </div>
        <span className="text-label-lg text-on-surface">{speakerId}</span>
      </div>
    );
  } else {
    speakerDisplay = (
      <div className="flex items-center gap-2">
        <span className={`material-symbols-outlined text-lg ${iconColor}`}>{icon}</span>
        <span className="text-label-lg text-on-surface">{speakerId || ''}</span>
      </div>
    );
  }

  return (
    <div className={`bg-surface-container rounded-lg border-l-4 ${borderColor} p-4`}>
      <div className="flex items-center justify-between mb-2">
        {speakerDisplay}
        <div className="flex items-center gap-2">
          {pending ? (
            <span className="material-symbols-outlined text-sm animate-spin" aria-live="polite">
              progress_activity
            </span>
          ) : null}
          <span className="badge-neutral text-label-sm">{logType}</span>
        </div>
      </div>
      <p className="text-body-lg text-on-surface whitespace-pre-wrap break-words pl-8">{content}</p>
    </div>
  );
}

function ErrorPanel({
  endpoint,
  message,
  onRetry,
}: {
  endpoint: string;
  message: string;
  onRetry: () => void;
}) {
  const { t } = useTranslation();
  return (
    <div className="card-outlined border-error bg-error-container/30 p-4" role="alert">
      <div className="flex items-center gap-2">
        <span className="material-symbols-outlined text-error">error</span>
        <p className="text-body-lg text-error-on-container">
          {t('common.error', { message: `${endpoint}: ${message}` })}
        </p>
        <button type="button" className="btn-text" onClick={onRetry}>
          {t('common.retry')}
        </button>
      </div>
    </div>
  );
}

export default function SessionDetail() {
  const { t } = useTranslation();
  const { id } = useParams<{ id: string }>();
  const [sessionKind, setSessionKind] = useState<LoadKind>('idle');
  const [session, setSession] = useState<SessionDto | null>(null);
  const [sessionError, setSessionError] = useState('');
  const [logsKind, setLogsKind] = useState<LoadKind>('idle');
  const [logs, setLogs] = useState<SessionLogRow[]>([]);
  const [logsError, setLogsError] = useState('');
  const [hasOlder, setHasOlder] = useState(false);
  const [ownerInput, setOwnerInput] = useState('');
  const [sendPhase, setSendPhase] = useState<SendPhase>('idle');
  const [pendingText, setPendingText] = useState('');
  const [pendingId, setPendingId] = useState<string | null>(null);
  const [sendError, setSendError] = useState<string | null>(null);
  const [liveAgent, setLiveAgent] = useState<string | null>(null);
  const [noReply, setNoReply] = useState(false);
  const abortRef = useRef<AbortController | null>(null);
  const sourceRef = useRef<EventSource | null>(null);

  const loadSession = (sessionId: string) => {
    abortRef.current?.abort();
    const ac = new AbortController();
    abortRef.current = ac;
    setSessionKind('loading');
    setLogsKind('loading');
    getSession(sessionId, ac.signal)
      .then((s) => {
        if (ac.signal.aborted) return;
        setSession(s);
        setSessionKind('loaded');
      })
      .catch((e: Error) => {
        if (ac.signal.aborted) return;
        setSessionError(e.message);
        setSessionKind('error');
      });
    getSessionLogs(sessionId, { signal: ac.signal })
      .then((rows) => {
        if (ac.signal.aborted) return;
        setLogs(rows);
        setHasOlder(rows.length === 100);
        setLogsKind(rows.length === 0 ? 'loaded-empty' : 'loaded');
      })
      .catch((e: Error) => {
        if (ac.signal.aborted) return;
        setLogsError(e.message);
        setLogsKind('error');
      });
  };

  const refreshTail = (sessionId: string) => {
    getSessionLogs(sessionId).then((rows) => {
      setLogs(rows);
      setHasOlder(rows.length === 100);
      setLogsKind(rows.length === 0 ? 'loaded-empty' : 'loaded');
      setLiveAgent(null);
      setPendingText('');
      setPendingId(null);
    });
  };

  const loadOlder = () => {
    if (!id || logs.length === 0 || logs[0].id == null) return;
    const ac = new AbortController();
    getSessionLogs(id, { before: String(logs[0].id), signal: ac.signal }).then((older) => {
      setLogs((cur) => [...older, ...cur]);
      setHasOlder(older.length === 100);
    });
  };

  useEffect(() => {
    if (!id) return;
    loadSession(id);
    return () => {
      abortRef.current?.abort();
      sourceRef.current?.close();
    };
  }, [id]);

  useEffect(() => {
    if (!id || sessionKind !== 'loaded' || logsKind === 'loading' || logsKind === 'idle') return;
    sourceRef.current?.close();
    const es = new EventSource(conversationEventsUrl(id));
    sourceRef.current = es;
    es.addEventListener('activity', (ev) => {
      const data = JSON.parse((ev as MessageEvent).data) as { state?: string };
      if (data.state === 'started') {
        setSendPhase((p) => (p === 'idle' ? 'responding' : p === 'submitting' || p === 'accepted' ? 'responding' : p));
      }
      if (data.state === 'ended') {
        if (liveAgent === null && sendPhase !== 'idle') {
          setSendPhase('idle');
          setPendingId(null);
          refreshTail(id);
        } else {
          setSendPhase('idle');
        }
      }
    });
    es.addEventListener('message', (ev) => {
      const data = JSON.parse((ev as MessageEvent).data) as { text?: string };
      if (data.text) setLiveAgent(data.text);
      setSendPhase('idle');
      refreshTail(id);
    });
    es.addEventListener('completed_no_reply', () => {
      setSendPhase('idle');
      setPendingId(null);
      setLiveAgent(null);
      setNoReply(true);
      refreshTail(id);
    });
    es.addEventListener('error', (ev) => {
      try {
        const data = JSON.parse((ev as MessageEvent).data || '{}') as { code?: string };
        if (data.code) {
          setSendError(data.code);
          setSendPhase('idle');
        }
      } catch {
        // EventSource 接続断。プロトコル error ではない。
      }
    });
    return () => {
      es.close();
    };
  }, [id, sessionKind, logsKind]);

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    if (!id || !ownerInput.trim() || sendPhase === 'submitting') return;
    const text = ownerInput.trim();
    const clientId = pendingId ?? crypto.randomUUID().toLowerCase();
    setPendingId(clientId);
    setPendingText(text);
    setSendPhase('submitting');
    setSendError(null);
    setNoReply(false);
    try {
      await sendWebMessage(id, clientId, text);
      setSendPhase('accepted');
      setOwnerInput('');
    } catch (err) {
      const code = err instanceof ConversationSendError ? err.code : (err as Error).message;
      setSendError(code);
      setSendPhase('idle');
    }
  };

  const retrySend = async () => {
    if (!id || !pendingId || !pendingText) return;
    setSendPhase('submitting');
    setSendError(null);
    try {
      await sendWebMessage(id, pendingId, pendingText);
      setSendPhase('accepted');
    } catch (err) {
      const code = err instanceof ConversationSendError ? err.code : (err as Error).message;
      setSendError(code);
      setSendPhase('idle');
    }
  };

  const badgeClass = session
    ? session.status === 'active'
      ? 'badge-success'
      : session.status === 'completed'
        ? 'badge-info'
        : session.status === 'paused'
          ? 'badge-warning'
          : 'badge-neutral'
    : '';

  const visibleLogs = logs.length > 200 ? logs.slice(logs.length - 200) : logs;
  const bound = session?.gateway_bound === true;

  return (
    <div className="max-w-4xl mx-auto h-full flex flex-col">
      {sessionKind === 'loading' ? (
        <div className="card-elevated mb-4" aria-busy="true">
          <p className="text-body-lg text-on-surface-variant">{t('sessionDetail.loadingSession')}</p>
        </div>
      ) : sessionKind === 'error' ? (
        <ErrorPanel endpoint="GET /api/sessions/{id}" message={sessionError} onRetry={() => id && loadSession(id)} />
      ) : session ? (
        <div className="card-elevated mb-4">
          <div className="flex items-center justify-between gap-2 flex-wrap">
            <div className="flex items-center gap-4 min-w-0">
              <Link to="/sessions" className="btn-text p-1 flex items-center gap-1 shrink-0 whitespace-nowrap">
                <span className="material-symbols-outlined">arrow_back</span>
                <span className="text-sm hidden sm:inline">{t('sessions.backToList')}</span>
              </Link>
              <div className="min-w-0">
                <h1 className="text-title-lg text-on-surface break-words">{session.theme}</h1>
                <div className="flex items-center gap-2 flex-wrap text-body-sm text-on-surface-variant mt-0.5">
                  <span>{t('sessionDetail.mode', { value: session.mode })}</span>
                  <span>{t('sessionDetail.phase', { value: session.phase })}</span>
                  <span>{t('sessionDetail.turn', { value: session.turn_number })}</span>
                </div>
              </div>
            </div>
            <span className={badgeClass}>{session.status}</span>
          </div>
        </div>
      ) : null}

      <div className="flex-1 overflow-y-auto space-y-2 mb-4">
        {logsKind === 'loading' ? (
          <div className="empty-state" aria-busy="true">
            <p className="text-body-lg text-on-surface-variant">{t('sessionDetail.loadingLogs')}</p>
          </div>
        ) : logsKind === 'error' ? (
          <ErrorPanel endpoint="GET /api/sessions/{id}/logs" message={logsError} onRetry={() => id && loadSession(id)} />
        ) : logsKind === 'loaded-empty' && !pendingText ? (
          <div className="empty-state">
            <span className="material-symbols-outlined empty-state-icon">chat</span>
            <p className="empty-state-text">{t('sessionDetail.noLogs')}</p>
          </div>
        ) : logsKind === 'loaded' || pendingText ? (
          <>
            {hasOlder ? (
              <button type="button" className="btn-text" onClick={loadOlder}>
                {t('sessions.loadMore')}
              </button>
            ) : null}
            {visibleLogs.map((log) => (
              <SessionLogItem
                key={log.id}
                logType={log.log_type}
                content={log.content}
                speakerId={log.speaker_id}
                metadataJson={log.metadata_json}
              />
            ))}
            {pendingText ? (
              <SessionLogItem
                logType="speech"
                content={pendingText}
                speakerId="web-user"
                metadataJson={null}
                pending={sendPhase === 'submitting' || sendPhase === 'accepted' || sendPhase === 'responding'}
              />
            ) : null}
            {sendPhase === 'responding' && !liveAgent ? (
              <p className="text-body-sm text-on-surface-variant" aria-live="polite">
                {t('sessionDetail.responding')}
              </p>
            ) : null}
            {liveAgent ? (
              <SessionLogItem logType="speech" content={liveAgent} speakerId="agent" metadataJson={null} />
            ) : null}
            {noReply ? (
              <p className="text-body-sm text-on-surface-variant" aria-live="polite">
                {t('sessionDetail.noReply')}
              </p>
            ) : null}
          </>
        ) : null}
      </div>

      {sendError ? (
        <ErrorPanel
          endpoint="POST /api/web-conversations/{session_id}/messages"
          message={sendError}
          onRetry={() => void retrySend()}
        />
      ) : null}

      <div className="card-elevated">
        {!bound ? (
          <p className="text-body-lg text-on-surface-variant" role="status">
            {t('sessionDetail.unbound')}
          </p>
        ) : (
          <form className="flex gap-3" onSubmit={(e) => void submit(e)}>
            <input
              type="text"
              className="input-outlined flex-1"
              placeholder={t('sessionDetail.ownerPlaceholder')}
              value={ownerInput}
              onChange={(e) => setOwnerInput(e.target.value)}
              disabled={sendPhase === 'submitting'}
            />
            <button type="submit" className="btn-filled" disabled={sendPhase === 'submitting'}>
              <span className="material-symbols-outlined text-xl">send</span>
              {t('common.send')}
            </button>
          </form>
        )}
      </div>
    </div>
  );
}
