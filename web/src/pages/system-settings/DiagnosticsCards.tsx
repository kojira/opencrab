import { useCallback, useEffect, useState } from 'react';
import { getAcpDiagnostics, getCodexDiagnostics, getCursorDiagnostics } from '../../api/providers';
import type { AcpDiagnostics, CodexDiagnostics, CursorDiagnostics } from '../../api/providers';
import { btnGhost } from './shared';

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

function AcpDiagnosticsCard() {
  const [diag, setDiag] = useState<AcpDiagnostics | null>(null);
  const [loading, setLoading] = useState(false);

  const load = useCallback(async () => {
    setLoading(true);
    try {
      setDiag(await getAcpDiagnostics());
    } catch (e) {
      setDiag({
        configured_path: '',
        args: [],
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
        <h2 className="text-lg font-semibold text-on-surface">ACP 診断</h2>
        <button onClick={load} disabled={loading} className={btnGhost}>
          {loading ? '確認中...' : '再確認'}
        </button>
      </div>
      <p className="text-xs text-on-surface-variant">
        opencrab の<strong>サーバープロセスが実際に使う</strong> ACP エージェントの起動バイナリと
        引数です。ACP は <code className="font-mono">binary_path</code>（例{' '}
        <code className="font-mono">npx</code>）+ <code className="font-mono">args</code>（例{' '}
        <code className="font-mono">-y @zed-industries/claude-code-acp</code>）で起動し、
        <strong>引数がエージェント本体を担う</strong>ため <code className="font-mono">--version</code>
        だけでは起動可否が分かりません。実際に ACP を話せるかは各プロバイダ行の
        <strong>「接続テスト」</strong>で確認してください。
      </p>
      {diag && (
        <div className="space-y-1 text-sm">
          <div className="flex gap-2">
            <span className="w-32 shrink-0 text-on-surface-variant">解決パス</span>
            <span className="font-mono text-on-surface break-all">
              {diag.resolved_path ?? '（PATH 上に見つからない）'}
            </span>
          </div>
          <div className="flex gap-2">
            <span className="w-32 shrink-0 text-on-surface-variant">設定バイナリ</span>
            <span className="font-mono text-on-surface break-all">
              {diag.configured_path || '（未設定）'}
            </span>
          </div>
          <div className="flex gap-2">
            <span className="w-32 shrink-0 text-on-surface-variant">引数</span>
            <span className="font-mono text-on-surface break-all">
              {diag.args.length ? diag.args.join(' ') : '（なし）'}
            </span>
          </div>
          <div className="flex gap-2">
            <span className="w-32 shrink-0 text-on-surface-variant">バージョン</span>
            <span className="font-mono text-on-surface break-all">
              {diag.version ?? '（取得できません）'}
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

export { AcpDiagnosticsCard, CodexDiagnosticsCard, CursorDiagnosticsCard };
