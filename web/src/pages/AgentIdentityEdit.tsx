import { useState, useEffect } from 'react';
import { Link, useParams, useNavigate } from 'react-router-dom';
import { useTranslation } from 'react-i18next';
import { getAgent, updateIdentity } from '../api/agents';

export default function AgentIdentityEdit() {
  const { t } = useTranslation();
  const { id } = useParams<{ id: string }>();
  const navigate = useNavigate();
  const [name, setName] = useState('');
  const [role, setRole] = useState('');
  const [initialized, setInitialized] = useState(false);
  const [saving, setSaving] = useState(false);
  const [saveStatus, setSaveStatus] = useState<string | null>(null);

  useEffect(() => {
    if (!id) return;
    getAgent(id).then((detail) => {
      setName(detail.name);
      setRole(detail.role);
      setInitialized(true);
    });
  }, [id]);

  const handleSave = async () => {
    if (!id) return;
    setSaving(true);
    try {
      await updateIdentity(id, {
        name,
        role,
        job_title: null,
        organization: null,
        image_url: null,
        metadata_json: null,
      });
      navigate(`/agents/${id}`);
    } catch (e) {
      setSaveStatus(`Error: ${e instanceof Error ? e.message : e}`);
      setSaving(false);
    }
  };

  if (!initialized) {
    return (
      <div className="empty-state">
        <p className="text-body-lg text-on-surface-variant">{t('common.loading')}</p>
      </div>
    );
  }

  return (
    <div className="max-w-2xl mx-auto">
      <div className="flex items-center gap-3 mb-6">
        <Link to={`/agents/${id}`} className="btn-text p-2">
          <span className="material-symbols-outlined">arrow_back</span>
        </Link>
        <h1 className="page-title">{t('identityEdit.title')}</h1>
      </div>

      <div className="card-elevated space-y-6">
        <div>
          <label className="block text-label-lg text-on-surface mb-2">
            {t('identityEdit.nameLabel')}
          </label>
          <input
            type="text"
            className="input-outlined"
            value={name}
            onChange={(e) => setName(e.target.value)}
          />
        </div>

        <div>
          <label className="block text-label-lg text-on-surface mb-2">
            {t('identityEdit.roleLabel')}
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

        {saveStatus && (
          <div className="flex items-center gap-2 p-4 rounded-md bg-success-container">
            <span className="material-symbols-outlined text-success">
              check_circle
            </span>
            <p className="text-body-md text-success-on-container">
              {saveStatus}
            </p>
          </div>
        )}

        <div className="flex gap-3 pt-2">
          <button
            className="btn-filled flex-1"
            disabled={saving}
            onClick={handleSave}
          >
            {saving ? (
              <>
                <span className="material-symbols-outlined animate-spin text-xl">
                  progress_activity
                </span>
                {t('common.saving')}
              </>
            ) : (
              <>
                <span className="material-symbols-outlined text-xl">save</span>
                {t('common.save')}
              </>
            )}
          </button>
          <Link to={`/agents/${id}`} className="btn-outlined">
            {t('common.cancel')}
          </Link>
        </div>
      </div>
    </div>
  );
}
