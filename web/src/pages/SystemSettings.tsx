import { useCallback, useEffect, useState } from 'react';
import { getLogLevel, patchLogLevel } from '../api/system';
import { getLlmProviders } from '../api/providers';
import type { LlmProviderInfo } from '../api/providers';
import {
  AcpDiagnosticsCard,
  CodexDiagnosticsCard,
  CursorDiagnosticsCard,
} from './system-settings/DiagnosticsCards';
import { ModelPricingSection } from './system-settings/ModelPricingSection';
import ProviderRow from './system-settings/ProviderRow';
import VoiceSettings from './system-settings/VoiceSettings';
import { LOG_LEVELS } from './system-settings/shared';

export { ModelPricingSection } from './system-settings/ModelPricingSection';

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

      <ModelPricingSection />

      <CodexDiagnosticsCard />
      <CursorDiagnosticsCard />
      <AcpDiagnosticsCard />

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
