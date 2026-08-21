import { useState, useEffect } from 'react';
import { Link } from 'react-router-dom';
import { useTranslation } from 'react-i18next';
import { getAgents } from '../api/agents';
import { getSessions } from '../api/sessions';
import AgentCard from '../components/ui/AgentCard';
import SessionCard from '../components/ui/SessionCard';
import SetupChecklist from '../components/setup/SetupChecklist';
import type { AgentSummary, SessionDto } from '../api/types';

export default function Home() {
  const { t } = useTranslation();
  const [agents, setAgents] = useState<AgentSummary[]>([]);
  const [sessions, setSessions] = useState<SessionDto[]>([]);

  useEffect(() => {
    getAgents().then(setAgents).catch(() => {});
    getSessions().then(setSessions).catch(() => {});
  }, []);

  const activeSessions = sessions.filter((s) => s.status === 'active').length;
  const previewAgents = agents.slice(0, 5);
  const recentSessions = sessions.slice(0, 3);

  return (
    <div className="max-w-5xl mx-auto space-y-4">
      {/* Page header */}
      <div>
        <h1 className="text-xl text-on-surface font-bold">{t('home.title')}</h1>
        <p className="text-xs text-on-surface-variant mt-0.5">{t('home.subtitle')}</p>
      </div>

      {/* Onboarding checklist (未完なら導線、完了なら控えめ表示) */}
      <SetupChecklist />

      {/* Stat bar */}
      <div className="flex items-center gap-4 px-4 py-2.5 rounded-xl bg-surface-container border border-outline-variant/50 flex-wrap">
        <div className="flex items-center gap-1.5">
          <span className="material-symbols-outlined text-base text-primary">smart_toy</span>
          <span className="text-sm text-on-surface-variant">{t('home.totalAgents')}:</span>
          <span className="text-sm font-semibold text-on-surface">{agents.length}</span>
        </div>
        <div className="w-px h-4 bg-outline-variant hidden sm:block" />
        <div className="flex items-center gap-1.5">
          <span className="material-symbols-outlined text-base text-tertiary">forum</span>
          <span className="text-sm text-on-surface-variant">{t('home.totalSessions')}:</span>
          <span className="text-sm font-semibold text-on-surface">{sessions.length}</span>
        </div>
        <div className="w-px h-4 bg-outline-variant hidden sm:block" />
        <div className="flex items-center gap-1.5">
          <span className="material-symbols-outlined text-base text-success">stream</span>
          <span className="text-sm text-on-surface-variant">{t('home.activeSessions')}:</span>
          <span className="text-sm font-semibold text-on-surface">{activeSessions}</span>
        </div>
      </div>

      {/* Agent preview */}
      <div>
        <div className="flex items-center justify-between mb-3">
          <h2 className="text-lg font-semibold text-on-surface">{t('agents.title')}</h2>
          <Link to="/agents/new" className="btn-tonal text-sm py-1.5 px-3">
            <span className="material-symbols-outlined text-base">add</span>
            {t('agents.newAgent')}
          </Link>
        </div>
        {agents.length === 0 ? (
          <div className="card-outlined text-center py-6">
            <p className="text-sm text-on-surface-variant">{t('agents.noAgents')}</p>
          </div>
        ) : (
          <>
            <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-3">
              {previewAgents.map((agent) => (
                <AgentCard key={agent.id} agent={agent} />
              ))}
            </div>
            {agents.length > 5 && (
              <div className="mt-3 text-right">
                <Link to="/agents" className="btn-text text-sm">
                  {t('agents.title')} ({agents.length}) →
                </Link>
              </div>
            )}
          </>
        )}
      </div>

      {/* Recent sessions */}
      <div>
        <div className="flex items-center justify-between mb-3">
          <h2 className="text-lg font-semibold text-on-surface">{t('sessions.title')}</h2>
          <Link to="/sessions" className="btn-text text-sm">
            {t('sessions.title')} →
          </Link>
        </div>
        {sessions.length === 0 ? (
          <div className="card-outlined text-center py-6">
            <p className="text-sm text-on-surface-variant">{t('sessions.noSessions')}</p>
          </div>
        ) : (
          <>
            <div className="space-y-2">
              {recentSessions.map((session) => (
                <SessionCard key={session.id} session={session} />
              ))}
            </div>
            {sessions.length > 3 && (
              <div className="mt-3 text-right">
                <Link to="/sessions" className="btn-text text-sm">
                  {t('sessions.title')} ({sessions.length}) →
                </Link>
              </div>
            )}
          </>
        )}
      </div>
    </div>
  );
}
