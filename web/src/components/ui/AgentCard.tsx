import { Link } from 'react-router-dom';
import type { AgentSummary } from '../../api/types';

interface Props {
  agent: AgentSummary;
}

export default function AgentCard({ agent }: Props) {
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
    <Link to={`/agents/${agent.id}`} className="card-elevated block group">
      <div className="flex items-center gap-4 mb-4">
        {agent.image_url ? (
          <img
            className="w-12 h-12 rounded-full object-cover"
            src={agent.image_url}
            alt={agent.name}
          />
        ) : (
          <div className="w-12 h-12 rounded-full bg-primary-container flex items-center justify-center">
            <span className="text-title-md text-primary-on-container font-semibold">
              {firstChar}
            </span>
          </div>
        )}

        <div className="flex-1 min-w-0">
          <h3 className="text-title-md text-on-surface group-hover:text-primary transition-colors truncate">
            {agent.name}
          </h3>
          <p className="text-body-sm text-on-surface-variant truncate">
            {agent.persona_name}
          </p>
        </div>

        <span className={badgeClass}>
          <span className="material-symbols-outlined text-sm mr-0.5">
            {statusIcon}
          </span>
          {agent.status}
        </span>
      </div>

      <div className="flex items-center gap-4 pt-3 border-t border-outline-variant/50">
        <div className="flex items-center gap-1.5 text-body-sm text-on-surface-variant">
          <span className="material-symbols-outlined text-base">
            psychology
          </span>
          <span>{agent.skill_count} skills</span>
        </div>
        <div className="flex items-center gap-1.5 text-body-sm text-on-surface-variant">
          <span className="material-symbols-outlined text-base">forum</span>
          <span>{agent.session_count} sessions</span>
        </div>
      </div>
    </Link>
  );
}
