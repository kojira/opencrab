import { useState, useEffect, useCallback } from 'react';
import { useTranslation } from 'react-i18next';
import {
  getDiscordConfig,
  updateDiscordConfig,
  patchDiscordConfig,
  deleteDiscordConfig,
  startDiscordGateway,
  stopDiscordGateway,
} from '../../api/agents';
import type { DiscordConfigDto } from '../../api/types';
import { DetailRow } from './presentation';

export function DiscordBotSection({ agentId }: { agentId: string }) {
  const { t } = useTranslation();
  const [config, setConfig] = useState<DiscordConfigDto | null>(null);
  const [editing, setEditing] = useState(false);
  const [token, setToken] = useState('');
  const [ownerDiscordId, setOwnerDiscordId] = useState('');
  const [saving, setSaving] = useState(false);
  const [message, setMessage] = useState<string | null>(null);
  const [editMode, setEditMode] = useState<"full" | "owner_only">("owner_only");

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
      let res: { ok: boolean; message?: string; error?: string };
      if (editMode === "full") {
        res = await updateDiscordConfig(agentId, {
          bot_token: token,
          owner_discord_id: ownerDiscordId || undefined,
        });
      } else {
        res = await patchDiscordConfig(agentId, {
          owner_discord_id: ownerDiscordId,
        });
      }
      if (res.ok) {
        setMessage(editMode === "full" ? t('agentDetail.gatewayStarted') : t('agentDetail.ownerUpdated'));
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
          <button className="btn-tonal" onClick={() => {
            setToken("");
            setOwnerDiscordId("");
            setEditMode("full");
            setEditing(true);
          }}>
            <span className="material-symbols-outlined text-xl">add</span>
            {t('agentDetail.configureBot')}
          </button>
        </div>
      )}

      {config.configured && !editing && (
        <div className="space-y-2">
          <DetailRow label={t('agentDetail.botToken')} value={config.configured ? '●●●●●●●●●●●●●●●●●●●●' : t('agentDetail.notConfigured')} />
          {config.owner_discord_id && (
            <DetailRow label={t('agentDetail.ownerDiscordId')} value={config.owner_discord_id} />
          )}
          <DetailRow
            label={t('agentDetail.gatewayStatus')}
            value={config.running ? t('agentDetail.statusRunning') : t('agentDetail.statusStopped')}
          />
          <div className="flex gap-2 pt-2 flex-wrap">
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
              setToken("");
              setOwnerDiscordId(config.owner_discord_id || "");
              setEditMode("owner_only");
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
          {/* モード切り替えタブ */}
          <div className="flex gap-2 border-b border-outline-variant pb-2">
            <button
              className={`text-label-lg px-3 py-1 rounded-t ${editMode === "owner_only" ? "bg-primary-container text-on-primary-container" : "text-on-surface-variant hover:bg-surface-variant/50"}`}
              onClick={() => setEditMode("owner_only")}
            >
              {t('agentDetail.editModeOwnerOnly')}
            </button>
            <button
              className={`text-label-lg px-3 py-1 rounded-t ${editMode === "full" ? "bg-primary-container text-on-primary-container" : "text-on-surface-variant hover:bg-surface-variant/50"}`}
              onClick={() => setEditMode("full")}
            >
              {t('agentDetail.editModeFullToken')}
            </button>
          </div>

          {/* owner_only モード */}
          {editMode === "owner_only" && (
            <div>
              <label className="text-label-lg text-on-surface-variant block mb-1">
                {t("agentDetail.ownerDiscordIdLabel")}
              </label>
              <input
                type="text"
                className="input w-full"
                value={ownerDiscordId}
                onChange={(e) => setOwnerDiscordId(e.target.value)}
                placeholder="e.g. 390123456789012345"
              />
              <p className="text-body-sm text-on-surface-variant mt-1">
                Bot トークンはそのまま保持されます
              </p>
            </div>
          )}

          {/* full モード */}
          {editMode === "full" && (
            <>
              <div>
                <label className="text-label-lg text-on-surface-variant block mb-1">
                  {t("agentDetail.botTokenLabel")}
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
                  {t("agentDetail.ownerDiscordIdLabel")}
                  <span className="text-body-sm text-on-surface-variant ml-1">({t("common.optional")})</span>
                </label>
                <input
                  type="text"
                  className="input w-full"
                  value={ownerDiscordId}
                  onChange={(e) => setOwnerDiscordId(e.target.value)}
                  placeholder="e.g. 390123456789012345"
                />
              </div>
            </>
          )}

          <div className="flex gap-2">
            <button
              className="btn-filled"
              onClick={handleSave}
              disabled={saving || (editMode === "full" && !token)}
            >
              {saving ? t("common.saving") : t("common.save")}
            </button>
            <button className="btn-outlined" onClick={() => setEditing(false)}>
              {t("common.cancel")}
            </button>
          </div>
        </div>
      )}
    </div>
  );
}
