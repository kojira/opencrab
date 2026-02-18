import { useState, useEffect } from 'react';
import { Link, useParams, useNavigate } from 'react-router-dom';
import { getAgent, updateIdentity } from '../api/agents';

export default function AgentIdentityEdit() {
  const { id } = useParams<{ id: string }>();
  const navigate = useNavigate();
  const [name, setName] = useState('');
  const [role, setRole] = useState('');
  const [jobTitle, setJobTitle] = useState('');
  const [organization, setOrganization] = useState('');
  const [initialized, setInitialized] = useState(false);
  const [saving, setSaving] = useState(false);
  const [saveStatus, setSaveStatus] = useState<string | null>(null);

  useEffect(() => {
    if (!id) return;
    getAgent(id).then((detail) => {
      setName(detail.name);
      setRole(detail.role);
      setJobTitle(detail.job_title ?? '');
      setOrganization(detail.organization ?? '');
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
        job_title: jobTitle || null,
        organization: organization || null,
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
        <p className="text-body-lg text-on-surface-variant">Loading...</p>
      </div>
    );
  }

  return (
    <div className="max-w-2xl mx-auto">
      <div className="flex items-center gap-3 mb-6">
        <Link to={`/agents/${id}`} className="btn-text p-2">
          <span className="material-symbols-outlined">arrow_back</span>
        </Link>
        <h1 className="page-title">Edit Identity</h1>
      </div>

      <div className="card-elevated space-y-6">
        <div>
          <label className="block text-label-lg text-on-surface mb-2">
            Name
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
            Role
          </label>
          <select
            className="select-outlined"
            value={role}
            onChange={(e) => setRole(e.target.value)}
          >
            <option value="discussant">Discussant</option>
            <option value="facilitator">Facilitator</option>
            <option value="observer">Observer</option>
            <option value="mentor">Mentor</option>
          </select>
        </div>

        <div>
          <label className="block text-label-lg text-on-surface mb-2">
            Job Title
          </label>
          <input
            type="text"
            className="input-outlined"
            placeholder="Optional"
            value={jobTitle}
            onChange={(e) => setJobTitle(e.target.value)}
          />
        </div>

        <div>
          <label className="block text-label-lg text-on-surface mb-2">
            Organization
          </label>
          <input
            type="text"
            className="input-outlined"
            placeholder="Optional"
            value={organization}
            onChange={(e) => setOrganization(e.target.value)}
          />
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
                Saving...
              </>
            ) : (
              <>
                <span className="material-symbols-outlined text-xl">save</span>
                Save
              </>
            )}
          </button>
          <Link to={`/agents/${id}`} className="btn-outlined">
            Cancel
          </Link>
        </div>
      </div>
    </div>
  );
}
