import { useState, useEffect, useCallback } from 'react';
import { useTranslation } from 'react-i18next';
import {
  listMcpServers,
  putMcpServer,
  setMcpEnabled,
  deleteMcpServer,
  testMcpServer,
  type McpServerDto,
} from '../../api/mcp';

export function McpSection({ agentId }: { agentId: string }) {
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
