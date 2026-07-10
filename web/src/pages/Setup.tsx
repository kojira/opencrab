import { useState, useEffect, useCallback } from 'react';
import { Link } from 'react-router-dom';
import { useTranslation } from 'react-i18next';
import {
  getSetupStatus,
  seedStandardSkills,
  STEP_ORDER,
  type StepKey,
  type SetupStatus,
} from '../api/setup';
import {
  getLlmProviders,
  updateLlmProvider,
  type LlmProviderInfo,
} from '../api/providers';
import { getAgents, createAgent, updateDiscordConfig } from '../api/agents';
import { upsertChannelConfig } from '../api/channel_configs';
import type { AgentSummary } from '../api/types';

export default function Setup() {
  const { t } = useTranslation();
  const [status, setStatus] = useState<SetupStatus | null>(null);
  const [current, setCurrent] = useState(0);
  // ウィザードで作成/選択したエージェント（Discord・チャンネルステップで使う）
  const [agentId, setAgentId] = useState('');
  // エージェント一覧は親で 1 度だけ取得し、両ステップの選択に共有する。
  const [agents, setAgents] = useState<AgentSummary[]>([]);

  const reloadAgents = useCallback(() => {
    getAgents().then(setAgents).catch(() => {});
  }, []);

  const refresh = useCallback(async () => {
    try {
      const s = await getSetupStatus();
      setStatus(s);
      return s;
    } catch {
      return null;
    }
  }, []);

  useEffect(() => {
    reloadAgents();
    // 初回は未完の最初のステップにフォーカスする。
    refresh().then((s) => {
      if (s?.next_step) {
        const idx = STEP_ORDER.indexOf(s.next_step);
        if (idx >= 0) setCurrent(idx);
      }
    });
  }, [refresh, reloadAgents]);

  const stepDone = (k: StepKey): boolean => status?.steps[k]?.done ?? false;

  return (
    <div className="max-w-2xl mx-auto space-y-6">
      <div>
        <h1 className="text-xl text-on-surface font-bold">{t('setup.title')}</h1>
        <p className="text-xs text-on-surface-variant mt-0.5">{t('setup.subtitle')}</p>
      </div>

      {/* Stepper */}
      <div className="flex items-center gap-1 flex-wrap">
        {STEP_ORDER.map((k, i) => {
          const done = stepDone(k);
          const active = i === current;
          return (
            <button
              key={k}
              onClick={() => setCurrent(i)}
              className={`flex items-center gap-1.5 px-3 py-1.5 rounded-full text-sm transition-colors ${
                active
                  ? 'bg-primary text-white'
                  : done
                    ? 'bg-success/15 text-success'
                    : 'bg-surface-container text-on-surface-variant'
              }`}
            >
              <span className="material-symbols-outlined text-base">
                {done ? 'check_circle' : 'radio_button_unchecked'}
              </span>
              <span>{t(`setup.step.${k}`)}</span>
            </button>
          );
        })}
      </div>

      {status?.complete && (
        <div className="card-outlined border-success bg-success/10 p-4 flex items-start gap-3">
          <span className="material-symbols-outlined text-success">celebration</span>
          <div>
            <p className="text-sm font-semibold text-on-surface">{t('setup.complete.title')}</p>
            <p className="text-xs text-on-surface-variant mt-0.5">{t('setup.complete.body')}</p>
            <Link to="/" className="btn-tonal text-sm mt-3 inline-flex">
              {t('setup.complete.goHome')}
            </Link>
          </div>
        </div>
      )}

      {/* Step body */}
      <div className="card-elevated">
        {current === 0 && (
          <LlmStep
            onDone={refresh}
            done={stepDone('llm_provider')}
            defaultProvider={status?.steps.llm_provider.default_provider}
          />
        )}
        {current === 1 && (
          <AgentStep
            onCreated={(id) => {
              setAgentId(id);
              reloadAgents();
              refresh();
            }}
          />
        )}
        {current === 2 && (
          <DiscordStep
            agentId={agentId}
            setAgentId={setAgentId}
            agents={agents}
            onDone={refresh}
          />
        )}
        {current === 3 && (
          <ChannelStep
            agentId={agentId}
            setAgentId={setAgentId}
            agents={agents}
            onDone={refresh}
          />
        )}
      </div>

      {/* Nav */}
      <div className="flex justify-between">
        <button
          className="btn-outlined"
          disabled={current === 0}
          onClick={() => setCurrent((c) => Math.max(0, c - 1))}
        >
          {t('setup.back')}
        </button>
        <button
          className="btn-filled"
          disabled={current === STEP_ORDER.length - 1}
          onClick={() => setCurrent((c) => Math.min(STEP_ORDER.length - 1, c + 1))}
        >
          {t('setup.next')}
        </button>
      </div>
    </div>
  );
}

function FieldLabel({ children }: { children: React.ReactNode }) {
  return <label className="block text-label-lg text-on-surface mb-2">{children}</label>;
}

function ErrorBox({ message }: { message: string }) {
  return (
    <div className="flex items-center gap-2 p-3 rounded-md bg-error-container">
      <span className="material-symbols-outlined text-error text-base">error</span>
      <p className="text-body-sm text-error-on-container break-all">{message}</p>
    </div>
  );
}

function OkBox({ message }: { message: string }) {
  return (
    <div className="flex items-center gap-2 p-3 rounded-md bg-success/15">
      <span className="material-symbols-outlined text-success text-base">check_circle</span>
      <p className="text-body-sm text-success">{message}</p>
    </div>
  );
}

// ---- Step 1: LLM provider ----
function LlmStep({
  onDone,
  done,
  defaultProvider,
}: {
  onDone: () => void;
  done: boolean;
  defaultProvider?: string;
}) {
  const { t } = useTranslation();
  const [providers, setProviders] = useState<LlmProviderInfo[]>([]);
  const [name, setName] = useState(defaultProvider || 'openai');
  const [apiKey, setApiKey] = useState('');
  const [baseUrl, setBaseUrl] = useState('');
  const [defaultModel, setDefaultModel] = useState('');
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);

  useEffect(() => {
    getLlmProviders()
      .then((r) => setProviders(r.providers))
      .catch(() => {});
  }, [saved]);

  const active = providers.filter((p) => p.active).map((p) => p.name);

  const save = async () => {
    setSaving(true);
    setError(null);
    try {
      await updateLlmProvider(name, {
        enabled: true,
        api_key: apiKey || undefined,
        base_url: baseUrl || undefined,
        default_model: defaultModel || undefined,
      });
      setSaved(true);
      onDone();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setSaving(false);
    }
  };

  return (
    <div className="space-y-5">
      <div>
        <h2 className="text-lg font-semibold text-on-surface">{t('setup.llm.title')}</h2>
        <p className="text-body-sm text-on-surface-variant mt-1">{t('setup.llm.desc')}</p>
      </div>

      {done ? (
        <OkBox message={t('setup.llm.active', { list: active.join(', ') })} />
      ) : (
        <div className="flex items-center gap-2 p-3 rounded-md bg-primary/10">
          <span className="material-symbols-outlined text-primary text-base">info</span>
          <p className="text-body-sm text-on-surface-variant">
            {t('setup.llm.needsKey', { provider: defaultProvider || 'openai' })}
          </p>
        </div>
      )}

      <div>
        <FieldLabel>{t('setup.llm.provider')}</FieldLabel>
        <select
          className="input-outlined"
          value={name}
          onChange={(e) => setName(e.target.value)}
        >
          {['openai', 'anthropic', 'google', 'openrouter', 'ollama', 'llamacpp', 'codex', 'chatgpt'].map(
            (p) => (
              <option key={p} value={p}>
                {p}
              </option>
            ),
          )}
        </select>
      </div>
      <div>
        <FieldLabel>{t('setup.llm.apiKey')}</FieldLabel>
        <input
          type="password"
          className="input-outlined"
          value={apiKey}
          placeholder="sk-..."
          onChange={(e) => setApiKey(e.target.value)}
        />
      </div>
      <div>
        <FieldLabel>{t('setup.llm.baseUrl')}</FieldLabel>
        <input
          type="text"
          className="input-outlined"
          value={baseUrl}
          placeholder="https://api.openai.com/v1"
          onChange={(e) => setBaseUrl(e.target.value)}
        />
      </div>
      <div>
        <FieldLabel>{t('setup.llm.defaultModel')}</FieldLabel>
        <input
          type="text"
          className="input-outlined"
          value={defaultModel}
          onChange={(e) => setDefaultModel(e.target.value)}
        />
      </div>

      {error && <ErrorBox message={error} />}
      {saved && !error && <OkBox message={t('setup.llm.saved')} />}

      <div className="flex items-center gap-3">
        <button className="btn-filled" disabled={saving} onClick={save}>
          {saving ? t('common.saving') : t('setup.llm.save')}
        </button>
        <Link to="/settings" className="btn-text text-sm">
          {t('setup.llm.advanced')}
        </Link>
      </div>
      <p className="text-body-sm text-on-surface-variant">{t('setup.llm.readyNote')}</p>
    </div>
  );
}

// ---- Step 2: Create agent + seed skills ----
function AgentStep({ onCreated }: { onCreated: (id: string) => void }) {
  const { t } = useTranslation();
  const [name, setName] = useState('');
  const [personaName, setPersonaName] = useState('');
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [seedWarning, setSeedWarning] = useState<string | null>(null);
  const [result, setResult] = useState<{ id: string; seeded: number } | null>(null);

  const create = async () => {
    if (!name.trim()) {
      setError(t('setup.agent.nameRequired'));
      return;
    }
    setSaving(true);
    setError(null);
    setSeedWarning(null);
    try {
      const agent = await createAgent({
        name: name.trim(),
        persona_name: personaName.trim() || name.trim(),
      });
      // 作成直後に標準スキルをシード（作ってすぐ使える状態にする）。
      // 失敗してもエージェント自体は作成済みなので致命的ではないが、
      // 握り潰すとスキル無しエージェントが無言で出来上がるため必ず表示する。
      let seeded = 0;
      try {
        const seed = await seedStandardSkills(agent.id);
        seeded = seed.seeded_count;
        if (seed.errors.length > 0) {
          setSeedWarning(t('setup.agent.seedErrors', { errors: seed.errors.join('; ') }));
        }
      } catch (e) {
        setSeedWarning(
          t('setup.agent.seedFailed', {
            message: e instanceof Error ? e.message : String(e),
          }),
        );
      }
      setResult({ id: agent.id, seeded });
      onCreated(agent.id);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setSaving(false);
    }
  };

  return (
    <div className="space-y-5">
      <div>
        <h2 className="text-lg font-semibold text-on-surface">{t('setup.agent.title')}</h2>
        <p className="text-body-sm text-on-surface-variant mt-1">{t('setup.agent.desc')}</p>
      </div>

      {result ? (
        <div className="space-y-3">
          <OkBox message={t('setup.agent.created', { id: result.id })} />
          {seedWarning ? (
            <ErrorBox message={seedWarning} />
          ) : (
            <OkBox message={t('setup.agent.seeded', { count: result.seeded })} />
          )}
          <Link to={`/agents/${result.id}`} className="btn-tonal text-sm inline-flex">
            {t('setup.agent.openAgent')}
          </Link>
        </div>
      ) : (
        <>
          <div>
            <FieldLabel>{t('setup.agent.name')}</FieldLabel>
            <input
              type="text"
              className="input-outlined"
              value={name}
              onChange={(e) => setName(e.target.value)}
            />
          </div>
          <div>
            <FieldLabel>{t('setup.agent.persona')}</FieldLabel>
            <input
              type="text"
              className="input-outlined"
              value={personaName}
              onChange={(e) => setPersonaName(e.target.value)}
            />
          </div>
          {error && <ErrorBox message={error} />}
          <button className="btn-filled" disabled={saving} onClick={create}>
            {saving ? t('common.creating') : t('setup.agent.create')}
          </button>
        </>
      )}
    </div>
  );
}

// エージェント選択ドロップダウン（Discord/チャンネルステップ共通）。
// 一覧は親（Setup）が保持し、prop で受け取る（ステップごとの重複フェッチを避ける）。
function AgentPicker({
  agentId,
  setAgentId,
  agents,
}: {
  agentId: string;
  setAgentId: (id: string) => void;
  agents: AgentSummary[];
}) {
  const { t } = useTranslation();
  return (
    <div>
      <FieldLabel>{t('setup.selectAgent')}</FieldLabel>
      <select
        className="input-outlined"
        value={agentId}
        onChange={(e) => setAgentId(e.target.value)}
      >
        <option value="">{t('common.selectAgentPlaceholder')}</option>
        {agents.map((a) => (
          <option key={a.id} value={a.id}>
            {a.name} ({a.id.slice(0, 8)})
          </option>
        ))}
      </select>
    </div>
  );
}

// ---- Step 3: Discord ----
function DiscordStep({
  agentId,
  setAgentId,
  agents,
  onDone,
}: {
  agentId: string;
  setAgentId: (id: string) => void;
  agents: AgentSummary[];
  onDone: () => void;
}) {
  const { t } = useTranslation();
  const [token, setToken] = useState('');
  const [ownerId, setOwnerId] = useState('');
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [msg, setMsg] = useState<string | null>(null);

  const save = async () => {
    if (!agentId) {
      setError(t('setup.discord.needAgent'));
      return;
    }
    setSaving(true);
    setError(null);
    setMsg(null);
    try {
      const res = await updateDiscordConfig(agentId, {
        bot_token: token,
        owner_discord_id: ownerId || undefined,
      });
      if (res.ok) {
        setMsg(res.message || t('setup.discord.saved'));
        onDone();
      } else {
        setError(res.error || 'error');
      }
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setSaving(false);
    }
  };

  return (
    <div className="space-y-5">
      <div>
        <h2 className="text-lg font-semibold text-on-surface">{t('setup.discord.title')}</h2>
        <p className="text-body-sm text-on-surface-variant mt-1">{t('setup.discord.desc')}</p>
      </div>
      <AgentPicker agentId={agentId} setAgentId={setAgentId} agents={agents} />
      <div>
        <FieldLabel>{t('setup.discord.token')}</FieldLabel>
        <input
          type="password"
          className="input-outlined"
          value={token}
          onChange={(e) => setToken(e.target.value)}
        />
      </div>
      <div>
        <FieldLabel>{t('setup.discord.owner')}</FieldLabel>
        <input
          type="text"
          className="input-outlined"
          value={ownerId}
          placeholder="390732846236434452"
          onChange={(e) => setOwnerId(e.target.value)}
        />
      </div>
      {error && <ErrorBox message={error} />}
      {msg && !error && <OkBox message={msg} />}
      <button className="btn-filled" disabled={saving} onClick={save}>
        {saving ? t('common.saving') : t('setup.discord.save')}
      </button>
    </div>
  );
}

// ---- Step 4: Channel whitelist ----
function ChannelStep({
  agentId,
  setAgentId,
  agents,
  onDone,
}: {
  agentId: string;
  setAgentId: (id: string) => void;
  agents: AgentSummary[];
  onDone: () => void;
}) {
  const { t } = useTranslation();
  const [guildId, setGuildId] = useState('');
  const [channelId, setChannelId] = useState('');
  const [channelName, setChannelName] = useState('');
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [msg, setMsg] = useState<string | null>(null);

  const save = async () => {
    if (!agentId) {
      setError(t('setup.channel.needAgent'));
      return;
    }
    if (!channelId.trim()) {
      setError(t('setup.channel.needChannel'));
      return;
    }
    setSaving(true);
    setError(null);
    setMsg(null);
    try {
      await upsertChannelConfig(agentId, {
        channel_id: channelId.trim(),
        guild_id: guildId.trim(),
        channel_name: channelName.trim(),
        readable: true,
        writable: true,
        whitelisted: true,
        heartbeat_enabled: false,
        heartbeat_interval_secs: null,
      });
      setMsg(t('setup.channel.saved'));
      onDone();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setSaving(false);
    }
  };

  return (
    <div className="space-y-5">
      <div>
        <h2 className="text-lg font-semibold text-on-surface">{t('setup.channel.title')}</h2>
        <p className="text-body-sm text-on-surface-variant mt-1">{t('setup.channel.desc')}</p>
      </div>
      <AgentPicker agentId={agentId} setAgentId={setAgentId} agents={agents} />
      <div>
        <FieldLabel>{t('setup.channel.guildId')}</FieldLabel>
        <input
          type="text"
          className="input-outlined"
          value={guildId}
          onChange={(e) => setGuildId(e.target.value)}
        />
      </div>
      <div>
        <FieldLabel>{t('setup.channel.channelId')}</FieldLabel>
        <input
          type="text"
          className="input-outlined"
          value={channelId}
          onChange={(e) => setChannelId(e.target.value)}
        />
      </div>
      <div>
        <FieldLabel>{t('setup.channel.channelName')}</FieldLabel>
        <input
          type="text"
          className="input-outlined"
          value={channelName}
          onChange={(e) => setChannelName(e.target.value)}
        />
      </div>
      {error && <ErrorBox message={error} />}
      {msg && !error && <OkBox message={msg} />}
      <button className="btn-filled" disabled={saving} onClick={save}>
        {saving ? t('common.saving') : t('setup.channel.save')}
      </button>
    </div>
  );
}
