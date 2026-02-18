import { useState, useEffect } from 'react';
import { Link } from 'react-router-dom';
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
  const [agents, setAgents] = useState<AgentSummary[]>([]);
  const [sessions, setSessions] = useState<SessionDto[]>([]);

  useEffect(() => {
    getAgents().then(setAgents).catch(() => {});
    getSessions().then(setSessions).catch(() => {});
  }, []);

  const activeSessions = sessions.filter((s) => s.status === 'active').length;

  return (
    <div className="max-w-7xl mx-auto">
      <h1 className="page-title mb-8">Dashboard</h1>

      <div className="grid grid-cols-1 md:grid-cols-3 gap-6 mb-8">
        <StatCard
          icon="smart_toy"
          iconBg="bg-primary-container"
          iconColor="text-primary"
          label="Total Agents"
          value={String(agents.length)}
        />
        <StatCard
          icon="forum"
          iconBg="bg-tertiary-container"
          iconColor="text-tertiary"
          label="Total Sessions"
          value={String(sessions.length)}
        />
        <StatCard
          icon="stream"
          iconBg="bg-success-container"
          iconColor="text-success"
          label="Active Sessions"
          value={String(activeSessions)}
        />
      </div>

      <h2 className="section-title">Quick Actions</h2>
      <div className="grid grid-cols-1 md:grid-cols-2 gap-4">
        <QuickLink
          to="/agents"
          icon="smart_toy"
          title="Agent Management"
          description="Create, configure, and manage autonomous agents"
        />
        <QuickLink
          to="/sessions"
          icon="forum"
          title="Session Monitor"
          description="Watch real-time conversations and agent interactions"
        />
        <QuickLink
          to="/memory"
          icon="memory"
          title="Memory Explorer"
          description="Browse and search agent memories and session logs"
        />
        <QuickLink
          to="/analytics"
          icon="analytics"
          title="Analytics & Metrics"
          description="LLM costs, quality scores, and usage analytics"
        />
      </div>
    </div>
  );
}
