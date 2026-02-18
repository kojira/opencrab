import { useState, useEffect, useCallback } from 'react';
import { useTranslation } from 'react-i18next';
import { getAgents } from '../api/agents';
import { getSkills, toggleSkill } from '../api/skills';
import type { AgentSummary, SkillDto } from '../api/types';
import SkillEditor from '../components/ui/SkillEditor';

export default function Skills() {
  const { t } = useTranslation();
  const [agents, setAgents] = useState<AgentSummary[] | null>(null);
  const [selectedAgent, setSelectedAgent] = useState<string | null>(null);
  const [skills, setSkills] = useState<SkillDto[] | null>(null);
  const [skillsError, setSkillsError] = useState<string | null>(null);

  useEffect(() => {
    getAgents().then(setAgents).catch(() => {});
  }, []);

  const loadSkills = useCallback((agentId: string) => {
    setSkills(null);
    setSkillsError(null);
    getSkills(agentId)
      .then(setSkills)
      .catch((e: Error) => setSkillsError(e.message));
  }, []);

  useEffect(() => {
    if (selectedAgent) loadSkills(selectedAgent);
  }, [selectedAgent, loadSkills]);

  const handleToggle = async (skillId: string, active: boolean) => {
    if (!selectedAgent) return;
    await toggleSkill(selectedAgent, skillId, active);
    loadSkills(selectedAgent);
  };

  return (
    <div className="max-w-7xl mx-auto">
      <h1 className="page-title mb-6">{t('skills.title')}</h1>

      <div className="card-elevated mb-6">
        <label className="block text-label-lg text-on-surface mb-2">
          <span className="flex items-center gap-1.5">
            <span className="material-symbols-outlined text-lg">
              smart_toy
            </span>
            {t('common.selectAgent')}
          </span>
        </label>
        {agents ? (
          <select
            className="select-outlined"
            onChange={(e) =>
              setSelectedAgent(e.target.value || null)
            }
          >
            <option value="">{t('common.selectAgentPlaceholder')}</option>
            {agents.map((a) => (
              <option key={a.id} value={a.id}>
                {a.name}
              </option>
            ))}
          </select>
        ) : (
          <p className="text-body-md text-on-surface-variant">
            {t('common.loadingAgents')}
          </p>
        )}
      </div>

      {selectedAgent ? (
        skillsError ? (
          <div className="card-outlined border-error bg-error-container/30 p-4">
            <div className="flex items-center gap-2">
              <span className="material-symbols-outlined text-error">
                error
              </span>
              <p className="text-body-lg text-error-on-container">
                {t('common.error', { message: skillsError })}
              </p>
            </div>
          </div>
        ) : skills === null ? (
          <div className="empty-state">
            <p className="text-body-lg text-on-surface-variant">
              {t('skills.loadingSkills')}
            </p>
          </div>
        ) : skills.length === 0 ? (
          <div className="empty-state">
            <span className="material-symbols-outlined empty-state-icon">
              psychology
            </span>
            <p className="empty-state-text">
              {t('skills.noSkills')}
            </p>
          </div>
        ) : (
          <div className="space-y-3">
            {skills.map((skill) => (
              <SkillEditor
                key={skill.id}
                skill={skill}
                onToggle={handleToggle}
              />
            ))}
          </div>
        )
      ) : (
        <div className="empty-state">
          <span className="material-symbols-outlined empty-state-icon">
            psychology
          </span>
          <p className="empty-state-text">
            {t('skills.selectAgent')}
          </p>
        </div>
      )}
    </div>
  );
}
