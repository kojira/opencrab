import { useCallback, useEffect, useState } from 'react';
import { getVoiceConfig, resetVoiceConfig, updateVoiceConfig } from '../../api/providers';
import type { VoiceConfig } from '../../api/providers';
import { btnGhost, btnPrimary, inputCls } from './shared';

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
              placeholder={'crab=3\nagent-b=1'}
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

export default VoiceSettings;
