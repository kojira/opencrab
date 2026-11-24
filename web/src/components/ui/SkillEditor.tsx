import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import type { SkillDto, SkillUpdate } from '../../api/types';

interface Props {
  skill: SkillDto;
  onToggle: (skillId: string, active: boolean) => void;
  onUpdate: (skillId: string, update: SkillUpdate) => void;
}

/** Comma/whitespace separated tool names → list (empty entries dropped). */
export function parseTools(text: string): string[] {
  return text
    .split(/[\s,]+/)
    .map((name) => name.trim())
    .filter((name) => name.length > 0);
}

export default function SkillEditor({ skill, onToggle, onUpdate }: Props) {
  const { t } = useTranslation();
  const savedTools = (skill.tools ?? []).join(', ');
  const [toolsText, setToolsText] = useState(savedTools);

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
          aria-label={t('skillEditor.active')}
          onClick={() => onToggle(skill.id, !skill.is_active)}
        >
          <span
            className={
              skill.is_active ? 'switch-thumb-active' : 'switch-thumb'
            }
          />
        </button>
      </div>

      <div className="mt-3 pt-3 border-t border-outline-variant/50 ml-8 space-y-2">
        <div className="flex items-center justify-between gap-4">
          <div className="min-w-0">
            <p className="text-body-md text-on-surface">
              {t('skillEditor.usableByAnyone')}
            </p>
            <p className="text-body-sm text-on-surface-variant">
              {t('skillEditor.usableByAnyoneHelp')}
            </p>
          </div>
          <button
            className={skill.agent_visible ? 'switch-active' : 'switch'}
            aria-label={t('skillEditor.usableByAnyone')}
            onClick={() =>
              onUpdate(skill.id, { agent_visible: !skill.agent_visible })
            }
          >
            <span
              className={
                skill.agent_visible ? 'switch-thumb-active' : 'switch-thumb'
              }
            />
          </button>
        </div>
        <div className="flex items-end gap-2">
          <label className="flex-1 text-body-sm text-on-surface-variant">
            {t('skillEditor.tools')}
            <input
              className="input-outlined w-full mt-1"
              value={toolsText}
              placeholder="ws_edit, ws_read"
              onChange={(e) => setToolsText(e.target.value)}
            />
          </label>
          <button
            className="btn-filled"
            disabled={toolsText === savedTools}
            onClick={() => onUpdate(skill.id, { tools: parseTools(toolsText) })}
          >
            {t('skillEditor.saveTools')}
          </button>
        </div>
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
