import { useState } from 'react';
import { Link, useNavigate } from 'react-router-dom';
import { useTranslation } from 'react-i18next';
import { createAgent } from '../api/agents';

export default function AgentCreate() {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const [name, setName] = useState('');
  const [role, setRole] = useState('discussant');
  const [personaName, setPersonaName] = useState('');
  const [errorMsg, setErrorMsg] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);

  const handleSubmit = async () => {
    if (!name.trim()) {
      setErrorMsg(t('agentCreate.nameRequired'));
      return;
    }
    setSaving(true);
    try {
      const agent = await createAgent({
        name: name.trim(),
        persona_name: personaName.trim() || name.trim(),
        role,
      });
      navigate(`/agents/${agent.id}`);
    } catch (e) {
      setErrorMsg(`Error: ${e instanceof Error ? e.message : e}`);
      setSaving(false);
    }
  };

  return (
    <div className="max-w-2xl mx-auto">
      <div className="flex items-center gap-3 mb-6">
        <Link to="/agents" className="btn-text p-2">
          <span className="material-symbols-outlined">arrow_back</span>
        </Link>
        <h1 className="page-title">{t('agentCreate.title')}</h1>
      </div>

      <div className="card-elevated space-y-6">
        <div>
          <label className="block text-label-lg text-on-surface mb-2">
            {t('agentCreate.nameLabel')}
          </label>
          <input
            type="text"
            className="input-outlined"
            placeholder={t('agentCreate.namePlaceholder')}
            value={name}
            onChange={(e) => setName(e.target.value)}
          />
        </div>

        <div>
          <label className="block text-label-lg text-on-surface mb-2">
            {t('agentCreate.roleLabel')}
          </label>
          <input
            type="text"
            className="input-outlined"
            list="role-options"
            placeholder={t('agentCreate.rolePlaceholder')}
            value={role}
            onChange={(e) => setRole(e.target.value)}
          />
          <datalist id="role-options">
            <option value={t('roles.discussant')} />
            <option value={t('roles.facilitator')} />
            <option value={t('roles.observer')} />
          </datalist>
        </div>

        <div>
          <label className="block text-label-lg text-on-surface mb-2">
            {t('agentCreate.personaNameLabel')}
          </label>
          <input
            type="text"
            className="input-outlined"
            placeholder={t('agentCreate.personaNamePlaceholder')}
            value={personaName}
            onChange={(e) => setPersonaName(e.target.value)}
          />
        </div>

        {errorMsg && (
          <div className="flex items-center gap-2 p-4 rounded-md bg-error-container">
            <span className="material-symbols-outlined text-error">error</span>
            <p className="text-body-md text-error-on-container">{errorMsg}</p>
          </div>
        )}

        <div className="flex gap-3 pt-2">
          <button
            className="btn-filled flex-1"
            disabled={saving}
            onClick={handleSubmit}
          >
            {saving ? (
              <>
                <span className="material-symbols-outlined animate-spin text-xl">
                  progress_activity
                </span>
                {t('common.creating')}
              </>
            ) : (
              <>
                <span className="material-symbols-outlined text-xl">add</span>
                {t('common.create')}
              </>
            )}
          </button>
          <Link to="/agents" className="btn-outlined">
            {t('common.cancel')}
          </Link>
        </div>
      </div>
    </div>
  );
}
