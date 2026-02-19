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
      <div className="card-elevated mb-6">
        <div className="flex items-center gap-5">
          <div className="w-16 h-16 rounded-full bg-primary-container flex items-center justify-center">
            <span className="text-headline-sm text-primary-on-container font-semibold">
              {agent.name.charAt(0) || '?'}
            </span>
          </div>
          <div className="flex-1 min-w-0">
            <h1 className="text-headline-sm text-on-surface font-medium truncate">
              {agent.name}
            </h1>
            <p className="text-body-lg text-on-surface-variant">
              {agent.persona_name} / {agent.role}
            </p>
          </div>
          <div className="flex items-center gap-2">
            <Link to={`/agents/${id}/edit`} className="btn-tonal">
              <span className="material-symbols-outlined text-xl">edit</span>
              {t('common.edit')}
            </Link>
            <button
              className="btn-outlined border-error text-error hover:bg-error-container/30"
              onClick={() => setShowDeleteConfirm(true)}
            >
              <span className="material-symbols-outlined text-xl">delete</span>
              {t('common.delete')}
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
        <div className="flex border-b border-outline-variant mb-6 gap-1">
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
                className={`flex items-center gap-1.5 px-4 py-3 text-label-lg border-b-2 transition-colors ${
                  active
                    ? 'border-primary text-primary'
                    : 'border-transparent text-on-surface-variant hover:text-on-surface hover:bg-surface-container'
                }`}
              >
                <span className="material-symbols-outlined text-lg">{tab.icon}</span>
                {t(tab.labelKey)}
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
