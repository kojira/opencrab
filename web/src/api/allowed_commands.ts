import { api } from "./client";

export interface AllowedCommand {
  command: string;
}

export const allowedCommandsApi = {
  list: (agentId: string) =>
    api.get<AllowedCommand[]>(`/agents/${agentId}/allowed-commands`),

  add: (agentId: string, command: string) =>
    api.post<{ command: string; added: boolean }>(
      `/agents/${agentId}/allowed-commands`,
      { command }
    ),

  remove: (agentId: string, command: string) =>
    api.del<{ removed: boolean }>(
      `/agents/${agentId}/allowed-commands/${command}`
    ),
};
