import { api } from './client';

// ============ LLM プロバイダー設定 ============

export interface LlmProviderInfo {
  name: string;
  /** 現在のルーターに登録済み（= 実際に使える状態） */
  active: boolean;
  in_toml: boolean;
  has_override: boolean;
  enabled_override: boolean | null;
  api_key_source: 'db' | 'toml' | 'none';
  api_key_masked: string;
  base_url: string;
  default_model: string;
  /** 推論（thinking）強度。空はモデル既定。 */
  reasoning_effort: string;
  /** 起動バイナリ（subprocess プロバイダ codex/cursor/acp）。 */
  binary_path: string;
  /** 起動引数（acp 等）。 */
  args: string[];
  /** 作業ディレクトリ。 */
  working_dir: string;
  /** タイムアウト秒（0 は既定）。 */
  timeout_secs: number;
}

export interface LlmProvidersResponse {
  providers: LlmProviderInfo[];
  default_model: string;
}

/**
 * PUT ボディの三値セマンティクス:
 * フィールド省略 = 変更しない / null = オーバーライド解除（TOMLに戻す）/ 値 = 上書き
 */
export interface UpdateProviderBody {
  enabled?: boolean | null;
  api_key?: string | null;
  base_url?: string | null;
  default_model?: string | null;
  reasoning_effort?: string | null;
  binary_path?: string | null;
  args?: string[] | null;
  working_dir?: string | null;
  timeout_secs?: number | null;
}

export function getLlmProviders(): Promise<LlmProvidersResponse> {
  return api.get<LlmProvidersResponse>('/llm/providers');
}

export function updateLlmProvider(
  name: string,
  body: UpdateProviderBody,
): Promise<{ provider: LlmProviderInfo; reloaded: boolean; test_ok: boolean | null }> {
  return api.put(`/llm/providers/${encodeURIComponent(name)}`, body);
}

export function resetLlmProvider(name: string): Promise<{ deleted: boolean; reloaded: boolean }> {
  return api.del(`/llm/providers/${encodeURIComponent(name)}/override`);
}

export function reloadLlmProviders(): Promise<{ reloaded: boolean; active_providers: string[] }> {
  return api.post('/llm/providers/reload');
}

/** 現在の設定でプロバイダの起動確認（health_check）を行う。 */
export function testLlmProvider(name: string): Promise<{ provider: string; ok: boolean }> {
  return api.post(`/llm/providers/${encodeURIComponent(name)}/test`);
}

export interface CodexDiagnostics {
  /** config で指定したパス（"codex" は PATH 検索） */
  configured_path: string;
  /** サーバー環境で実際に解決される絶対パス（which）。null = 見つからない */
  resolved_path: string | null;
  /** `<codex> --version` の出力。null = 実行失敗 */
  version: string | null;
  /** 実行失敗時のエラー文 */
  error: string | null;
}

export function getCodexDiagnostics(): Promise<CodexDiagnostics> {
  return api.get<CodexDiagnostics>('/llm/codex/diagnostics');
}

export interface CursorDiagnostics {
  /** config で指定したパス（"cursor-agent" は PATH 検索） */
  configured_path: string;
  /** サーバー環境で実際に解決される絶対パス（which）。null = 見つからない */
  resolved_path: string | null;
  /** `<cursor> --version` の出力。null = 実行失敗 */
  version: string | null;
  /** 実行失敗時のエラー文 */
  error: string | null;
}

export function getCursorDiagnostics(): Promise<CursorDiagnostics> {
  return api.get<CursorDiagnostics>('/llm/cursor/diagnostics');
}

export interface AcpDiagnostics {
  /** config で指定した起動バイナリ（PATH 検索されうる）。空 = 未設定 */
  configured_path: string;
  /** 起動引数（ACP 本体の指定を含む） */
  args: string[];
  /** サーバー環境で実際に解決される絶対パス（which）。null = 見つからない */
  resolved_path: string | null;
  /** `<binary> --version` の出力（npx 等では起動バイナリ自身）。null = 実行失敗 */
  version: string | null;
  /** 実行失敗/未設定時のエラー文 */
  error: string | null;
}

export function getAcpDiagnostics(): Promise<AcpDiagnostics> {
  return api.get<AcpDiagnostics>('/llm/acp/diagnostics');
}

// ============ Voice (VC) 設定 ============

export interface VoiceSttConfig {
  provider: string;
  base_url?: string;
  model?: string;
  api_key_env?: string;
  language?: string | null;
}

export interface VoiceTtsConfig {
  provider: string;
  base_url?: string;
  model?: string;
  api_key_env?: string;
  default_voice?: string;
  agent_voices?: Record<string, string>;
}

export interface VoiceConfig {
  enabled: boolean;
  stt: VoiceSttConfig;
  tts: VoiceTtsConfig;
}

export interface VoiceConfigResponse {
  config: VoiceConfig;
  source: 'db' | 'toml';
  /** true なら STT/TTS の変更は保存と同時に反映される */
  runtime_active: boolean;
}

export function getVoiceConfig(): Promise<VoiceConfigResponse> {
  return api.get<VoiceConfigResponse>('/voice/config');
}

export function updateVoiceConfig(
  config: VoiceConfig,
): Promise<{ saved: boolean; applied_live: boolean; restart_required: boolean }> {
  return api.put('/voice/config', config);
}

export function resetVoiceConfig(): Promise<{ deleted: boolean; restart_required: boolean }> {
  return api.del('/voice/config');
}
