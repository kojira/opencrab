import { useState, useEffect, useCallback } from 'react';
import { getLogLevel, patchLogLevel } from '../api/system';
import {
  getLlmProviders,
  updateLlmProvider,
  resetLlmProvider,
  getVoiceConfig,
  updateVoiceConfig,
  resetVoiceConfig,
  getCodexDiagnostics,
  getCursorDiagnostics,
  LlmProviderInfo,
  UpdateProviderBody,
  VoiceConfig,
  CodexDiagnostics,
  CursorDiagnostics,
} from '../api/providers';

const LOG_LEVELS = ['debug', 'info', 'warn', 'error'];

const inputCls =
  'rounded-lg border border-outline bg-surface px-3 py-2 text-sm text-on-surface focus:outline-none focus:ring-2 focus:ring-primary w-full';
const btnPrimary =
  'rounded-lg bg-primary text-on-primary px-3 py-1.5 text-sm font-medium hover:opacity-90 disabled:opacity-50';
const btnGhost =
  'rounded-lg border border-outline px-3 py-1.5 text-sm text-on-surface-variant hover:bg-surface-variant disabled:opacity-50';

// ============ LLM プロバイダー編集フォーム ============

function ProviderEditor({
  provider,
  onSaved,
  onCancel,
}: {
  provider: LlmProviderInfo;
  onSaved: (msg: string) => void;
  onCancel: () => void;
}) {
  const [apiKey, setApiKey] = useState('');
  const [baseUrl, setBaseUrl] = useState(provider.base_url);
  const [defaultModel, setDefaultModel] = useState(provider.default_model);
  const [reasoningEffort, setReasoningEffort] = useState(provider.reasoning_effort);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const save = async () => {
    setSaving(true);
    setError(null);
    const body: UpdateProviderBody = {};
    // 空欄のままなら API キーは変更しない（マスク値の再送を防ぐ）
    if (apiKey !== '') body.api_key = apiKey;
    if (baseUrl !== provider.base_url) body.base_url = baseUrl === '' ? null : baseUrl;
    if (defaultModel !== provider.default_model)
      body.default_model = defaultModel === '' ? null : defaultModel;
    if (reasoningEffort !== provider.reasoning_effort)
      body.reasoning_effort = reasoningEffort === '' ? null : reasoningEffort;
    try {
      await updateLlmProvider(provider.name, body);
      onSaved(`${provider.name} を保存し、ルーターを再構築しました（再起動不要）`);
    } catch (e) {
      setError(String(e));
    } finally {
      setSaving(false);
    }
  };

  return (
    <div className="mt-2 space-y-3 rounded-lg border border-outline bg-surface-variant/30 p-3">
      <div className="grid gap-3 sm:grid-cols-2">
        <div>
          <label className="mb-1 block text-xs font-medium text-on-surface-variant">
            API キー{' '}
            {provider.api_key_masked && (
              <span className="font-mono">（現在: {provider.api_key_masked}）</span>
            )}
          </label>
          <input
            type="password"
            value={apiKey}
            onChange={(e) => setApiKey(e.target.value)}
            placeholder="変更する場合のみ入力"
            autoComplete="new-password"
            className={inputCls}
          />
        </div>
        <div>
          <label className="mb-1 block text-xs font-medium text-on-surface-variant">
            Base URL（空欄 = 既定）
          </label>
          <input
            value={baseUrl}
            onChange={(e) => setBaseUrl(e.target.value)}
            placeholder="https://..."
            className={inputCls}
          />
        </div>
        <div>
          <label className="mb-1 block text-xs font-medium text-on-surface-variant">
            既定モデル
          </label>
          <input
            value={defaultModel}
            onChange={(e) => setDefaultModel(e.target.value)}
            placeholder="例: gpt-4o"
            className={inputCls}
          />
        </div>
        <div>
          <label className="mb-1 block text-xs font-medium text-on-surface-variant">
            推論（thinking）強度
          </label>
          <select
            value={reasoningEffort}
            onChange={(e) => setReasoningEffort(e.target.value)}
            className={inputCls}
          >
            <option value="">モデル既定</option>
            <option value="minimal">minimal</option>
            <option value="low">low</option>
            <option value="medium">medium</option>
            <option value="high">high</option>
            <option value="xhigh">xhigh</option>
          </select>
        </div>
      </div>
      {error && <p className="text-sm text-red-500">エラー: {error}</p>}
      <div className="flex gap-2">
        <button onClick={save} disabled={saving} className={btnPrimary}>
          {saving ? '保存中...' : '保存して反映'}
        </button>
        <button onClick={onCancel} disabled={saving} className={btnGhost}>
          キャンセル
        </button>
      </div>
    </div>
  );
}

function ProviderRow({
  provider,
  onChanged,
}: {
  provider: LlmProviderInfo;
  onChanged: (msg: string) => void;
}) {
  const [editing, setEditing] = useState(false);
  const [busy, setBusy] = useState(false);

  const toggleEnabled = async () => {
    setBusy(true);
    try {
      if (provider.enabled_override === false) {
        // 無効化を解除（TOML の状態に戻す）
        await updateLlmProvider(provider.name, { enabled: null });
        onChanged(`${provider.name} の無効化を解除しました`);
      } else {
        await updateLlmProvider(provider.name, { enabled: false });
        onChanged(`${provider.name} を無効化しました`);
      }
    } catch (e) {
      onChanged(`エラー: ${String(e)}`);
    } finally {
      setBusy(false);
    }
  };

  const reset = async () => {
    if (!window.confirm(`${provider.name} のダッシュボード設定を破棄して TOML 設定に戻しますか？`))
      return;
    setBusy(true);
    try {
      await resetLlmProvider(provider.name);
      onChanged(`${provider.name} を TOML 設定に戻しました`);
    } catch (e) {
      onChanged(`エラー: ${String(e)}`);
    } finally {
      setBusy(false);
    }
  };

  const disabled = provider.enabled_override === false;
  const keySourceLabel =
    provider.api_key_source === 'db'
      ? 'キー: ダッシュボード'
      : provider.api_key_source === 'toml'
        ? 'キー: 設定ファイル'
        : 'キー: 未設定';

  return (
    <div className="border-b border-outline/40 py-3 last:border-b-0">
      <div className="flex flex-wrap items-center gap-2">
        <span
          className={`inline-block h-2.5 w-2.5 shrink-0 rounded-full ${
            provider.active ? 'bg-green-500' : 'bg-gray-400'
          }`}
          title={provider.active ? '稼働中' : '未稼働'}
        />
        <span className="font-mono text-sm font-semibold text-on-surface">{provider.name}</span>
        <span className="rounded-full bg-surface-variant px-2 py-0.5 text-xs text-on-surface-variant">
          {keySourceLabel}
        </span>
        {disabled && (
          <span className="rounded-full bg-red-500/10 px-2 py-0.5 text-xs text-red-500">
            無効化中
          </span>
        )}
        {provider.has_override && !disabled && (
          <span className="rounded-full bg-primary/10 px-2 py-0.5 text-xs text-primary">
            上書きあり
          </span>
        )}
        <div className="ml-auto flex gap-2">
          <button onClick={() => setEditing((v) => !v)} disabled={busy} className={btnGhost}>
            {editing ? '閉じる' : '編集'}
          </button>
          <button onClick={toggleEnabled} disabled={busy} className={btnGhost}>
            {disabled ? '有効に戻す' : '無効化'}
          </button>
          {provider.has_override && (
            <button onClick={reset} disabled={busy} className={btnGhost}>
              リセット
            </button>
          )}
        </div>
      </div>
      {(provider.base_url || provider.default_model || provider.reasoning_effort) && (
        <p className="mt-1 truncate pl-5 text-xs text-on-surface-variant">
          {provider.base_url && <span className="font-mono">{provider.base_url}</span>}
          {provider.base_url && provider.default_model && ' ・ '}
          {provider.default_model && <span>既定: {provider.default_model}</span>}
          {provider.reasoning_effort && (
            <span>
              {(provider.base_url || provider.default_model) && ' ・ '}
              thinking: {provider.reasoning_effort}
            </span>
          )}
        </p>
      )}
      {editing && (
        <ProviderEditor
          provider={provider}
          onSaved={(msg) => {
            setEditing(false);
            onChanged(msg);
          }}
          onCancel={() => setEditing(false)}
        />
      )}
    </div>
  );
}

// ============ Codex 診断 ============

function CodexDiagnosticsCard() {
  const [diag, setDiag] = useState<CodexDiagnostics | null>(null);
  const [loading, setLoading] = useState(false);

  const load = useCallback(async () => {
    setLoading(true);
    try {
      setDiag(await getCodexDiagnostics());
    } catch (e) {
      setDiag({
        configured_path: '',
        resolved_path: null,
        version: null,
        error: String(e),
      });
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    load();
  }, [load]);

  return (
    <div className="card-elevated space-y-3">
      <div className="flex flex-wrap items-center gap-2">
        <h2 className="text-lg font-semibold text-on-surface">Codex 診断</h2>
        <button onClick={load} disabled={loading} className={btnGhost}>
          {loading ? '確認中...' : '再確認'}
        </button>
      </div>
      <p className="text-xs text-on-surface-variant">
        opencrab の<strong>サーバープロセスが実際に使う</strong> codex のパスとバージョンです。
        ターミナルの <code className="font-mono">codex --version</code> と食い違う場合、
        サーバーが古い codex を拾っています（新しいモデルが弾かれる原因）。
        その時は <code className="font-mono">which codex</code> の絶対パスを
        <code className="font-mono">[llm.providers.codex] binary_path</code> に設定してください。
      </p>
      {diag && (
        <div className="space-y-1 text-sm">
          <div className="flex gap-2">
            <span className="w-32 shrink-0 text-on-surface-variant">バージョン</span>
            {diag.version ? (
              <span className="font-mono text-on-surface">{diag.version}</span>
            ) : (
              <span className="text-red-500">取得できませんでした</span>
            )}
          </div>
          <div className="flex gap-2">
            <span className="w-32 shrink-0 text-on-surface-variant">解決パス</span>
            <span className="font-mono text-on-surface break-all">
              {diag.resolved_path ?? '（PATH 上に見つからない）'}
            </span>
          </div>
          <div className="flex gap-2">
            <span className="w-32 shrink-0 text-on-surface-variant">設定パス</span>
            <span className="font-mono text-on-surface break-all">
              {diag.configured_path || 'codex（PATH 検索）'}
            </span>
          </div>
          {diag.error && (
            <p className="mt-1 whitespace-pre-wrap break-words text-xs text-red-500">
              {diag.error}
            </p>
          )}
        </div>
      )}
    </div>
  );
}

// ============ Cursor 診断 ============

function CursorDiagnosticsCard() {
  const [diag, setDiag] = useState<CursorDiagnostics | null>(null);
  const [loading, setLoading] = useState(false);

  const load = useCallback(async () => {
    setLoading(true);
    try {
      setDiag(await getCursorDiagnostics());
    } catch (e) {
      setDiag({
        configured_path: '',
        resolved_path: null,
        version: null,
        error: String(e),
      });
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    load();
  }, [load]);

  return (
    <div className="card-elevated space-y-3">
      <div className="flex flex-wrap items-center gap-2">
        <h2 className="text-lg font-semibold text-on-surface">Cursor 診断</h2>
        <button onClick={load} disabled={loading} className={btnGhost}>
          {loading ? '確認中...' : '再確認'}
        </button>
      </div>
      <p className="text-xs text-on-surface-variant">
        opencrab の<strong>サーバープロセスが実際に使う</strong> Cursor CLI のパスとバージョンです。
        コマンド名はインストールでゆれます（<code className="font-mono">cursor-agent</code> /{' '}
        <code className="font-mono">agent</code> / <code className="font-mono">cursor</code>）。
        解決パスが空なら <code className="font-mono">which cursor-agent</code> の絶対パスを
        <code className="font-mono">[llm.providers.cursor] binary_path</code> に設定してください。
        認証は <code className="font-mono">CURSOR_API_KEY</code> か{' '}
        <code className="font-mono">cursor-agent login</code> 済みのアンビエント認証です。
      </p>
      {diag && (
        <div className="space-y-1 text-sm">
          <div className="flex gap-2">
            <span className="w-32 shrink-0 text-on-surface-variant">バージョン</span>
            {diag.version ? (
              <span className="font-mono text-on-surface">{diag.version}</span>
            ) : (
              <span className="text-red-500">取得できませんでした</span>
            )}
          </div>
          <div className="flex gap-2">
            <span className="w-32 shrink-0 text-on-surface-variant">解決パス</span>
            <span className="font-mono text-on-surface break-all">
              {diag.resolved_path ?? '（PATH 上に見つからない）'}
            </span>
          </div>
          <div className="flex gap-2">
            <span className="w-32 shrink-0 text-on-surface-variant">設定パス</span>
            <span className="font-mono text-on-surface break-all">
              {diag.configured_path || 'cursor-agent（PATH 検索）'}
            </span>
          </div>
          {diag.error && (
            <p className="mt-1 whitespace-pre-wrap break-words text-xs text-red-500">
              {diag.error}
            </p>
          )}
        </div>
      )}
    </div>
  );
}

// ============ 音声 (VC) 設定 ============

function VoiceSettings() {
  const [config, setConfig] = useState<VoiceConfig | null>(null);
  const [source, setSource] = useState<'db' | 'toml'>('toml');
  const [runtimeActive, setRuntimeActive] = useState(false);
  const [saving, setSaving] = useState(false);
  const [message, setMessage] = useState<string | null>(null);
  const [agentVoicesText, setAgentVoicesText] = useState('');

  const load = useCallback(async () => {
    try {
      const res = await getVoiceConfig();
      setConfig(res.config);
      setSource(res.source);
      setRuntimeActive(res.runtime_active);
      setAgentVoicesText(
        Object.entries(res.config.tts.agent_voices ?? {})
          .map(([k, v]) => `${k}=${v}`)
          .join('\n'),
      );
    } catch {
      // voice API が無い旧サーバでは何も表示しない
    }
  }, []);

  useEffect(() => {
    load();
  }, [load]);

  if (!config) return null;

  const patch = (updater: (c: VoiceConfig) => VoiceConfig) =>
    setConfig((c) => (c ? updater(structuredClone(c)) : c));

  const save = async () => {
    setSaving(true);
    setMessage(null);
    // agent_voices は "agent_id=話者" の行形式から復元
    const agent_voices: Record<string, string> = {};
    for (const line of agentVoicesText.split('\n')) {
      const trimmed = line.trim();
      if (!trimmed) continue;
      const eq = trimmed.indexOf('=');
      if (eq <= 0) {
        setMessage(`エラー: agent_voices の形式が不正です: "${trimmed}"（agent_id=話者 で指定）`);
        setSaving(false);
        return;
      }
      agent_voices[trimmed.slice(0, eq).trim()] = trimmed.slice(eq + 1).trim();
    }
    try {
      const res = await updateVoiceConfig({
        ...config,
        tts: { ...config.tts, agent_voices },
      });
      setMessage(
        res.applied_live
          ? '保存しました（STT/TTS は即時反映されました）'
          : '保存しました（反映にはサーバー再起動が必要です）',
      );
      await load();
    } catch (e) {
      setMessage(`エラー: ${String(e)}`);
    } finally {
      setSaving(false);
    }
  };

  const reset = async () => {
    if (!window.confirm('音声設定のダッシュボード上書きを破棄して TOML 設定に戻しますか？')) return;
    setSaving(true);
    try {
      await resetVoiceConfig();
      setMessage('TOML 設定に戻しました（反映にはサーバー再起動が必要です）');
      await load();
    } catch (e) {
      setMessage(`エラー: ${String(e)}`);
    } finally {
      setSaving(false);
    }
  };

  return (
    <div className="card-elevated space-y-4">
      <div className="flex flex-wrap items-center gap-2">
        <h2 className="text-lg font-semibold text-on-surface">音声 (VC) 設定</h2>
        <span className="rounded-full bg-surface-variant px-2 py-0.5 text-xs text-on-surface-variant">
          設定元: {source === 'db' ? 'ダッシュボード' : '設定ファイル'}
        </span>
        <span
          className={`rounded-full px-2 py-0.5 text-xs ${
            runtimeActive ? 'bg-green-500/10 text-green-600' : 'bg-surface-variant text-on-surface-variant'
          }`}
        >
          ランタイム: {runtimeActive ? '稼働中（変更は即時反映）' : '停止中（反映には再起動）'}
        </span>
      </div>

      <label className="flex items-center gap-2 text-sm text-on-surface">
        <input
          type="checkbox"
          checked={config.enabled}
          onChange={(e) => patch((c) => ({ ...c, enabled: e.target.checked }))}
        />
        VC 対話を有効にする（有効/無効の切り替えは再起動後に反映）
      </label>

      <div className="grid gap-4 md:grid-cols-2">
        <div className="space-y-3">
          <h3 className="text-sm font-semibold text-on-surface">文字起こし (STT)</h3>
          <div>
            <label className="mb-1 block text-xs text-on-surface-variant">プロバイダー</label>
            <select
              value={config.stt.provider}
              onChange={(e) => patch((c) => ({ ...c, stt: { ...c.stt, provider: e.target.value } }))}
              className={inputCls}
            >
              <option value="openai">OpenAI 互換 (Whisper / ローカル互換サーバ)</option>
            </select>
          </div>
          <div>
            <label className="mb-1 block text-xs text-on-surface-variant">
              Base URL（ローカル Whisper を使う場合のみ）
            </label>
            <input
              value={config.stt.base_url ?? ''}
              onChange={(e) =>
                patch((c) => ({ ...c, stt: { ...c.stt, base_url: e.target.value || undefined } }))
              }
              placeholder="http://localhost:8000/v1"
              className={inputCls}
            />
          </div>
          <div className="grid grid-cols-2 gap-3">
            <div>
              <label className="mb-1 block text-xs text-on-surface-variant">モデル</label>
              <input
                value={config.stt.model ?? ''}
                onChange={(e) => patch((c) => ({ ...c, stt: { ...c.stt, model: e.target.value } }))}
                placeholder="whisper-1"
                className={inputCls}
              />
            </div>
            <div>
              <label className="mb-1 block text-xs text-on-surface-variant">言語</label>
              <input
                value={config.stt.language ?? ''}
                onChange={(e) =>
                  patch((c) => ({ ...c, stt: { ...c.stt, language: e.target.value || null } }))
                }
                placeholder="ja"
                className={inputCls}
              />
            </div>
          </div>
          <div>
            <label className="mb-1 block text-xs text-on-surface-variant">
              API キー環境変数名（キー自体は環境変数で渡す）
            </label>
            <input
              value={config.stt.api_key_env ?? ''}
              onChange={(e) =>
                patch((c) => ({ ...c, stt: { ...c.stt, api_key_env: e.target.value } }))
              }
              placeholder="OPENAI_API_KEY"
              className={inputCls}
            />
          </div>
        </div>

        <div className="space-y-3">
          <h3 className="text-sm font-semibold text-on-surface">読み上げ (TTS)</h3>
          <div>
            <label className="mb-1 block text-xs text-on-surface-variant">プロバイダー</label>
            <select
              value={config.tts.provider}
              onChange={(e) => patch((c) => ({ ...c, tts: { ...c.tts, provider: e.target.value } }))}
              className={inputCls}
            >
              <option value="voicevox">VOICEVOX（ローカル・無料）</option>
              <option value="openai">OpenAI TTS</option>
            </select>
          </div>
          <div>
            <label className="mb-1 block text-xs text-on-surface-variant">Base URL</label>
            <input
              value={config.tts.base_url ?? ''}
              onChange={(e) =>
                patch((c) => ({ ...c, tts: { ...c.tts, base_url: e.target.value || undefined } }))
              }
              placeholder={
                config.tts.provider === 'voicevox' ? 'http://localhost:50021' : 'https://api.openai.com/v1'
              }
              className={inputCls}
            />
          </div>
          <div>
            <label className="mb-1 block text-xs text-on-surface-variant">
              既定の話者（VOICEVOX: スタイルID / OpenAI: alloy 等）
            </label>
            <input
              value={config.tts.default_voice ?? ''}
              onChange={(e) =>
                patch((c) => ({ ...c, tts: { ...c.tts, default_voice: e.target.value } }))
              }
              placeholder="3"
              className={inputCls}
            />
          </div>
          <div>
            <label className="mb-1 block text-xs text-on-surface-variant">
              エージェント別の声（1行に agent_id=話者）
            </label>
            <textarea
              value={agentVoicesText}
              onChange={(e) => setAgentVoicesText(e.target.value)}
              placeholder={'crab=3\nrabomi=1'}
              rows={3}
              className={`${inputCls} font-mono`}
            />
          </div>
        </div>
      </div>

      {message && (
        <p className={`text-sm ${message.startsWith('エラー') ? 'text-red-500' : 'text-green-600'}`}>
          {message}
        </p>
      )}
      <div className="flex gap-2">
        <button onClick={save} disabled={saving} className={btnPrimary}>
          {saving ? '保存中...' : '保存'}
        </button>
        {source === 'db' && (
          <button onClick={reset} disabled={saving} className={btnGhost}>
            TOML 設定に戻す
          </button>
        )}
      </div>
    </div>
  );
}

// ============ ページ本体 ============

export default function SystemSettings() {
  const [currentLevel, setCurrentLevel] = useState<string>('info');
  const [saving, setSaving] = useState(false);
  const [message, setMessage] = useState<string | null>(null);

  const [providers, setProviders] = useState<LlmProviderInfo[]>([]);
  const [providersMessage, setProvidersMessage] = useState<string | null>(null);

  const loadProviders = useCallback(async () => {
    try {
      const res = await getLlmProviders();
      setProviders(res.providers);
    } catch {
      // プロバイダー API が無い旧サーバでは一覧を出さない
    }
  }, []);

  useEffect(() => {
    getLogLevel()
      .then((res) => setCurrentLevel(res.log_level))
      .catch(() => {});
    loadProviders();
  }, [loadProviders]);

  const handleLogLevelChange = async (newLevel: string) => {
    setSaving(true);
    setMessage(null);
    try {
      const res = await patchLogLevel(newLevel);
      setCurrentLevel(res.log_level);
      setMessage(`ログレベルを "${res.log_level}" に変更しました`);
    } catch (e) {
      setMessage(`エラー: ${String(e)}`);
    } finally {
      setSaving(false);
    }
  };

  const onProviderChanged = async (msg: string) => {
    setProvidersMessage(msg);
    await loadProviders();
  };

  return (
    <div className="max-w-3xl mx-auto space-y-6">
      <div>
        <h1 className="text-xl font-bold text-on-surface">システム設定</h1>
        <p className="text-sm text-on-surface-variant mt-1">サーバーの動作設定を管理します</p>
      </div>

      <div className="card-elevated space-y-2">
        <h2 className="text-lg font-semibold text-on-surface">LLM プロバイダー</h2>
        <p className="text-xs text-on-surface-variant">
          API キー・Base URL・既定モデルを上書きできます。保存すると再起動なしで反映されます。
          キーは末尾4文字のみ表示され、平文が画面に出ることはありません。
        </p>
        {providersMessage && (
          <p
            className={`text-sm ${
              providersMessage.startsWith('エラー') ? 'text-red-500' : 'text-green-600'
            }`}
          >
            {providersMessage}
          </p>
        )}
        <div>
          {providers.map((p) => (
            <ProviderRow key={p.name} provider={p} onChanged={onProviderChanged} />
          ))}
          {providers.length === 0 && (
            <p className="py-4 text-sm text-on-surface-variant">読み込み中...</p>
          )}
        </div>
      </div>

      <CodexDiagnosticsCard />
      <CursorDiagnosticsCard />

      <VoiceSettings />

      <div className="card-elevated space-y-4">
        <h2 className="text-lg font-semibold text-on-surface">ログ設定</h2>
        <div className="flex items-center gap-4">
          <label className="text-sm font-medium text-on-surface-variant w-32">ログレベル</label>
          <select
            value={currentLevel}
            onChange={(e) => handleLogLevelChange(e.target.value)}
            disabled={saving}
            className="rounded-lg border border-outline bg-surface px-3 py-2 text-sm text-on-surface focus:outline-none focus:ring-2 focus:ring-primary flex-1 max-w-xs"
          >
            {LOG_LEVELS.map((level) => (
              <option key={level} value={level}>
                {level.toUpperCase()}
              </option>
            ))}
          </select>
        </div>
        {message && (
          <p className={`text-sm ${message.startsWith('エラー') ? 'text-red-500' : 'text-green-600'}`}>
            {message}
          </p>
        )}
        <p className="text-xs text-on-surface-variant">
          変更は即座に反映されます。サーバー再起動後はデフォルト (INFO) に戻ります。
        </p>
      </div>
    </div>
  );
}
