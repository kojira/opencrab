import { useState, useEffect, useCallback } from 'react';
import { Link } from 'react-router-dom';
import { useTranslation } from 'react-i18next';
import {
  getDiscordConfig,
  updateDiscordConfig,
  patchDiscordConfig,
  deleteDiscordConfig,
  startDiscordGateway,
  stopDiscordGateway,
  patchAgent,
} from '../api/agents';
import { getLlmModelChoices } from '../api/llm';
import ModelPricingForm from '../components/ui/ModelPricingForm';
import {
  getNostrConfig,
  updateNostrConfig,
  deleteNostrConfig,
  generateNostrKey,
  getNostrRelayConfig,
  updateNostrRelayConfig,
  type NostrConfigDto,
  type NostrRelayConfigDto,
} from '../api/nostr';
import {
  listMcpServers,
  putMcpServer,
  setMcpEnabled,
  deleteMcpServer,
  testMcpServer,
  type McpServerDto,
} from '../api/mcp';
import type { DiscordConfigDto } from '../api/types';
import { useAgentContext } from '../hooks/useAgentContext';

function DetailRow({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex items-start py-2 gap-2">
      <span className="w-36 shrink-0 text-label-lg text-on-surface-variant">
        {label}
      </span>
      <span className="text-body-lg text-on-surface font-mono break-words min-w-0">{value}</span>
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

function LlmModelSection({ agentId }: { agentId: string }) {
  const { t } = useTranslation();
  const { agent } = useAgentContext();
  const [defaultModel, setDefaultModel] = useState('');
  const [choices, setChoices] = useState<string[]>([]);
  const [selection, setSelection] = useState('');
  const [reasoningEffort, setReasoningEffort] = useState('');
  const [webSearch, setWebSearch] = useState(false);
  const [saving, setSaving] = useState(false);
  const [message, setMessage] = useState<string | null>(null);
  // 保存が「未登録モデル」エラーで弾かれたときに、その場で登録するための spec。
  const [unregisteredSpec, setUnregisteredSpec] = useState<string | null>(null);

  useEffect(() => {
    getLlmModelChoices()
      .then((c) => {
        setDefaultModel(c.default_model);
        setChoices(c.choices);
      })
      .catch(() => {
        setDefaultModel('');
        setChoices([]);
      });
  }, [agentId]);

  useEffect(() => {
    setSelection(agent?.model ?? '');
  }, [agent?.model]);

  useEffect(() => {
    setReasoningEffort(agent?.reasoning_effort ?? '');
  }, [agent?.reasoning_effort]);

  useEffect(() => {
    setWebSearch(agent?.web_search ?? false);
  }, [agent?.web_search]);

  // サーバーが「model_pricing に context_window が無い」ときに返す文言（process.rs:958）。
  const UNREGISTERED_MARKER = 'has no context_window registered in model_pricing';

  // patchAgent を実行し、失敗ならエラー文字列を返す（成功なら null）。
  const runPatch = async (): Promise<string | null> => {
    const res = await patchAgent(agentId, {
      model: selection === '' ? null : selection,
      // 既定選択時は空文字を送る（サーバー側で NULL に正規化）。null は
      // serde の都合で「変更なし」に潰れてクリアできないため。
      reasoning_effort: reasoningEffort,
      web_search: webSearch,
    });
    if (res.updated) return null;
    return res.error ?? t('agentDetail.modelSaveFailed');
  };

  const save = async () => {
    setSaving(true);
    setMessage(null);
    setUnregisteredSpec(null);
    try {
      const err = await runPatch();
      if (!err) {
        setMessage(t('agentDetail.modelSaved'));
      } else if (err.includes(UNREGISTERED_MARKER)) {
        // エラー文から失敗した spec を拾う（無ければ選択中の spec）。その場に
        // 登録フォームを出す導線。ターミナルで curl を叩く必要をなくす。
        const m = err.match(/model "([^"]+)"/);
        setUnregisteredSpec(m ? m[1] : selection);
        setMessage(t('agentDetail.modelUnregisteredHint'));
      } else {
        setMessage(err);
      }
    } catch (e) {
      setMessage(String(e));
    } finally {
      setSaving(false);
    }
  };

  // 登録が済んだら、元々やろうとしていたモデル保存を自動で再試行する。
  // 「登録できたのか / 保存できたのか」を分けて見せる（登録は成功したが保存が
  // 別理由で失敗する経路があるため）。
  const onPricingRegistered = async () => {
    setUnregisteredSpec(null);
    setSaving(true);
    try {
      const err = await runPatch();
      if (!err) {
        setMessage(t('agentDetail.modelRegisteredAndSaved'));
      } else {
        setMessage(t('agentDetail.modelRegisteredButSaveFailed', { error: err }));
      }
    } catch (e) {
      setMessage(t('agentDetail.modelRegisteredButSaveFailed', { error: String(e) }));
    } finally {
      setSaving(false);
    }
  };

  return (
    <div className="card-outlined mt-6">
      <h2 className="section-title flex items-center gap-2">
        <span className="material-symbols-outlined text-xl text-primary">smart_toy</span>
        {t('agentDetail.llmModel')}
      </h2>
      <p className="text-body-sm text-on-surface-variant mb-3">
        {t('agentDetail.llmModelDesc', { default: defaultModel || '—' })}
      </p>
      {message && (
        <p className="text-body-sm mb-2 text-on-surface-variant">{message}</p>
      )}
      <div className="flex flex-col sm:flex-row gap-3 items-stretch sm:items-end">
        <div className="flex-1">
          <label className="text-label-lg text-on-surface-variant block mb-1">
            {t('agentDetail.modelSelect')}
          </label>
          <select
            className="input w-full"
            value={selection}
            onChange={(e) => setSelection(e.target.value)}
          >
            <option value="">{t('agentDetail.useServerDefault')}</option>
            {choices.map((m) => (
              <option key={m} value={m}>
                {m}
              </option>
            ))}
          </select>
        </div>
        <div className="sm:w-48">
          <label className="text-label-lg text-on-surface-variant block mb-1">
            {t('agentDetail.thinkingLevel')}
          </label>
          <select
            className="input w-full"
            value={reasoningEffort}
            onChange={(e) => setReasoningEffort(e.target.value)}
          >
            <option value="">{t('agentDetail.thinkingDefault')}</option>
            <option value="minimal">minimal</option>
            <option value="low">low</option>
            <option value="medium">medium</option>
            <option value="high">high</option>
            <option value="xhigh">xhigh</option>
          </select>
        </div>
        <button
          type="button"
          className="btn-filled"
          disabled={saving}
          onClick={() => void save()}
        >
          {saving ? t('common.saving') : t('common.save')}
        </button>
      </div>
      <label className="mt-3 flex items-start gap-2 cursor-pointer">
        <input
          type="checkbox"
          className="mt-1"
          checked={webSearch}
          onChange={(e) => setWebSearch(e.target.checked)}
        />
        <span>
          <span className="text-label-lg text-on-surface block">
            {t('agentDetail.webSearch')}
          </span>
          <span className="text-body-sm text-on-surface-variant">
            {t('agentDetail.webSearchDesc')}
          </span>
        </span>
      </label>

      {unregisteredSpec && (
        <div className="mt-4">
          <p className="text-label-lg text-on-surface mb-2">
            {t('agentDetail.registerModelTitle', { spec: unregisteredSpec })}
          </p>
          <ModelPricingForm
            initial={splitModelSpec(unregisteredSpec)}
            submitLabel={t('agentDetail.registerAndSave')}
            onSaved={onPricingRegistered}
            onCancel={() => setUnregisteredSpec(null)}
          />
        </div>
      )}
    </div>
  );
}

// "provider:model" 形式の spec を登録フォームの初期値に分解する。
function splitModelSpec(spec: string): { provider?: string; model?: string } {
  const i = spec.indexOf(':');
  if (i < 0) return { model: spec };
  return { provider: spec.slice(0, i), model: spec.slice(i + 1) };
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
          <DetailRow
            label={t('agentDetail.effectiveModel')}
            value={agent.model ?? t('agentDetail.useServerDefault')}
          />
        </div>
      </div>

      <LlmModelSection agentId={agentId} />

      {/* Discord Bot */}
      <DiscordBotSection agentId={agentId} />

      {/* Nostr sub-gateway */}
      <NostrSection agentId={agentId} />

      {/* Nostr 受信 → Discord 転記先 */}
      <NostrRelaySection agentId={agentId} />

      {/* MCP servers */}
      <McpSection agentId={agentId} />
    </>
  );
}

function McpSection({ agentId }: { agentId: string }) {
  const { t } = useTranslation();
  const [servers, setServers] = useState<McpServerDto[]>([]);
  const [name, setName] = useState('');
  const [command, setCommand] = useState('');
  const [args, setArgs] = useState('');
  const [env, setEnv] = useState('');
  const [trustedOnly, setTrustedOnly] = useState(true);
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      const res = await listMcpServers(agentId);
      setServers(res.servers);
    } catch {
      setServers([]);
    }
  }, [agentId]);

  useEffect(() => {
    void load();
  }, [load]);

  const runTest = async (s: McpServerDto) => {
    setMessage(`${s.name}: テスト中...`);
    try {
      const r = await testMcpServer(agentId, s.name);
      setMessage(
        r.ok
          ? `${s.name}: ✅ 接続OK（ツール ${r.tools ?? 0}）`
          : `${s.name}: ❌ 接続失敗 — ${r.error ?? '不明'}`,
      );
    } catch (e) {
      setMessage(`${s.name}: ❌ ${String(e)}`);
    }
  };

  // "KEY=value" 行を { KEY: value } に。値の無い行は無視。
  const parseEnv = (s: string): Record<string, string> => {
    const out: Record<string, string> = {};
    for (const line of s.split('\n')) {
      const idx = line.indexOf('=');
      if (idx <= 0) continue;
      const k = line.slice(0, idx).trim();
      const v = line.slice(idx + 1).trim();
      if (k) out[k] = v;
    }
    return out;
  };

  const add = async (enabled: boolean) => {
    setBusy(true);
    setMessage(null);
    try {
      await putMcpServer(agentId, {
        name: name.trim(),
        command: command.trim(),
        args: args
          .split(/\s+/)
          .map((x) => x.trim())
          .filter((x) => x.length > 0),
        env: parseEnv(env),
        trusted_only: trustedOnly,
        enabled,
      });
      setName('');
      setCommand('');
      setArgs('');
      setEnv('');
      setTrustedOnly(true);
      setMessage(t('common.save') + ' OK');
      await load();
    } catch (e) {
      setMessage(String(e));
    } finally {
      setBusy(false);
    }
  };

  const toggle = async (s: McpServerDto) => {
    setBusy(true);
    try {
      await setMcpEnabled(agentId, s.name, !s.enabled);
      await load();
    } catch (e) {
      setMessage(String(e));
    } finally {
      setBusy(false);
    }
  };

  const remove = async (s: McpServerDto) => {
    if (!window.confirm(t('agentDetail.mcpDeleteConfirm', { name: s.name }))) return;
    setBusy(true);
    try {
      await deleteMcpServer(agentId, s.name);
      await load();
    } catch (e) {
      setMessage(String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="card-outlined mt-6">
      <h2 className="section-title flex items-center gap-2">
        <span className="material-symbols-outlined text-xl text-primary">extension</span>
        {t('agentDetail.mcp')}
      </h2>
      <p className="text-body-sm text-on-surface-variant mb-3">{t('agentDetail.mcpDesc')}</p>
      {message && <p className="text-body-sm mb-2 text-on-surface-variant">{message}</p>}

      {/* 既存サーバ一覧 */}
      <div className="space-y-2 mb-4">
        {servers.length === 0 && (
          <p className="text-body-sm text-tertiary">{t('agentDetail.mcpNoServers')}</p>
        )}
        {servers.map((s) => (
          <div
            key={s.name}
            className="flex flex-wrap items-center gap-2 border border-outline-variant rounded-lg p-2"
          >
            <span className="font-medium">{s.name}</span>
            <code className="text-body-sm text-on-surface-variant">
              {s.command} {s.args.join(' ')}
            </code>
            {s.env_keys.length > 0 && (
              <code className="text-body-sm text-tertiary">env: {s.env_keys.join(', ')}</code>
            )}
            {s.trusted_only && (
              <span className="text-label-sm px-1.5 py-0.5 rounded bg-surface-variant">
                {t('agentDetail.mcpTrustedOnly')}
              </span>
            )}
            {s.enabled && s.connected && (
              <span className="text-label-sm text-tertiary">
                ● {t('agentDetail.mcpConnected', { count: s.tools ?? 0 })}
              </span>
            )}
            {s.enabled && !s.connected && (
              <span className="text-label-sm text-error">● {t('agentDetail.mcpDisconnected')}</span>
            )}
            {!s.connected && s.connect_error && (
              <code
                className="text-label-sm text-error max-w-full truncate"
                title={s.connect_error}
              >
                {s.connect_error}
              </code>
            )}
            <span className="flex-1" />
            <button type="button" className="btn-text" disabled={busy} onClick={() => void runTest(s)}>
              接続テスト
            </button>
            <button
              type="button"
              className="btn-text"
              disabled={busy}
              onClick={() => void toggle(s)}
            >
              {s.enabled ? t('agentDetail.nostrDisable') : t('agentDetail.nostrEnable')}
            </button>
            <button
              type="button"
              className="btn-text text-error"
              disabled={busy}
              onClick={() => void remove(s)}
            >
              {t('common.delete')}
            </button>
          </div>
        ))}
      </div>

      {/* 追加フォーム */}
      <div className="space-y-2">
        <div className="grid grid-cols-1 sm:grid-cols-2 gap-2">
          <input
            className="input w-full"
            placeholder={t('agentDetail.mcpNamePlaceholder')}
            value={name}
            onChange={(e) => setName(e.target.value)}
          />
          <input
            className="input w-full"
            placeholder={t('agentDetail.mcpCommandPlaceholder')}
            value={command}
            onChange={(e) => setCommand(e.target.value)}
          />
        </div>
        <input
          className="input w-full"
          placeholder={t('agentDetail.mcpArgsPlaceholder')}
          value={args}
          onChange={(e) => setArgs(e.target.value)}
        />
        <textarea
          className="input w-full font-mono text-body-sm"
          rows={2}
          placeholder={t('agentDetail.mcpEnvPlaceholder')}
          value={env}
          onChange={(e) => setEnv(e.target.value)}
        />
        <p className="text-body-sm text-on-surface-variant">{t('agentDetail.mcpEnvWarning')}</p>
        <label className="flex items-center gap-2 text-body-sm text-on-surface-variant">
          <input
            type="checkbox"
            checked={trustedOnly}
            onChange={(e) => setTrustedOnly(e.target.checked)}
          />
          {t('agentDetail.mcpTrustedOnlyHint')}
        </label>
        <div className="flex flex-wrap gap-2">
          <button
            type="button"
            className="btn-filled"
            disabled={busy || name.trim() === '' || command.trim() === ''}
            onClick={() => void add(true)}
          >
            {t('agentDetail.mcpAddEnable')}
          </button>
          <button
            type="button"
            className="btn-outlined"
            disabled={busy || name.trim() === '' || command.trim() === ''}
            onClick={() => void add(false)}
          >
            {t('agentDetail.mcpAddDisabled')}
          </button>
        </div>
      </div>
    </div>
  );
}

function NostrSection({ agentId }: { agentId: string }) {
  const { t } = useTranslation();
  const [cfg, setCfg] = useState<NostrConfigDto | null>(null);
  const [secretKey, setSecretKey] = useState('');
  const [vanityPrefix, setVanityPrefix] = useState('');
  const [relays, setRelays] = useState('');
  const [authors, setAuthors] = useState('');
  const [keywords, setKeywords] = useState('');
  const [kinds, setKinds] = useState('');
  const [enabled, setEnabled] = useState(false);
  const [saving, setSaving] = useState(false);
  const [message, setMessage] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      const c = await getNostrConfig(agentId);
      setCfg(c);
      setRelays(c.relays.join(', '));
      setAuthors(c.filter.authors.join(', '));
      setKeywords(c.filter.keywords.join(', '));
      setKinds(c.filter.kinds.join(', '));
      setEnabled(c.enabled);
    } catch {
      setCfg(null);
    }
  }, [agentId]);

  useEffect(() => {
    void load();
  }, [load]);

  const splitList = (s: string) =>
    s
      .split(',')
      .map((x) => x.trim())
      .filter((x) => x.length > 0);

  const save = async (nextEnabled: boolean) => {
    setSaving(true);
    setMessage(null);
    try {
      const res = await updateNostrConfig(agentId, {
        secret_key: secretKey.trim() === '' ? undefined : secretKey.trim(),
        relays: splitList(relays),
        authors: splitList(authors),
        keywords: splitList(keywords),
        kinds: splitList(kinds)
          .map((k) => parseInt(k, 10))
          .filter((n) => !Number.isNaN(n)),
        enabled: nextEnabled,
      });
      setEnabled(res.enabled);
      setSecretKey('');
      setMessage(t('common.save') + ' OK');
      await load();
    } catch (e) {
      setMessage(String(e));
    } finally {
      setSaving(false);
    }
  };

  const remove = async () => {
    setSaving(true);
    try {
      await deleteNostrConfig(agentId);
      setMessage('deleted');
      await load();
    } catch (e) {
      setMessage(String(e));
    } finally {
      setSaving(false);
    }
  };

  const generate = async () => {
    // 既存鍵があるなら上書き確認（アイデンティティ喪失を防ぐ）。
    const overwrite = Boolean(cfg?.has_secret_key);
    if (overwrite && !window.confirm(t('agentDetail.nostrGenerateOverwriteConfirm'))) {
      return;
    }
    setSaving(true);
    setMessage(null);
    try {
      const res = await generateNostrKey(agentId, {
        prefix: vanityPrefix.trim() === '' ? undefined : vanityPrefix.trim(),
        overwrite,
      });
      setVanityPrefix('');
      setSecretKey('');
      setMessage(t('agentDetail.nostrGenerated', { npub: res.npub }));
      await load();
    } catch (e) {
      setMessage(String(e));
    } finally {
      setSaving(false);
    }
  };

  return (
    <div className="card-outlined mt-6">
      <h2 className="section-title flex items-center gap-2">
        <span className="material-symbols-outlined text-xl text-primary">hub</span>
        {t('agentDetail.nostr')}
      </h2>
      <p className="text-body-sm text-on-surface-variant mb-3">
        {t('agentDetail.nostrDesc')}
        {cfg?.running && (
          <span className="ml-2 text-tertiary">● {t('agentDetail.nostrRunning')}</span>
        )}
      </p>
      {message && <p className="text-body-sm mb-2 text-on-surface-variant">{message}</p>}
      <div className="space-y-3">
        <div>
          <label className="text-label-lg text-on-surface-variant block mb-1">
            {t('agentDetail.nostrSecretKey')}
          </label>
          <input
            className="input w-full"
            type="password"
            placeholder={cfg?.has_secret_key ? cfg.secret_key_masked : 'nsec1...'}
            value={secretKey}
            onChange={(e) => setSecretKey(e.target.value)}
          />
          <p className="text-body-sm text-on-surface-variant mt-2">
            {t('agentDetail.nostrGenerateHint')}
          </p>
          <div className="flex flex-wrap items-center gap-2 mt-1">
            <input
              className="input flex-1 min-w-[8rem]"
              placeholder={t('agentDetail.nostrVanityPlaceholder')}
              value={vanityPrefix}
              onChange={(e) => setVanityPrefix(e.target.value)}
            />
            <button
              type="button"
              className="btn-outlined"
              disabled={saving}
              onClick={() => void generate()}
            >
              {t('agentDetail.nostrGenerate')}
            </button>
          </div>
        </div>
        <div>
          <label className="text-label-lg text-on-surface-variant block mb-1">
            {t('agentDetail.nostrRelays')}
          </label>
          <input
            className="input w-full"
            placeholder="wss://yabu.me, wss://r.kojira.io"
            value={relays}
            onChange={(e) => setRelays(e.target.value)}
          />
        </div>
        <div className="grid grid-cols-1 sm:grid-cols-3 gap-3">
          <div>
            <label className="text-label-lg text-on-surface-variant block mb-1">
              {t('agentDetail.nostrAuthors')}
            </label>
            <input
              className="input w-full"
              placeholder="npub1..., npub1..."
              value={authors}
              onChange={(e) => setAuthors(e.target.value)}
            />
          </div>
          <div>
            <label className="text-label-lg text-on-surface-variant block mb-1">
              {t('agentDetail.nostrKeywords')}
            </label>
            <input
              className="input w-full"
              placeholder="opencrab, ..."
              value={keywords}
              onChange={(e) => setKeywords(e.target.value)}
            />
          </div>
          <div>
            <label className="text-label-lg text-on-surface-variant block mb-1">
              {t('agentDetail.nostrKinds')}
            </label>
            <input
              className="input w-full"
              placeholder="1"
              value={kinds}
              onChange={(e) => setKinds(e.target.value)}
            />
          </div>
        </div>
        <p className="text-body-sm text-on-surface-variant">{t('agentDetail.nostrFilterHint')}</p>
        <div className="flex flex-wrap gap-2">
          <button
            type="button"
            className="btn-filled"
            disabled={saving}
            onClick={() => void save(true)}
          >
            {enabled ? t('agentDetail.nostrSaveRestart') : t('agentDetail.nostrEnable')}
          </button>
          {enabled && (
            <button
              type="button"
              className="btn-outlined"
              disabled={saving}
              onClick={() => void save(false)}
            >
              {t('agentDetail.nostrDisable')}
            </button>
          )}
          {cfg?.configured && (
            <button
              type="button"
              className="btn-text text-error"
              disabled={saving}
              onClick={() => void remove()}
            >
              {t('common.delete')}
            </button>
          )}
        </div>
      </div>
    </div>
  );
}

/**
 * Nostr 受信 → Discord 転記先の設定（issue #252 段階 B）。
 *
 * 自分宛の Nostr 受信（メンション/リプライ/DM）を、指定した Discord チャンネルの
 * webhook へ転記する。webhook URL の生値は API から返らない（伏字のみ）ため、入力欄は
 * 常に空で、現在値は伏字表示する。
 *
 * webhook_url は三状態（省略=保持 / null=消去 / 文字列=設定）。保存で入力欄が空なら
 * webhook_url を**送らず現状維持**する（enabled トグルだけの保存で既存転記先が消えない）。
 * 消去は「転記先を削除」ボタンの明示操作に分離する。
 */
export function NostrRelaySection({ agentId }: { agentId: string }) {
  const { t } = useTranslation();
  const [cfg, setCfg] = useState<NostrRelayConfigDto | null>(null);
  const [enabled, setEnabled] = useState(false);
  const [webhookUrl, setWebhookUrl] = useState('');
  const [saving, setSaving] = useState(false);
  const [message, setMessage] = useState<string | null>(null);
  const [warning, setWarning] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      const c = await getNostrRelayConfig(agentId);
      setCfg(c);
      setEnabled(c.enabled);
      // 生 URL は取得できない（伏字のみ）。入力欄は空のまま上書き入力させる。
      setWebhookUrl('');
    } catch {
      setCfg(null);
    }
  }, [agentId]);

  useEffect(() => {
    void load();
  }, [load]);

  // 入力欄が空なら webhook_url を送らず（保持）、入力があればそれを設定する。
  const save = async () => {
    setSaving(true);
    setMessage(null);
    setWarning(null);
    try {
      const trimmed = webhookUrl.trim();
      const res = await updateNostrRelayConfig(agentId, {
        enabled,
        // 空欄 = 現状維持なのでフィールド自体を送らない（undefined は JSON から除かれる）。
        ...(trimmed === '' ? {} : { webhook_url: trimmed }),
      });
      setEnabled(res.enabled);
      setWebhookUrl('');
      setMessage(t('common.save') + ' OK');
      if (res.warning) setWarning(res.warning);
      await load();
    } catch (e) {
      setMessage(String(e));
    } finally {
      setSaving(false);
    }
  };

  // 転記先を明示的に消去する（null を送る）。誤操作を避けるため確認する。
  const clearWebhook = async () => {
    if (!window.confirm(t('agentDetail.nostrRelayDeleteConfirm'))) return;
    setSaving(true);
    setMessage(null);
    setWarning(null);
    try {
      const res = await updateNostrRelayConfig(agentId, {
        enabled,
        webhook_url: null,
      });
      setEnabled(res.enabled);
      setWebhookUrl('');
      setMessage(t('common.save') + ' OK');
      if (res.warning) setWarning(res.warning);
      await load();
    } catch (e) {
      setMessage(String(e));
    } finally {
      setSaving(false);
    }
  };

  return (
    <div className="card-outlined mt-6">
      <h2 className="section-title flex items-center gap-2">
        <span className="material-symbols-outlined text-xl text-primary">forward_to_inbox</span>
        {t('agentDetail.nostrRelay')}
      </h2>
      <p className="text-body-sm text-on-surface-variant mb-3">
        {t('agentDetail.nostrRelayDesc')}
      </p>
      {message && <p className="text-body-sm mb-2 text-on-surface-variant">{message}</p>}
      {warning && <p className="text-body-sm mb-2 text-error">{warning}</p>}
      <div className="space-y-3">
        <label className="flex items-center gap-2">
          <input
            type="checkbox"
            checked={enabled}
            onChange={(e) => setEnabled(e.target.checked)}
          />
          <span className="text-label-lg">{t('agentDetail.nostrRelayEnabled')}</span>
        </label>
        <div>
          <label className="text-label-lg text-on-surface-variant block mb-1">
            {t('agentDetail.nostrRelayWebhook')}
          </label>
          {cfg?.has_webhook && (
            <p className="text-body-sm text-on-surface-variant mb-1">
              {t('agentDetail.nostrRelayCurrent', { url: cfg.webhook_url_masked })}
            </p>
          )}
          <input
            className="input w-full"
            placeholder="https://discord.com/api/webhooks/..."
            value={webhookUrl}
            onChange={(e) => setWebhookUrl(e.target.value)}
          />
          <p className="text-body-sm text-on-surface-variant mt-1">
            {t('agentDetail.nostrRelayWebhookHint')}
          </p>
        </div>
        <div className="flex flex-wrap gap-2">
          <button
            type="button"
            className="btn-filled"
            disabled={saving}
            onClick={() => void save()}
          >
            {t('common.save')}
          </button>
          {cfg?.has_webhook && (
            <button
              type="button"
              className="btn-text text-error"
              disabled={saving}
              onClick={() => void clearWebhook()}
            >
              {t('agentDetail.nostrRelayDelete')}
            </button>
          )}
        </div>
      </div>
    </div>
  );
}
