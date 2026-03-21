import { useState, useEffect } from 'react';
import { Link, Outlet, useParams, useLocation, useNavigate } from 'react-router-dom';
import { useTranslation } from 'react-i18next';
import { getAgent, deleteAgent } from '../../api/agents';
import type { AgentDetail } from '../../api/types';
import ConfirmDialog from '../ui/ConfirmDialog';

const tabs = [
  { key: 'overview', path: '', icon: 'info', labelKey: 'agentNav.overview' },
  { key: 'skills', path: '/skills', icon: 'psychology', labelKey: 'agentNav.skills' },
  { key: 'memory', path: '/memory', icon: 'memory', labelKey: 'agentNav.memory' },
  { key: 'sessions', path: '/sessions', icon: 'forum', labelKey: 'agentNav.sessions' },
  { key: 'co-agents', path: '/co-agents', icon: 'group', labelKey: 'agentNav.coAgents' },
  { key: 'trusted-users', path: '/trusted-users', icon: 'shield_person', labelKey: 'agentNav.trustedUsers' },
  { key: 'channels', path: '/channels', icon: 'tag', labelKey: 'agentNav.channels' },
  { key: 'allowed-commands', path: '/allowed-commands', icon: 'terminal', labelKey: 'agentNav.allowedCommands' },
  { key: 'llm-logs', path: '/llm-logs', icon: 'receipt_long', labelKey: 'agentNav.llmLogs' },
  { key: 'analytics', path: '/analytics', icon: 'analytics', labelKey: 'agentNav.analytics' },
];

export default function AgentLayout() {
  const { t } = useTranslation();
  const { id } = useParams<{ id: string }>();
  const location = useLocation();
  const navigate = useNavigate();
  const [agent, setAgent] = useState<AgentDetail | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [showDeleteConfirm, setShowDeleteConfirm] = useState(false);

  useEffect(() => {
    if (!id) return;
    getAgent(id)
      .then(setAgent)
      .catch((e: Error) => setError(e.message));
  }, [id]);

  const handleDelete = async () => {
    if (!id) return;
    const res = await deleteAgent(id);
    if (res.deleted) {
      navigate('/agents');
    }
  };

  // Hide tab bar on edit/persona sub-routes
  const basePath = `/agents/${id}`;
  const isEditRoute =
    location.pathname === `${basePath}/edit` ||
    location.pathname === `${basePath}/persona`;

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

  if (!agent) {
    return (
      <div className="empty-state">
        <p className="text-body-lg text-on-surface-variant">{t('common.loading')}</p>
      </div>
    );
  }

  return (
    <div className="max-w-4xl mx-auto">
      {/* Breadcrumb */}
      <nav className="flex items-center gap-1.5 text-body-sm text-on-surface-variant mb-4">
        <Link to="/agents" className="hover:text-primary transition-colors">
          {t('nav.agents')}
        </Link>
        <span className="material-symbols-outlined text-sm">chevron_right</span>
        <span className="text-on-surface">{agent.name}</span>
      </nav>

      {/* Agent header card */}
      <div className="rounded-xl bg-gradient-to-r from-primary-container/60 to-surface-container border border-primary/20 p-3 mb-4 shadow-elevation-1">
        <div className="flex items-center gap-1.5">
          <div className="w-8 h-8 rounded-xl bg-gradient-to-br from-primary to-primary/70 flex items-center justify-center shadow-elevation-2 shrink-0">
            <span className="text-xs text-white font-bold">
              {agent.name.charAt(0) || '?'}
            </span>
          </div>
          <div className="flex-1 min-w-0">
            <h1 className="text-sm font-semibold text-on-surface truncate">
              {agent.name}
            </h1>
            <p className="text-xs text-on-surface-variant truncate">
              {agent.persona_name}
            </p>
          </div>
          <div className="flex items-center gap-1 shrink-0">
            <Link to={`/agents/${id}/edit`} className="btn-tonal p-1.5">
              <span className="material-symbols-outlined text-xl">edit</span>
            </Link>
            <button
              className="btn-outlined border-error text-error hover:bg-error-container/30 p-1.5"
              onClick={() => setShowDeleteConfirm(true)}
            >
              <span className="material-symbols-outlined text-xl">delete</span>
            </button>
          </div>
        </div>
      </div>

      {showDeleteConfirm && (
        <ConfirmDialog
          title={t('agentDetail.deleteTitle')}
          message={t('agentDetail.deleteMessage')}
          onConfirm={handleDelete}
          onCancel={() => setShowDeleteConfirm(false)}
        />
      )}

      {/* Tab navigation */}
      {!isEditRoute && (
        <div className="flex overflow-x-auto flex-nowrap mb-6 gap-0.5 bg-surface-container-high rounded-xl p-1">
          {tabs.map((tab) => {
            const tabPath = `${basePath}${tab.path}`;
            const active =
              tab.path === ''
                ? location.pathname === basePath
                : location.pathname.startsWith(tabPath);
            return (
              <Link
                key={tab.key}
                to={tabPath}
                className={`flex flex-col sm:flex-row items-center gap-0.5 sm:gap-1.5 px-2 sm:px-3 py-2 text-label-sm sm:text-label-lg rounded-lg transition-all duration-200 whitespace-nowrap flex-shrink-0 ${
                  active
                    ? 'bg-surface-container shadow-elevation-1 text-primary font-semibold'
                    : 'text-on-surface-variant hover:text-on-surface hover:bg-surface-container/60'
                }`}
              >
                <span className="material-symbols-outlined text-xl">{tab.icon}</span>
                <span className="hidden sm:inline">{t(tab.labelKey)}</span>
              </Link>
            );
          })}
        </div>
      )}

      {/* Sub-page content */}
      <Outlet context={{ agent, agentId: id! }} />
    </div>
  );
}
