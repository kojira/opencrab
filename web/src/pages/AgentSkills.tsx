import { useState, useEffect, useCallback } from 'react';
import { useTranslation } from 'react-i18next';
import { getSkills, toggleSkill } from '../api/skills';
import type { SkillDto } from '../api/types';
import SkillEditor from '../components/ui/SkillEditor';
import { useAgentContext } from '../hooks/useAgentContext';

export default function AgentSkills() {
  const { t } = useTranslation();
  const { agentId } = useAgentContext();
  const [skills, setSkills] = useState<SkillDto[] | null>(null);
  const [skillsError, setSkillsError] = useState<string | null>(null);

  const loadSkills = useCallback(() => {
    setSkills(null);
    setSkillsError(null);
    getSkills(agentId)
      .then(setSkills)
      .catch((e: Error) => setSkillsError(e.message));
  }, [agentId]);

  useEffect(() => {
    loadSkills();
  }, [loadSkills]);

  const handleToggle = async (skillId: string, active: boolean) => {
    await toggleSkill(agentId, skillId, active);
    loadSkills();
  };

  if (skillsError) {
    return (
      <div className="card-outlined border-error bg-error-container/30 p-4">
        <div className="flex items-center gap-2">
          <span className="material-symbols-outlined text-error">error</span>
          <p className="text-body-lg text-error-on-container">
            {t('common.error', { message: skillsError })}
          </p>
        </div>
      </div>
    );
  }

  if (skills === null) {
    return (
      <div className="empty-state">
        <p className="text-body-lg text-on-surface-variant">
          {t('skills.loadingSkills')}
        </p>
      </div>
    );
  }

  if (skills.length === 0) {
    return (
      <div className="empty-state">
        <span className="material-symbols-outlined empty-state-icon">psychology</span>
        <p className="empty-state-text">{t('skills.noSkills')}</p>
      </div>
    );
  }

  return (
    <div className="space-y-3">
      {skills.map((skill) => (
        <SkillEditor key={skill.id} skill={skill} onToggle={handleToggle} />
      ))}
    </div>
  );
}
