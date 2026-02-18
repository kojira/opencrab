import { useTranslation } from 'react-i18next';
import type { SkillDto } from '../../api/types';

interface Props {
  skill: SkillDto;
  onToggle: (skillId: string, active: boolean) => void;
}

export default function SkillEditor({ skill, onToggle }: Props) {
  const { t } = useTranslation();

  const effectivenessPct = skill.effectiveness
    ? Math.round(skill.effectiveness * 100)
    : 0;

  const sourceBadge =
    skill.source_type === 'standard'
      ? 'badge-info'
      : skill.source_type === 'acquired'
        ? 'bg-tertiary-container text-tertiary-on-container badge'
        : 'badge-neutral';

  return (
    <div className="card-outlined">
      <div className="flex items-start justify-between gap-4">
        <div className="flex-1 min-w-0">
          <div className="flex items-center gap-2 mb-1">
            <span className="material-symbols-outlined text-xl text-primary">
              extension
            </span>
            <h3 className="text-title-md text-on-surface truncate">
              {skill.name}
            </h3>
            <span className={sourceBadge}>{skill.source_type}</span>
          </div>
          <p className="text-body-md text-on-surface-variant ml-8">
            {skill.description}
          </p>
        </div>

        <button
          className={skill.is_active ? 'switch-active' : 'switch'}
          onClick={() => onToggle(skill.id, !skill.is_active)}
        >
          <span
            className={
              skill.is_active ? 'switch-thumb-active' : 'switch-thumb'
            }
          />
        </button>
      </div>

      <div className="mt-3 pt-3 border-t border-outline-variant/50 flex items-center gap-6 ml-8">
        <div className="flex items-center gap-1.5 text-body-sm text-on-surface-variant">
          <span className="material-symbols-outlined text-base">repeat</span>
          <span>{t('skillEditor.usedTimes', { count: skill.usage_count })}</span>
        </div>
        {skill.effectiveness != null && (
          <div className="flex items-center gap-1.5 text-body-sm text-on-surface-variant">
            <span className="material-symbols-outlined text-base">speed</span>
            <span>{t('skillEditor.effectiveness', { pct: effectivenessPct })}</span>
          </div>
        )}
      </div>
    </div>
  );
}
