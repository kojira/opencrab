import { api } from './client';

export interface NostrConfigDto {
  configured: boolean;
  enabled: boolean;
  running: boolean;
  has_secret_key: boolean;
  secret_key_masked: string;
  relays: string[];
  filter: {
    authors: string[];
    keywords: string[];
    kinds: number[];
  };
}

export interface UpdateNostrBody {
  /** nsec。空なら既存を保持（更新でクリアしない）。 */
  secret_key?: string;
  relays: string[];
  authors: string[];
  keywords: string[];
  kinds: number[];
  enabled: boolean;
}

export function getNostrConfig(agentId: string): Promise<NostrConfigDto> {
  return api.get<NostrConfigDto>(`/agents/${agentId}/nostr`);
}

export function updateNostrConfig(
  agentId: string,
  body: UpdateNostrBody,
): Promise<{ updated: boolean; enabled: boolean }> {
  return api.put(`/agents/${agentId}/nostr`, body);
}

export interface GenerateNostrResult {
  generated: boolean;
  npub: string;
  pubkey: string;
}

/** nostaro の vanity で新規鍵を生成して保存する（operator 操作）。 */
export function generateNostrKey(
  agentId: string,
  body: { prefix?: string; overwrite?: boolean },
): Promise<GenerateNostrResult> {
  return api.post(`/agents/${agentId}/nostr/generate`, body);
}

export function startNostrGateway(agentId: string): Promise<{ started: boolean }> {
  return api.post(`/agents/${agentId}/nostr/start`);
}

export function stopNostrGateway(agentId: string): Promise<{ stopped: boolean }> {
  return api.post(`/agents/${agentId}/nostr/stop`);
}

export function deleteNostrConfig(agentId: string): Promise<{ deleted: boolean }> {
  return api.del(`/agents/${agentId}/nostr`);
}

// ---- Nostr 受信 → Discord 転記先（issue #252 段階 B）----

export interface NostrRelayConfigDto {
  /** 行が存在するか（未設定なら false）。 */
  configured: boolean;
  enabled: boolean;
  /** 転記先 webhook が設定済みか。 */
  has_webhook: boolean;
  /** 伏字化した webhook URL（生 URL は API から返らない）。 */
  webhook_url_masked: string;
}

export interface UpdateNostrRelayBody {
  enabled: boolean;
  /** webhook URL。空 / 省略で転記先を消去する。 */
  webhook_url?: string;
}

export function getNostrRelayConfig(agentId: string): Promise<NostrRelayConfigDto> {
  return api.get<NostrRelayConfigDto>(`/agents/${agentId}/nostr-relay`);
}

export function updateNostrRelayConfig(
  agentId: string,
  body: UpdateNostrRelayBody,
): Promise<{
  updated: boolean;
  enabled: boolean;
  has_webhook: boolean;
  webhook_url_masked: string;
}> {
  return api.put(`/agents/${agentId}/nostr-relay`, body);
}
