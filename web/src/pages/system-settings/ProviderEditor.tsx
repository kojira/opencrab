import { useState } from 'react';
import { testLlmProvider, updateLlmProvider } from '../../api/providers';
import type { LlmProviderInfo, UpdateProviderBody } from '../../api/providers';
import { btnGhost, btnPrimary, inputCls, SUBPROCESS_PROVIDERS } from './shared';

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
  const isSubprocess = SUBPROCESS_PROVIDERS.includes(provider.name);
  const [binaryPath, setBinaryPath] = useState(provider.binary_path);
  const [args, setArgs] = useState(provider.args.join(' '));
  const [workingDir, setWorkingDir] = useState(provider.working_dir);
  const [timeoutSecs, setTimeoutSecs] = useState(
    provider.timeout_secs ? String(provider.timeout_secs) : '',
  );
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [testMsg, setTestMsg] = useState<string | null>(null);

  const save = async () => {
    setSaving(true);
    setError(null);
    setTestMsg(null);
    const body: UpdateProviderBody = {};
    // 空欄のままなら API キーは変更しない（マスク値の再送を防ぐ）
    if (apiKey !== '') body.api_key = apiKey;
    if (baseUrl !== provider.base_url) body.base_url = baseUrl === '' ? null : baseUrl;
    if (defaultModel !== provider.default_model)
      body.default_model = defaultModel === '' ? null : defaultModel;
    if (reasoningEffort !== provider.reasoning_effort)
      body.reasoning_effort = reasoningEffort === '' ? null : reasoningEffort;
    if (isSubprocess) {
      if (binaryPath !== provider.binary_path)
        body.binary_path = binaryPath === '' ? null : binaryPath;
      const argsList = args
        .trim()
        .split(/\s+/)
        .filter((s) => s.length > 0);
      if (args.trim() !== provider.args.join(' ')) body.args = argsList.length ? argsList : null;
      if (workingDir !== provider.working_dir)
        body.working_dir = workingDir === '' ? null : workingDir;
      // 空欄 = オーバーライド解除(null)。非数値は誤ってクリアしないよう変更なし扱い。
      const parsed = parseInt(timeoutSecs, 10);
      const tNum = timeoutSecs.trim() === '' ? null : Number.isNaN(parsed) ? undefined : parsed;
      if (tNum !== undefined && (provider.timeout_secs || 0) !== (tNum || 0))
        body.timeout_secs = tNum;
    }
    try {
      const res = await updateLlmProvider(provider.name, body);
      const suffix = isSubprocess
        ? res.test_ok
          ? '（接続OK）'
          : '（保存しましたが接続確認に失敗。binary_path/args を確認してください）'
        : '';
      onSaved(`${provider.name} を保存し、ルーターを再構築しました（再起動不要）${suffix}`);
    } catch (e) {
      setError(String(e));
    } finally {
      setSaving(false);
    }
  };

  const runTest = async () => {
    setTestMsg('テスト中...');
    try {
      const r = await testLlmProvider(provider.name);
      setTestMsg(r.ok ? '✅ 接続OK（起動確認できました）' : '❌ 接続失敗（binary_path/args を確認）');
    } catch (e) {
      setTestMsg(`❌ ${String(e)}`);
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

      {isSubprocess && (
        <div className="space-y-3 rounded-lg border border-outline/60 p-3">
          <p className="text-xs font-medium text-on-surface-variant">
            起動設定（subprocess プロバイダ）
          </p>
          <div className="grid gap-3 sm:grid-cols-2">
            <div>
              <label className="mb-1 block text-xs font-medium text-on-surface-variant">
                起動コマンド（binary_path）
              </label>
              <input
                value={binaryPath}
                onChange={(e) => setBinaryPath(e.target.value)}
                placeholder="例: gemini / npx / cursor-agent"
                className={inputCls}
              />
            </div>
            <div>
              <label className="mb-1 block text-xs font-medium text-on-surface-variant">
                起動引数（空白区切り）
              </label>
              <input
                value={args}
                onChange={(e) => setArgs(e.target.value)}
                placeholder="例: --experimental-acp / -y @zed-industries/claude-code-acp"
                className={inputCls}
              />
            </div>
            <div>
              <label className="mb-1 block text-xs font-medium text-on-surface-variant">
                作業ディレクトリ（空欄 = 既定）
              </label>
              <input
                value={workingDir}
                onChange={(e) => setWorkingDir(e.target.value)}
                placeholder="例: /path/to/workspace"
                className={inputCls}
              />
            </div>
            <div>
              <label className="mb-1 block text-xs font-medium text-on-surface-variant">
                タイムアウト秒（空欄 = 既定）
              </label>
              <input
                value={timeoutSecs}
                onChange={(e) => setTimeoutSecs(e.target.value)}
                placeholder="300"
                inputMode="numeric"
                className={inputCls}
              />
            </div>
          </div>
          <div className="flex items-center gap-2">
            <button onClick={runTest} disabled={saving} className={btnGhost}>
              接続テスト
            </button>
            {testMsg && <span className="text-sm text-on-surface-variant">{testMsg}</span>}
          </div>
        </div>
      )}

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

export default ProviderEditor;
