import { useState, useEffect } from 'react';
import { Link } from 'react-router-dom';
import { useTranslation } from 'react-i18next';
import { getAgents } from '../api/agents';
import type { AgentSummary } from '../api/types';
import AgentCard from '../components/ui/AgentCard';

export default function Agents() {
  const { t } = useTranslation();
  const [agents, setAgents] = useState<AgentSummary[] | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    getAgents()
      .then(setAgents)
      .catch((e: Error) => setError(e.message));
  }, []);

  return (
    <div className="max-w-7xl mx-auto">
      <div className="flex items-center justify-between mb-6">
        <h1 className="page-title">{t('agents.title')}</h1>
        <Link to="/agents/new" className="btn-filled">
          <span className="material-symbols-outlined text-xl">add</span>
          {t('agents.newAgent')}
        </Link>
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
      ) : agents === null ? (
        <div className="empty-state">
          <p className="text-body-lg text-on-surface-variant">{t('common.loading')}</p>
        </div>
      ) : agents.length === 0 ? (
        <div className="empty-state">
          <span className="material-symbols-outlined empty-state-icon">
            smart_toy
          </span>
          <p className="empty-state-text">{t('agents.noAgents')}</p>
          <p className="text-body-sm text-on-surface-variant mt-2">
            {t('agents.createFirstAgent')}
          </p>
        </div>
      ) : (
        <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-6">
          {agents.map((agent) => (
            <AgentCard key={agent.id} agent={agent} />
          ))}
        </div>
      )}
    </div>
  );
}
