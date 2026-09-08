import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { getAgents } from '../api/agents';
import { getSessions } from '../api/sessions';
import type { AgentSummary, SessionDto } from '../api/types';
import NewConversationButton from '../components/ui/NewConversationButton';
import SessionCard from '../components/ui/SessionCard';

type LoadKind = 'idle' | 'loading' | 'loaded-empty' | 'loaded' | 'error';

export default function Sessions() {
  const { t } = useTranslation();
  const [kind, setKind] = useState<LoadKind>('idle');
  const [sessions, setSessions] = useState<SessionDto[]>([]);
  const [error, setError] = useState('');
  const [hasMore, setHasMore] = useState(false);
  const [statusFilter, setStatusFilter] = useState<'all' | 'active' | 'completed'>('all');
  const [agents, setAgents] = useState<AgentSummary[]>([]);
  const [agentFilter, setAgentFilter] = useState('');
  const abortRef = useRef<AbortController | null>(null);

  const load = (reset: boolean) => {
    abortRef.current?.abort();
    const ac = new AbortController();
    abortRef.current = ac;
    if (reset) {
      setKind('loading');
      setSessions([]);
    }
    const before = reset || sessions.length === 0 ? undefined : sessions[sessions.length - 1]?.id;
    getSessions({ signal: ac.signal, before: reset ? undefined : before })
      .then((page) => {
        if (ac.signal.aborted) return;
        setSessions((cur) => (reset ? page : [...cur, ...page]));
        setHasMore(page.length === 100);
        const nextLen = reset ? page.length : sessions.length + page.length;
        setKind(nextLen === 0 ? 'loaded-empty' : 'loaded');
      })
      .catch((e: Error) => {
        if (ac.signal.aborted) return;
        setError(e.message);
        setKind('error');
      });
  };

  useEffect(() => {
    load(true);
    return () => abortRef.current?.abort();
  }, [statusFilter]);

  useEffect(() => {
    getAgents().then(setAgents).catch(() => setAgents([]));
  }, []);

  const filtered = sessions.filter((s) => {
    if (statusFilter !== 'all' && s.status !== statusFilter) return false;
    if (agentFilter && !s.agent_ids.includes(agentFilter)) return false;
    return true;
  });

  return (
    <div className="max-w-7xl mx-auto">
      <div className="flex items-center justify-between mb-4 flex-wrap gap-2">
        <div className="flex items-center gap-3">
          <h1 className="page-title">{t('sessions.title')}</h1>
          {kind === 'loaded' && (
            <span className="badge-neutral text-label-sm">
              {t('sessions.count', { count: filtered.length })}
            </span>
          )}
        </div>
        <div className="flex items-center gap-2">
          <select
            className="input-outlined py-1.5"
            value={agentFilter}
            onChange={(e) => setAgentFilter(e.target.value)}
            aria-label={t('common.selectAgent')}
          >
            <option value="">{t('common.selectAgentPlaceholder')}</option>
            {agents.map((a) => (
              <option key={a.id} value={a.id}>
                {a.name}
              </option>
            ))}
          </select>
          <NewConversationButton agentId={agentFilter || null} />
          {(['all', 'active', 'completed'] as const).map((f) => (
            <button
              key={f}
              onClick={() => setStatusFilter(f)}
              className={`px-3 py-1.5 rounded-full text-sm transition-colors ${
                statusFilter === f
                  ? 'bg-primary text-on-primary'
                  : 'bg-surface-container text-on-surface-variant hover:bg-surface-container-high'
              }`}
            >
              {t(`sessions.filter${f.charAt(0).toUpperCase() + f.slice(1)}`)}
            </button>
          ))}
        </div>
      </div>

      {kind === 'error' ? (
        <div className="card-outlined border-error bg-error-container/30 p-4" role="alert">
          <div className="flex items-center gap-2">
            <span className="material-symbols-outlined text-error">error</span>
            <p className="text-body-lg text-error-on-container">
              {t('common.error', { message: `GET /api/sessions: ${error}` })}
            </p>
            <button type="button" className="btn-text" onClick={() => load(true)}>
              {t('common.retry')}
            </button>
          </div>
        </div>
      ) : kind === 'loading' ? (
        <div className="empty-state" aria-busy="true">
          <p className="text-body-lg text-on-surface-variant">{t('common.loading')}</p>
        </div>
      ) : kind === 'loaded-empty' ? (
        <div className="empty-state">
          <span className="material-symbols-outlined empty-state-icon">forum</span>
          <p className="empty-state-text">{t('sessions.noSessions')}</p>
        </div>
      ) : (
        <div className="space-y-3">
          {filtered.map((session) => (
            <SessionCard key={session.id} session={session} />
          ))}
          {hasMore ? (
            <button type="button" className="btn-text" onClick={() => load(false)}>
              {t('sessions.loadMore')}
            </button>
          ) : null}
        </div>
      )}
    </div>
  );
}
