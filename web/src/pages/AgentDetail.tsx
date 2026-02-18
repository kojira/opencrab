import { useState, useEffect } from 'react';
import { Link, useParams, useNavigate } from 'react-router-dom';
import { getAgent, deleteAgent } from '../api/agents';
import type { AgentDetail as AgentDetailType } from '../api/types';
import ConfirmDialog from '../components/ui/ConfirmDialog';

function DetailRow({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex items-center py-2">
      <span className="w-36 text-label-lg text-on-surface-variant">
        {label}
      </span>
      <span className="text-body-lg text-on-surface font-mono">{value}</span>
    </div>
  );
}

function ActionCard({
  to,
  icon,
  title,
  description,
}: {
  to: string;
  icon: string;
  title: string;
  description: string;
}) {
  return (
    <Link to={to} className="card-elevated text-center group">
      <span className="material-symbols-outlined text-3xl text-primary mb-2 group-hover:scale-110 transition-transform">
        {icon}
      </span>
      <h3 className="text-title-md text-on-surface mb-1">{title}</h3>
      <p className="text-body-sm text-on-surface-variant">{description}</p>
    </Link>
  );
}

export default function AgentDetail() {
  const { id } = useParams<{ id: string }>();
  const navigate = useNavigate();
  const [agent, setAgent] = useState<AgentDetailType | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [showDeleteConfirm, setShowDeleteConfirm] = useState(false);

  useEffect(() => {
    if (!id) return;
    getAgent(id)
      .then(setAgent)
      .catch((e: Error) => setError(e.message));
  }, [id]);

  const handleDelete = async () => {
    if (!id) return;
    const res = await deleteAgent(id);
    if (res.deleted) {
      navigate('/agents');
    }
  };

  if (error) {
    return (
      <div className="card-outlined border-error bg-error-container/30 p-4">
        <div className="flex items-center gap-2">
          <span className="material-symbols-outlined text-error">error</span>
          <p className="text-body-lg text-error-on-container">
            Error: {error}
          </p>
        </div>
      </div>
    );
  }

  if (!agent) {
    return (
      <div className="empty-state">
        <p className="text-body-lg text-on-surface-variant">Loading...</p>
      </div>
    );
  }

  return (
    <div className="max-w-4xl mx-auto">
      {/* Agent header card */}
      <div className="card-elevated mb-6">
        <div className="flex items-center gap-5">
          <div className="w-16 h-16 rounded-full bg-primary-container flex items-center justify-center">
            <span className="text-headline-sm text-primary-on-container font-semibold">
              {agent.name.charAt(0) || '?'}
            </span>
          </div>
          <div className="flex-1 min-w-0">
            <h1 className="text-headline-sm text-on-surface font-medium truncate">
              {agent.name}
            </h1>
            <p className="text-body-lg text-on-surface-variant">
              {agent.persona_name} / {agent.role}
            </p>
            {agent.organization && (
              <p className="text-body-sm text-on-surface-variant">
                {agent.organization}
              </p>
            )}
          </div>
          <div className="flex items-center gap-2">
            <Link to={`/agents/${id}/edit`} className="btn-tonal">
              <span className="material-symbols-outlined text-xl">edit</span>
              Edit
            </Link>
            <button
              className="btn-outlined border-error text-error hover:bg-error-container/30"
              onClick={() => setShowDeleteConfirm(true)}
            >
              <span className="material-symbols-outlined text-xl">delete</span>
              Delete
            </button>
          </div>
        </div>
      </div>

      {showDeleteConfirm && (
        <ConfirmDialog
          title="Delete Agent?"
          message="This will permanently delete the agent and all associated data (soul, skills, memories)."
          onConfirm={handleDelete}
          onCancel={() => setShowDeleteConfirm(false)}
        />
      )}

      {/* Action cards */}
      <div className="grid grid-cols-1 md:grid-cols-3 gap-4 mb-6">
        <ActionCard
          to={`/agents/${id}/persona`}
          icon="face"
          title="Edit Persona"
          description="Personality & thinking style"
        />
        <ActionCard
          to="/skills"
          icon="psychology"
          title="Manage Skills"
          description="Enable/disable skills"
        />
        <ActionCard
          to={`/workspace/${id}`}
          icon="folder_open"
          title="Workspace"
          description="Browse agent files"
        />
      </div>

      {/* Identity details */}
      <div className="card-outlined">
        <h2 className="section-title flex items-center gap-2">
          <span className="material-symbols-outlined text-xl text-primary">
            badge
          </span>
          Identity
        </h2>
        <div className="space-y-3">
          <DetailRow label="Agent ID" value={agent.id} />
          <DetailRow label="Name" value={agent.name} />
          <DetailRow label="Role" value={agent.role} />
          {agent.job_title && (
            <DetailRow label="Job Title" value={agent.job_title} />
          )}
          {agent.organization && (
            <DetailRow label="Organization" value={agent.organization} />
          )}
        </div>
      </div>
    </div>
  );
}
