import { useState, useEffect } from 'react';
import { getLogLevel, patchLogLevel } from '../api/system';

const LOG_LEVELS = ['debug', 'info', 'warn', 'error'];

export default function SystemSettings() {
  const [currentLevel, setCurrentLevel] = useState<string>('info');
  const [saving, setSaving] = useState(false);
  const [message, setMessage] = useState<string | null>(null);

  useEffect(() => {
    getLogLevel().then((res) => setCurrentLevel(res.log_level)).catch(() => {});
  }, []);

  const handleChange = async (newLevel: string) => {
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

  return (
    <div className="max-w-2xl mx-auto space-y-6">
      <div>
        <h1 className="text-xl font-bold text-on-surface">システム設定</h1>
        <p className="text-sm text-on-surface-variant mt-1">サーバーの動作設定を管理します</p>
      </div>
      <div className="card-elevated space-y-4">
        <h2 className="text-lg font-semibold text-on-surface">ログ設定</h2>
        <div className="flex items-center gap-4">
          <label className="text-sm font-medium text-on-surface-variant w-32">ログレベル</label>
          <select
            value={currentLevel}
            onChange={(e) => handleChange(e.target.value)}
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
