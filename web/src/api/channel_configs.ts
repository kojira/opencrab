import { api } from "./client";
import type { ChannelConfigDto, ChannelConfigListResponse } from "./types";

export async function listChannelConfigs(agentId: string, guildId: string): Promise<ChannelConfigListResponse> {
  return api.get<ChannelConfigListResponse>(`/agents/${agentId}/channel-configs?guild_id=${guildId}`);
}

export async function upsertChannelConfig(agentId: string, config: ChannelConfigDto): Promise<{ channel_id: string; message: string }> {
  return api.put<{ channel_id: string; message: string }>(`/agents/${agentId}/channel-configs`, config);
}

export async function deleteChannelConfig(agentId: string, channelId: string): Promise<{ channel_id: string; message: string }> {
  return api.del<{ channel_id: string; message: string }>(`/agents/${agentId}/channel-configs/${channelId}`);
}
