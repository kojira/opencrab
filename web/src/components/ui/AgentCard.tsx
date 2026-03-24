import { Link } from 'react-router-dom';
import { useTranslation } from 'react-i18next';
import type { AgentSummary } from '../../api/types';

interface Props {
  agent: AgentSummary;
}

export default function AgentCard({ agent }: Props) {
  const { t } = useTranslation();

  const badgeClass =
    agent.status === 'active'
      ? 'badge-success'
      : agent.status === 'error'
        ? 'badge-error'
        : 'badge-neutral';

  const statusIcon =
    agent.status === 'active'
      ? 'check_circle'
      : agent.status === 'error'
        ? 'error'
        : 'schedule';

  const firstChar = agent.name.charAt(0) || '?';

  return (
    <Link to={`/agents/${agent.id}`} className="card-elevated block group hover:border-primary/40">
      <div className="flex items-center gap-3 mb-3">
        {agent.image_url ? (
          <img
            className="w-10 h-10 rounded-xl object-cover shadow-elevation-1"
            src={agent.image_url}
            alt={agent.name}
          />
        ) : (
          <div className="w-10 h-10 rounded-xl bg-gradient-to-br from-primary-container to-primary/20 flex items-center justify-center shadow-elevation-1">
            <span className="text-title-md text-primary font-bold">
              {firstChar}
            </span>
          </div>
        )}

        <div className="flex-1 min-w-0">
          <h3 className="text-title-sm text-on-surface group-hover:text-primary transition-colors truncate font-semibold">
            {agent.name}
          </h3>
          <p className="text-body-sm text-on-surface-variant truncate">
            {agent.persona_name}
          </p>
        </div>

        <span className={`${badgeClass} shrink-0`}>
          <span className="material-symbols-outlined text-sm mr-0.5">
            {statusIcon}
          </span>
          {t('agentStatus.' + agent.status, { defaultValue: agent.status })}
        </span>
      </div>

      <div className="flex items-center gap-4 pt-3 border-t border-outline-variant/40">
        <div className="flex items-center gap-1.5 text-body-sm text-on-surface-variant">
          <span className="material-symbols-outlined text-base text-primary/60">
            psychology
          </span>
          <span>{t('agentCard.skills', { count: agent.skill_count })}</span>
        </div>
        <div className="flex items-center gap-1.5 text-body-sm text-on-surface-variant">
          <span className="material-symbols-outlined text-base text-tertiary/60">forum</span>
          <span>{t('agentCard.sessions', { count: agent.session_count })}</span>
        </div>
        <span className="material-symbols-outlined text-on-surface-variant/30 group-hover:text-primary/50 ml-auto transition-colors">arrow_forward</span>
      </div>
    </Link>
  );
}
