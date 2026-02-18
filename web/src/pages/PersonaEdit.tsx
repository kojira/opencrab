import { useState, useEffect } from 'react';
import { Link, useParams } from 'react-router-dom';
import { useTranslation } from 'react-i18next';
import { getAgent, updateSoul } from '../api/agents';
import type { PersonalityDto } from '../api/types';

function PersonalitySlider({
  label,
  value,
  onChange,
}: {
  label: string;
  value: number;
  onChange: (v: number) => void;
}) {
  const pct = Math.round(value * 100);

  return (
    <div>
      <div className="flex justify-between mb-2">
        <span className="text-label-lg text-on-surface">{label}</span>
        <span className="text-label-md text-primary font-mono">
          {value.toFixed(2)}
        </span>
      </div>
      <div className="relative">
        <input
          type="range"
          className="m3-slider"
          min="0"
          max="1"
          step="0.05"
          value={value}
          onChange={(e) => onChange(parseFloat(e.target.value))}
        />
        <div
          className="absolute top-1/2 left-0 h-1 bg-primary rounded-full pointer-events-none -translate-y-1/2"
          style={{ width: `${pct}%` }}
        />
      </div>
    </div>
  );
}

export default function PersonaEdit() {
  const { t } = useTranslation();
  const { id } = useParams<{ id: string }>();
  const [personaName, setPersonaName] = useState('');
  const [personality, setPersonality] = useState<PersonalityDto>({
    openness: 0.5,
    conscientiousness: 0.5,
    extraversion: 0.5,
    agreeableness: 0.5,
    neuroticism: 0.5,
  });
  const [thinkingPrimary, setThinkingPrimary] = useState('Analytical');
  const [thinkingSecondary, setThinkingSecondary] = useState('Practical');
  const [thinkingDesc, setThinkingDesc] = useState('');
  const [initialized, setInitialized] = useState(false);
  const [saveStatus, setSaveStatus] = useState<string | null>(null);

  useEffect(() => {
    if (!id) return;
    getAgent(id).then((detail) => {
      setPersonaName(detail.persona_name);
      try {
        const p: PersonalityDto = JSON.parse(detail.personality_json);
        setPersonality(p);
      } catch {
        // keep defaults
      }
      try {
        const ts = JSON.parse(detail.thinking_style_json);
        if (ts.primary) setThinkingPrimary(ts.primary);
        if (ts.secondary) setThinkingSecondary(ts.secondary);
        if (ts.description) setThinkingDesc(ts.description);
      } catch {
        // keep defaults
      }
      setInitialized(true);
    });
  }, [id]);

  const handleSave = async () => {
    if (!id) return;
    const thinkingStyleJson = JSON.stringify({
      primary: thinkingPrimary,
      secondary: thinkingSecondary,
      description: thinkingDesc,
    });
    try {
      await updateSoul(id, {
        persona_name: personaName,
        social_style_json: '{}',
        personality_json: JSON.stringify(personality),
        thinking_style_json: thinkingStyleJson,
        custom_traits_json: null,
      });
      setSaveStatus(t('personaEdit.savedSuccess'));
    } catch (e) {
      setSaveStatus(`Error: ${e instanceof Error ? e.message : e}`);
    }
  };

  const updatePersonality = (key: keyof PersonalityDto, v: number) => {
    setPersonality((prev) => ({ ...prev, [key]: v }));
  };

  if (!initialized) {
    return (
      <div className="empty-state">
        <p className="text-body-lg text-on-surface-variant">{t('common.loading')}</p>
      </div>
    );
  }

  return (
    <div className="max-w-4xl mx-auto">
      <div className="flex items-center gap-3 mb-6">
        <Link to={`/agents/${id}`} className="btn-text p-2">
          <span className="material-symbols-outlined">arrow_back</span>
        </Link>
        <h1 className="page-title">{t('personaEdit.title')}</h1>
      </div>

      {/* Persona name section */}
      <div className="card-outlined mb-6">
        <h2 className="section-title flex items-center gap-2">
          <span className="material-symbols-outlined text-xl text-primary">
            face
          </span>
          {t('personaEdit.personaName')}
        </h2>
        <input
          type="text"
          className="input-outlined"
          value={personaName}
          onChange={(e) => setPersonaName(e.target.value)}
        />
      </div>

      {/* Big Five personality section */}
      <div className="card-outlined mb-6">
        <h2 className="section-title flex items-center gap-2">
          <span className="material-symbols-outlined text-xl text-primary">
            psychology
          </span>
          {t('personaEdit.personality')}
        </h2>
        <div className="space-y-5">
          <PersonalitySlider
            label={t('personaEdit.openness')}
            value={personality.openness}
            onChange={(v) => updatePersonality('openness', v)}
          />
          <PersonalitySlider
            label={t('personaEdit.conscientiousness')}
            value={personality.conscientiousness}
            onChange={(v) => updatePersonality('conscientiousness', v)}
          />
          <PersonalitySlider
            label={t('personaEdit.extraversion')}
            value={personality.extraversion}
            onChange={(v) => updatePersonality('extraversion', v)}
          />
          <PersonalitySlider
            label={t('personaEdit.agreeableness')}
            value={personality.agreeableness}
            onChange={(v) => updatePersonality('agreeableness', v)}
          />
          <PersonalitySlider
            label={t('personaEdit.neuroticism')}
            value={personality.neuroticism}
            onChange={(v) => updatePersonality('neuroticism', v)}
          />
        </div>
      </div>

      {/* Thinking style section */}
      <div className="card-outlined mb-6">
        <h2 className="section-title flex items-center gap-2">
          <span className="material-symbols-outlined text-xl text-primary">
            lightbulb
          </span>
          {t('personaEdit.thinkingStyle')}
        </h2>
        <div className="space-y-5">
          <div>
            <label className="block text-label-lg text-on-surface mb-2">
              {t('personaEdit.primary')}
            </label>
            <select
              className="select-outlined"
              value={thinkingPrimary}
              onChange={(e) => setThinkingPrimary(e.target.value)}
            >
              <option value="Analytical">{t('thinkingStyles.analytical')}</option>
              <option value="Intuitive">{t('thinkingStyles.intuitive')}</option>
              <option value="Practical">{t('thinkingStyles.practical')}</option>
              <option value="Creative">{t('thinkingStyles.creative')}</option>
            </select>
          </div>
          <div>
            <label className="block text-label-lg text-on-surface mb-2">
              {t('personaEdit.secondary')}
            </label>
            <select
              className="select-outlined"
              value={thinkingSecondary}
              onChange={(e) => setThinkingSecondary(e.target.value)}
            >
              <option value="Analytical">{t('thinkingStyles.analytical')}</option>
              <option value="Intuitive">{t('thinkingStyles.intuitive')}</option>
              <option value="Practical">{t('thinkingStyles.practical')}</option>
              <option value="Creative">{t('thinkingStyles.creative')}</option>
            </select>
          </div>
          <div>
            <label className="block text-label-lg text-on-surface mb-2">
              {t('personaEdit.description')}
            </label>
            <textarea
              className="input-outlined min-h-[80px]"
              rows={3}
              value={thinkingDesc}
              onChange={(e) => setThinkingDesc(e.target.value)}
            />
          </div>
        </div>
      </div>

      {saveStatus && (
        <div className="flex items-center gap-2 p-4 rounded-md bg-success-container mb-6">
          <span className="material-symbols-outlined text-success">
            check_circle
          </span>
          <p className="text-body-md text-success-on-container">{saveStatus}</p>
        </div>
      )}

      <button className="btn-filled w-full py-3" onClick={handleSave}>
        <span className="material-symbols-outlined text-xl">save</span>
        {t('common.save')}
      </button>
    </div>
  );
}
