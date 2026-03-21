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
  gradient,
}: {
  icon: string;
  iconBg: string;
  iconColor: string;
  label: string;
  value: string;
  gradient?: string;
}) {
  return (
    <div className={`stat-card ${gradient || "bg-surface-container"}`}>
      <div className="flex items-center justify-between mb-3">
        <div
          className={`w-11 h-11 rounded-xl ${iconBg} flex items-center justify-center shadow-elevation-1`}
        >
          <span className={`material-symbols-outlined text-2xl ${iconColor}`}>
            {icon}
          </span>
        </div>
        <span className={`material-symbols-outlined text-4xl opacity-10 ${iconColor}`}>
          {icon}
        </span>
      </div>
      <p className="text-body-md text-on-surface-variant mb-1">{label}</p>
      <p className="text-headline-md text-on-surface font-bold">{value}</p>
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
    <Link to={to} className="card-elevated flex items-center gap-4 group hover:border-primary/40">
      <div className="w-11 h-11 rounded-xl bg-primary-container flex items-center justify-center shrink-0 group-hover:bg-primary group-hover:shadow-elevation-1 transition-all duration-200">
        <span className="material-symbols-outlined text-xl text-primary group-hover:text-primary-on transition-colors">
          {icon}
        </span>
      </div>
      <div className="min-w-0">
        <h3 className="text-title-sm text-on-surface group-hover:text-primary transition-colors font-semibold">
          {title}
        </h3>
        <p className="text-body-sm text-on-surface-variant leading-relaxed">{description}</p>
      </div>
      <span className="material-symbols-outlined text-on-surface-variant/40 group-hover:text-primary/60 ml-auto shrink-0 transition-colors">chevron_right</span>
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
    <div className="max-w-5xl mx-auto space-y-8">
      {/* Page header */}
      <div className="flex flex-col sm:flex-row sm:items-center sm:justify-between gap-3">
        <div>
          <h1 className="text-headline-md text-on-surface font-bold">{t("home.title")}</h1>
          <p className="text-body-md text-on-surface-variant mt-1">{t("home.subtitle", "Manage your AI agents and sessions")}</p>
        </div>
        <div className="flex items-center gap-2 px-3 py-1.5 rounded-full bg-success-container shrink-0 self-start sm:self-auto">
          <span className="w-2 h-2 rounded-full bg-success animate-pulse" />
          <span className="text-label-md text-success-on-container font-medium">{t("header.dbConnected")}</span>
        </div>
      </div>

      {/* Stats grid */}
      <div className="grid grid-cols-1 sm:grid-cols-3 gap-4">
        <StatCard
          icon="smart_toy"
          iconBg="bg-primary-container"
          iconColor="text-primary"
          label={t("home.totalAgents")}
          value={String(agents.length)}
        />
        <StatCard
          icon="forum"
          iconBg="bg-tertiary-container"
          iconColor="text-tertiary"
          label={t("home.totalSessions")}
          value={String(sessions.length)}
        />
        <StatCard
          icon="stream"
          iconBg="bg-success-container"
          iconColor="text-success"
          label={t("home.activeSessions")}
          value={String(activeSessions)}
        />
      </div>

      {/* Quick actions */}
      <div>
        <h2 className="section-title">{t("home.quickActions")}</h2>
        <div className="grid grid-cols-1 sm:grid-cols-2 gap-3">
          <QuickLink
            to="/agents"
            icon="smart_toy"
            title={t("home.agentManagement")}
            description={t("home.agentManagementDesc")}
          />
          <QuickLink
            to="/sessions"
            icon="forum"
            title={t("home.sessionMonitor")}
            description={t("home.sessionMonitorDesc")}
          />
          <QuickLink
            to="/agents"
            icon="memory"
            title={t("home.memoryExplorer")}
            description={t("home.memoryExplorerDesc")}
          />
          <QuickLink
            to="/agents"
            icon="analytics"
            title={t("home.analyticsMetrics")}
            description={t("home.analyticsMetricsDesc")}
          />
        </div>
      </div>
    </div>
  );
}
