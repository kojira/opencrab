import { useState, useEffect } from 'react';
import { useTranslation } from 'react-i18next';
import { getSessions } from '../api/sessions';
import type { SessionDto } from '../api/types';
import SessionCard from '../components/ui/SessionCard';

export default function Sessions() {
  const { t } = useTranslation();
  const [sessions, setSessions] = useState<SessionDto[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [statusFilter, setStatusFilter] = useState<'all' | 'active' | 'completed'>('all');

  useEffect(() => {
    getSessions()
      .then(setSessions)
      .catch((e: Error) => setError(e.message));
  }, []);

  const filteredSessions = sessions
    ? statusFilter === 'all'
      ? sessions
      : sessions.filter((s) => s.status === statusFilter)
    : null;

  return (
    <div className="max-w-7xl mx-auto">
      <div className="flex items-center justify-between mb-4 flex-wrap gap-2">
        <div className="flex items-center gap-3">
          <h1 className="page-title">{t('sessions.title')}</h1>
          {filteredSessions !== null && (
            <span className="badge-neutral text-label-sm">
              {t('sessions.count', { count: filteredSessions.length })}
            </span>
          )}
        </div>
        <div className="flex items-center gap-2">
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

      {error ? (
        <div className="card-outlined border-error bg-error-container/30 p-4">
          <div className="flex items-center gap-2">
            <span className="material-symbols-outlined text-error">error</span>
            <p className="text-body-lg text-error-on-container">
              {t('common.error', { message: error })}
            </p>
          </div>
        </div>
      ) : filteredSessions === null ? (
        <div className="empty-state">
          <p className="text-body-lg text-on-surface-variant">{t('common.loading')}</p>
        </div>
      ) : filteredSessions.length === 0 ? (
        <div className="empty-state">
          <span className="material-symbols-outlined empty-state-icon">forum</span>
          <p className="empty-state-text">{t('sessions.noSessions')}</p>
        </div>
      ) : (
        <div className="space-y-3">
          {filteredSessions.map((session) => (
            <SessionCard key={session.id} session={session} />
          ))}
        </div>
      )}
    </div>
  );
}
