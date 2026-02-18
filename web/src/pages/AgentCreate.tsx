import { useState } from 'react';
import { Link, useNavigate } from 'react-router-dom';
import { createAgent } from '../api/agents';

export default function AgentCreate() {
  const navigate = useNavigate();
  const [name, setName] = useState('');
  const [role, setRole] = useState('discussant');
  const [personaName, setPersonaName] = useState('');
  const [errorMsg, setErrorMsg] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);

  const handleSubmit = async () => {
    if (!name.trim()) {
      setErrorMsg('Name is required.');
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
        <h1 className="page-title">Create New Agent</h1>
      </div>

      <div className="card-elevated space-y-6">
        <div>
          <label className="block text-label-lg text-on-surface mb-2">
            Name *
          </label>
          <input
            type="text"
            className="input-outlined"
            placeholder="e.g. Kai"
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
            Persona Name
          </label>
          <input
            type="text"
            className="input-outlined"
            placeholder="e.g. Pragmatic Engineer (defaults to name)"
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
                Creating...
              </>
            ) : (
              <>
                <span className="material-symbols-outlined text-xl">add</span>
                Create
              </>
            )}
          </button>
          <Link to="/agents" className="btn-outlined">
            Cancel
          </Link>
        </div>
      </div>
    </div>
  );
}
