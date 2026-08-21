import { useState } from "react";
import { useOutletContext } from "react-router-dom";
import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";
import { allowedCommandsApi } from "../api/allowed_commands";
import type { AgentDetail } from "../api/types";

export default function AgentAllowedCommands() {
  const { t } = useTranslation();
  const { agentId } = useOutletContext<{ agent: AgentDetail; agentId: string }>();
  const queryClient = useQueryClient();
  const [newCommand, setNewCommand] = useState("");
  const [error, setError] = useState<string | null>(null);

  const { data: commands = [], isLoading } = useQuery({
    queryKey: ["allowed-commands", agentId],
    queryFn: () => allowedCommandsApi.list(agentId),
  });

  const addMutation = useMutation({
    mutationFn: (command: string) => allowedCommandsApi.add(agentId, command),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["allowed-commands", agentId] });
      setNewCommand("");
      setError(null);
    },
    onError: (e: Error) => setError(e.message),
  });

  const removeMutation = useMutation({
    mutationFn: (command: string) => allowedCommandsApi.remove(agentId, command),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["allowed-commands", agentId] });
    },
  });

  const handleAdd = () => {
    const trimmed = newCommand.trim();
    if (!trimmed) return;
    if (!/^[a-zA-Z0-9_-]+$/.test(trimmed)) {
      setError(t("allowedCommands.invalidName"));
      return;
    }
    addMutation.mutate(trimmed);
  };

  return (
    <div>
      <h2 className="text-headline-sm text-on-surface font-medium mb-2">
        {t("allowedCommands.title")}
      </h2>
      <p className="text-body-md text-on-surface-variant mb-6">
        {t("allowedCommands.description")}
      </p>

      {/* Add form */}
      <div className="flex gap-2 mb-6">
        <input
          type="text"
          value={newCommand}
          onChange={(e) => setNewCommand(e.target.value)}
          onKeyDown={(e) => e.key === "Enter" && handleAdd()}
          placeholder={t("allowedCommands.placeholder")}
          className="input-outlined flex-1"
        />
        <button
          onClick={handleAdd}
          disabled={addMutation.isPending}
          className="btn-filled"
        >
          {t("allowedCommands.add")}
        </button>
      </div>

      {error && (
        <div className="card-outlined border-error bg-error-container/30 p-3 mb-4">
          <p className="text-body-md text-error">{error}</p>
        </div>
      )}

      {/* Command list */}
      {isLoading ? (
        <p className="text-body-md text-on-surface-variant">{t("common.loading")}</p>
      ) : commands.length === 0 ? (
        <p className="text-body-md text-on-surface-variant">{t("allowedCommands.noCommands")}</p>
      ) : (
        <div className="space-y-2">
          {commands.map((item) => (
            <div
              key={item.command}
              className="card-outlined flex items-center justify-between px-4 py-3"
            >
              <code className="text-body-md font-mono text-on-surface">{item.command}</code>
              <button
                onClick={() => removeMutation.mutate(item.command)}
                disabled={removeMutation.isPending}
                className="btn-text text-error"
              >
                <span className="material-symbols-outlined text-lg">delete</span>
                {t("common.delete")}
              </button>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
