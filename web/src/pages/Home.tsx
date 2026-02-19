import { useState, useEffect } from 'react';
import { Link } from 'react-router-dom';
import { useTranslation } from 'react-i18next';
import { getAgents } from '../api/agents';
import { getSessions } from '../api/sessions';
import type { AgentSummary, SessionDto } from '../api/types';

function StatCard({
  icon,
  iconBg,
  iconColor,
  label,
  value,
}: {
  icon: string;
  iconBg: string;
  iconColor: string;
  label: string;
  value: string;
}) {
  return (
    <div className="card-elevated">
      <div className="flex items-center gap-4">
        <div
          className={`w-12 h-12 rounded-lg ${iconBg} flex items-center justify-center`}
        >
          <span className={`material-symbols-outlined text-2xl ${iconColor}`}>
            {icon}
          </span>
        </div>
        <div>
          <p className="text-body-md text-on-surface-variant">{label}</p>
          <p className="text-headline-md text-on-surface font-semibold">
            {value}
          </p>
        </div>
      </div>
    </div>
  );
}

function QuickLink({
  to,
  icon,
  title,
  description,
}: {
  to: string;
  icon: string;
  title: string;
  description: string;
}) {
  return (
    <Link to={to} className="card-elevated flex items-start gap-4 group">
      <div className="w-10 h-10 rounded-lg bg-primary-container flex items-center justify-center shrink-0 group-hover:bg-primary group-hover:text-primary-on transition-colors">
        <span className="material-symbols-outlined text-xl text-primary group-hover:text-primary-on transition-colors">
          {icon}
        </span>
      </div>
      <div>
        <h3 className="text-title-md text-on-surface group-hover:text-primary transition-colors mb-1">
          {title}
        </h3>
        <p className="text-body-md text-on-surface-variant">{description}</p>
      </div>
    </Link>
  );
}

export default function Home() {
  const { t } = useTranslation();
  const [agents, setAgents] = useState<AgentSummary[]>([]);
  const [sessions, setSessions] = useState<SessionDto[]>([]);

  useEffect(() => {
    getAgents().then(setAgents).catch(() => {});
    getSessions().then(setSessions).catch(() => {});
  }, []);

  const activeSessions = sessions.filter((s) => s.status === 'active').length;

  return (
    <div className="max-w-7xl mx-auto">
      <h1 className="page-title mb-8">{t('home.title')}</h1>

      <div className="grid grid-cols-1 md:grid-cols-3 gap-6 mb-8">
        <StatCard
          icon="smart_toy"
          iconBg="bg-primary-container"
          iconColor="text-primary"
          label={t('home.totalAgents')}
          value={String(agents.length)}
        />
        <StatCard
          icon="forum"
          iconBg="bg-tertiary-container"
          iconColor="text-tertiary"
          label={t('home.totalSessions')}
          value={String(sessions.length)}
        />
        <StatCard
          icon="stream"
          iconBg="bg-success-container"
          iconColor="text-success"
          label={t('home.activeSessions')}
          value={String(activeSessions)}
        />
      </div>

      <h2 className="section-title">{t('home.quickActions')}</h2>
      <div className="grid grid-cols-1 md:grid-cols-2 gap-4">
        <QuickLink
          to="/agents"
          icon="smart_toy"
          title={t('home.agentManagement')}
          description={t('home.agentManagementDesc')}
        />
        <QuickLink
          to="/sessions"
          icon="forum"
          title={t('home.sessionMonitor')}
          description={t('home.sessionMonitorDesc')}
        />
        <QuickLink
          to="/agents"
          icon="memory"
          title={t('home.memoryExplorer')}
          description={t('home.memoryExplorerDesc')}
        />
        <QuickLink
          to="/agents"
          icon="analytics"
          title={t('home.analyticsMetrics')}
          description={t('home.analyticsMetricsDesc')}
        />
      </div>
    </div>
  );
}
