import { useState, useEffect, useCallback } from 'react';
import { Link } from 'react-router-dom';
import { useTranslation } from 'react-i18next';
import { getDiscordConfig, updateDiscordConfig, deleteDiscordConfig, startDiscordGateway, stopDiscordGateway } from '../api/agents';
import type { DiscordConfigDto } from '../api/types';
import { useAgentContext } from '../hooks/useAgentContext';

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

function DiscordBotSection({ agentId }: { agentId: string }) {
  const { t } = useTranslation();
  const [config, setConfig] = useState<DiscordConfigDto | null>(null);
  const [editing, setEditing] = useState(false);
  const [token, setToken] = useState('');
  const [ownerDiscordId, setOwnerDiscordId] = useState('');
  const [saving, setSaving] = useState(false);
  const [message, setMessage] = useState<string | null>(null);

  const loadConfig = useCallback(() => {
    getDiscordConfig(agentId)
      .then(setConfig)
      .catch(() => setConfig({ configured: false }));
  }, [agentId]);

  useEffect(() => {
    loadConfig();
  }, [loadConfig]);

  const handleSave = async () => {
    setSaving(true);
    setMessage(null);
    try {
      const res = await updateDiscordConfig(agentId, {
        bot_token: token,
        owner_discord_id: ownerDiscordId || undefined,
      });
      if (res.ok) {
        setMessage(t('agentDetail.gatewayStarted'));
        setEditing(false);
        setToken('');
        loadConfig();
      } else {
        setMessage(t('agentDetail.gatewayStartFailed', { error: res.error }));
      }
    } catch (e) {
      setMessage(t('agentDetail.gatewayStartFailed', { error: String(e) }));
    } finally {
      setSaving(false);
    }
  };

  const handleStart = async () => {
    setSaving(true);
    setMessage(null);
    try {
      const res = await startDiscordGateway(agentId);
      if (res.ok) {
        setMessage(t('agentDetail.gatewayStarted'));
      } else {
        setMessage(t('agentDetail.gatewayStartFailed', { error: res.error }));
      }
    } catch (e) {
      setMessage(t('agentDetail.gatewayStartFailed', { error: String(e) }));
    } finally {
      setSaving(false);
      loadConfig();
    }
  };

  const handleStop = async () => {
    setSaving(true);
    setMessage(null);
    try {
      await stopDiscordGateway(agentId);
      setMessage(t('agentDetail.gatewayStopped'));
    } catch (e) {
      setMessage(String(e));
    } finally {
      setSaving(false);
      loadConfig();
    }
  };

  const handleRemove = async () => {
    await deleteDiscordConfig(agentId);
    setMessage(t('agentDetail.botRemoved'));
    setEditing(false);
    loadConfig();
  };

  if (!config) return null;

  return (
    <div className="card-outlined mt-6">
      <h2 className="section-title flex items-center gap-2">
        <span className="material-symbols-outlined text-xl text-primary">smart_toy</span>
        {t('agentDetail.discordBot')}
      </h2>

      {message && (
        <div className="mb-3 p-2 rounded-lg bg-tertiary-container/30 text-body-sm text-on-surface">
          {message}
        </div>
      )}

      {!config.configured && !editing && (
        <div>
          <p className="text-body-md text-on-surface-variant mb-3">
            {t('agentDetail.noDiscordBot')}
          </p>
          <button className="btn-tonal" onClick={() => setEditing(true)}>
            <span className="material-symbols-outlined text-xl">add</span>
            {t('agentDetail.configureBot')}
          </button>
        </div>
      )}

      {config.configured && !editing && (
        <div className="space-y-2">
          <DetailRow label={t('agentDetail.botToken')} value={config.token_masked || '***'} />
          {config.owner_discord_id && (
            <DetailRow label={t('agentDetail.ownerDiscordId')} value={config.owner_discord_id} />
          )}
          <DetailRow
            label={t('agentDetail.gatewayStatus')}
            value={config.running ? t('agentDetail.statusRunning') : t('agentDetail.statusStopped')}
          />
          <div className="flex gap-2 pt-2">
            {config.running ? (
              <button className="btn-outlined" onClick={handleStop} disabled={saving}>
                <span className="material-symbols-outlined text-xl">stop</span>
                {t('agentDetail.stopBot')}
              </button>
            ) : (
              <button className="btn-filled" onClick={handleStart} disabled={saving}>
                <span className="material-symbols-outlined text-xl">play_arrow</span>
                {t('agentDetail.startBot')}
              </button>
            )}
            <button className="btn-tonal" onClick={() => {
              setToken('');
              setOwnerDiscordId(config.owner_discord_id || '');
              setEditing(true);
            }}>
              <span className="material-symbols-outlined text-xl">edit</span>
              {t('common.edit')}
            </button>
            <button
              className="btn-outlined border-error text-error hover:bg-error-container/30"
              onClick={handleRemove}
            >
              <span className="material-symbols-outlined text-xl">delete</span>
              {t('agentDetail.removeBot')}
            </button>
          </div>
        </div>
      )}

      {editing && (
        <div className="space-y-3">
          <div>
            <label className="text-label-lg text-on-surface-variant block mb-1">
              {t('agentDetail.botTokenLabel')}
            </label>
            <input
              type="password"
              className="input w-full"
              value={token}
              onChange={(e) => setToken(e.target.value)}
              placeholder="Bot token..."
            />
          </div>
          <div>
            <label className="text-label-lg text-on-surface-variant block mb-1">
              {t('agentDetail.ownerDiscordIdLabel')}
              <span className="text-body-sm text-on-surface-variant ml-1">({t('common.optional')})</span>
            </label>
            <input
              type="text"
              className="input w-full"
              value={ownerDiscordId}
              onChange={(e) => setOwnerDiscordId(e.target.value)}
              placeholder="e.g. 390123456789012345"
            />
          </div>
          <div className="flex gap-2">
            <button className="btn-filled" onClick={handleSave} disabled={saving || !token}>
              {saving ? t('common.saving') : t('common.save')}
            </button>
            <button className="btn-outlined" onClick={() => setEditing(false)}>
              {t('common.cancel')}
            </button>
          </div>
        </div>
      )}
    </div>
  );
}

export default function AgentOverview() {
  const { t } = useTranslation();
  const { agent, agentId } = useAgentContext();

  if (!agent) return null;

  return (
    <>
      {/* Action cards */}
      <div className="grid grid-cols-1 md:grid-cols-3 gap-4 mb-6">
        <ActionCard
          to={`/agents/${agentId}/persona`}
          icon="face"
          title={t('agentDetail.editPersona')}
          description={t('agentDetail.editPersonaDesc')}
        />
        <ActionCard
          to={`/agents/${agentId}/skills`}
          icon="psychology"
          title={t('agentDetail.manageSkills')}
          description={t('agentDetail.manageSkillsDesc')}
        />
        <ActionCard
          to={`/workspace/${agentId}`}
          icon="folder_open"
          title={t('agentDetail.workspace')}
          description={t('agentDetail.workspaceDesc')}
        />
        <ActionCard
          to={`/agents/${agentId}/memory`}
          icon="memory"
          title={t('agentDetail.manageMemory')}
          description={t('agentDetail.manageMemoryDesc')}
        />
        <ActionCard
          to={`/agents/${agentId}/sessions`}
          icon="forum"
          title={t('agentDetail.manageSessions')}
          description={t('agentDetail.manageSessionsDesc')}
        />
        <ActionCard
          to={`/agents/${agentId}/analytics`}
          icon="analytics"
          title={t('agentDetail.manageAnalytics')}
          description={t('agentDetail.manageAnalyticsDesc')}
        />
      </div>

      {/* Identity details */}
      <div className="card-outlined">
        <h2 className="section-title flex items-center gap-2">
          <span className="material-symbols-outlined text-xl text-primary">
            badge
          </span>
          {t('agentDetail.identity')}
        </h2>
        <div className="space-y-3">
          <DetailRow label={t('agentDetail.agentId')} value={agent.id} />
          <DetailRow label={t('agentDetail.name')} value={agent.name} />
          <DetailRow label={t('agentDetail.role')} value={agent.role} />
        </div>
      </div>

      {/* Discord Bot */}
      <DiscordBotSection agentId={agentId} />
    </>
  );
}
