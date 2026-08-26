import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { getSessions } from '../api/sessions';
import type { SessionDto } from '../api/types';
import { useAgentContext } from '../hooks/useAgentContext';
import SessionCard from '../components/ui/SessionCard';

type LoadKind = 'idle' | 'loading' | 'loaded-empty' | 'loaded' | 'error';

export default function AgentSessions() {
  const { t } = useTranslation();
  const { agentId } = useAgentContext();
  const [kind, setKind] = useState<LoadKind>('idle');
  const [sessions, setSessions] = useState<SessionDto[]>([]);
  const [error, setError] = useState('');
  const abortRef = useRef<AbortController | null>(null);

  const load = () => {
    abortRef.current?.abort();
    const ac = new AbortController();
    abortRef.current = ac;
    setKind('loading');
    getSessions({ signal: ac.signal })
      .then((all) => {
        if (ac.signal.aborted) return;
        const mine = all.filter((s) => s.agent_ids.includes(agentId));
        setSessions(mine);
        setKind(mine.length === 0 ? 'loaded-empty' : 'loaded');
      })
      .catch((e: Error) => {
        if (ac.signal.aborted) return;
        setError(e.message);
        setKind('error');
      });
  };

  useEffect(() => {
    load();
    return () => abortRef.current?.abort();
  }, [agentId]);

  if (kind === 'error') {
    return (
      <div className="card-outlined border-error bg-error-container/30 p-4" role="alert">
        <div className="flex items-center gap-2">
          <span className="material-symbols-outlined text-error">error</span>
          <p className="text-body-lg text-error-on-container">
            {t('common.error', { message: `GET /api/sessions: ${error}` })}
          </p>
          <button type="button" className="btn-text" onClick={load}>
            {t('common.retry')}
          </button>
        </div>
      </div>
    );
  }

  if (kind === 'loading' || kind === 'idle') {
    return (
      <div className="empty-state" aria-busy="true">
        <p className="text-body-lg text-on-surface-variant">{t('common.loading')}</p>
      </div>
    );
  }

  if (kind === 'loaded-empty') {
    return (
      <div className="empty-state">
        <span className="material-symbols-outlined empty-state-icon">forum</span>
        <p className="empty-state-text">{t('agentSessions.noSessions')}</p>
      </div>
    );
  }

  return (
    <div className="space-y-3">
      {sessions.map((session) => (
        <SessionCard key={session.id} session={session} />
      ))}
    </div>
  );
}
