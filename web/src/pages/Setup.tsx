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
import { createAgent } from '../api/agents';

export default function Setup() {
  const { t } = useTranslation();
  const [status, setStatus] = useState<SetupStatus | null>(null);
  const [current, setCurrent] = useState(0);

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
    // 初回は未完の最初のステップにフォーカスする。
    refresh().then((s) => {
      if (s?.next_step) {
        const idx = STEP_ORDER.indexOf(s.next_step);
        if (idx >= 0) setCurrent(idx);
      }
    });
  }, [refresh]);

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
            onCreated={() => {
              refresh();
            }}
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
          {['openai', 'anthropic', 'google', 'openrouter', 'ollama', 'llamacpp', 'codex', 'cursor', 'acp', 'chatgpt'].map(
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
