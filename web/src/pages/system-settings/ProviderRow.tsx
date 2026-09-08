import { useState } from 'react';
import { resetLlmProvider, updateLlmProvider } from '../../api/providers';
import type { LlmProviderInfo } from '../../api/providers';
import ProviderEditor from './ProviderEditor';
import { btnGhost } from './shared';

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

export default ProviderRow;
