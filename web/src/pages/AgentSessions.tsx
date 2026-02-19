import { useState, useEffect } from 'react';
import { useTranslation } from 'react-i18next';
import { getSessions } from '../api/sessions';
import type { SessionDto } from '../api/types';
import { useAgentContext } from '../hooks/useAgentContext';
import SessionCard from '../components/ui/SessionCard';

export default function AgentSessions() {
  const { t } = useTranslation();
  const { agentId } = useAgentContext();
  const [sessions, setSessions] = useState<SessionDto[] | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    getSessions()
      .then((all) => setSessions(all.filter((s) => s.agent_ids.includes(agentId))))
      .catch((e: Error) => setError(e.message));
  }, [agentId]);

  if (error) {
    return (
      <div className="card-outlined border-error bg-error-container/30 p-4">
        <div className="flex items-center gap-2">
          <span className="material-symbols-outlined text-error">error</span>
          <p className="text-body-lg text-error-on-container">
            {t('common.error', { message: error })}
          </p>
        </div>
      </div>
    );
  }

  if (sessions === null) {
    return (
      <div className="empty-state">
        <p className="text-body-lg text-on-surface-variant">{t('common.loading')}</p>
      </div>
    );
  }

  if (sessions.length === 0) {
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
