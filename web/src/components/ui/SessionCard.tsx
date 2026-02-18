import { Link } from 'react-router-dom';
import type { SessionDto } from '../../api/types';

interface Props {
  session: SessionDto;
}

export default function SessionCard({ session }: Props) {
  const badgeClass = session.status === 'active' ? 'badge-success' : session.status === 'completed' ? 'badge-info' : session.status === 'paused' ? 'badge-warning' : 'badge-neutral';
  const statusIcon = session.status === 'active' ? 'play_circle' : session.status === 'completed' ? 'check_circle' : session.status === 'paused' ? 'pause_circle' : 'help';

  return (
    <Link to={`/sessions/${session.id}`} className="card-elevated block group">
      <div className="flex items-center justify-between">
        <div className="flex items-center gap-4 flex-1 min-w-0">
          <div className="w-10 h-10 rounded-lg bg-tertiary-container flex items-center justify-center shrink-0">
            <span className="material-symbols-outlined text-xl text-tertiary">forum</span>
          </div>
          <div className="min-w-0">
            <h3 className="text-title-md text-on-surface group-hover:text-primary transition-colors truncate">{session.theme}</h3>
            <div className="flex items-center gap-3 text-body-sm text-on-surface-variant mt-0.5">
              <span className="flex items-center gap-1">
                <span className="material-symbols-outlined text-sm">settings</span>
                {session.mode}
              </span>
              <span className="flex items-center gap-1">
                <span className="material-symbols-outlined text-sm">flag</span>
                {session.phase}
              </span>
              <span className="flex items-center gap-1">
                <span className="material-symbols-outlined text-sm">replay</span>
                Turn {session.turn_number}
              </span>
            </div>
          </div>
        </div>
        <div className="flex items-center gap-3 shrink-0">
          <span className="chip text-body-sm">
            <span className="material-symbols-outlined text-sm">group</span>
            {session.participant_count}
          </span>
          <span className={badgeClass}>
            <span className="material-symbols-outlined text-sm mr-0.5">{statusIcon}</span>
            {session.status}
          </span>
        </div>
      </div>
    </Link>
  );
}
