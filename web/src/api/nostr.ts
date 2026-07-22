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

export function startNostrGateway(agentId: string): Promise<{ started: boolean }> {
  return api.post(`/agents/${agentId}/nostr/start`);
}

export function stopNostrGateway(agentId: string): Promise<{ stopped: boolean }> {
  return api.post(`/agents/${agentId}/nostr/stop`);
}

export function deleteNostrConfig(agentId: string): Promise<{ deleted: boolean }> {
  return api.del(`/agents/${agentId}/nostr`);
}
