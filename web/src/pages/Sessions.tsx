import { useState, useEffect } from 'react';
import { useTranslation } from 'react-i18next';
import { getSessions } from '../api/sessions';
import type { SessionDto } from '../api/types';
import SessionCard from '../components/ui/SessionCard';

export default function Sessions() {
  const { t } = useTranslation();
  const [sessions, setSessions] = useState<SessionDto[] | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    getSessions()
      .then(setSessions)
      .catch((e: Error) => setError(e.message));
  }, []);

  return (
    <div className="max-w-7xl mx-auto">
      <h1 className="page-title mb-6">{t('sessions.title')}</h1>

      {error ? (
        <div className="card-outlined border-error bg-error-container/30 p-4">
          <div className="flex items-center gap-2">
            <span className="material-symbols-outlined text-error">error</span>
            <p className="text-body-lg text-error-on-container">
              {t('common.error', { message: error })}
            </p>
          </div>
        </div>
      ) : sessions === null ? (
        <div className="empty-state">
          <p className="text-body-lg text-on-surface-variant">{t('common.loading')}</p>
        </div>
      ) : sessions.length === 0 ? (
        <div className="empty-state">
          <span className="material-symbols-outlined empty-state-icon">
            forum
          </span>
          <p className="empty-state-text">{t('sessions.noSessions')}</p>
        </div>
      ) : (
        <div className="space-y-3">
          {sessions.map((session) => (
            <SessionCard key={session.id} session={session} />
          ))}
        </div>
      )}
    </div>
  );
}
