import { useState, useEffect, useCallback } from "react";
import { useOutletContext } from "react-router-dom";
import type { AgentDetail } from "../api/types";
import { getCoAgents, addCoAgent, removeCoAgent } from "../api/co_agents";
import type { CoAgentDto } from "../api/co_agents";
import { useTranslation } from "react-i18next";

interface AgentContext {
  agent: AgentDetail;
  agentId: string;
}

export default function AgentCoAgents() {
  const { agentId } = useOutletContext<AgentContext>();
  const { t } = useTranslation();
  const [coAgents, setCoAgents] = useState<CoAgentDto[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  // Add modal state
  const [showAddModal, setShowAddModal] = useState(false);
  const [newCoAgentId, setNewCoAgentId] = useState("");
  const [adding, setAdding] = useState(false);
  const [addError, setAddError] = useState<string | null>(null);

  // Remove confirm state
  const [confirmRemoveId, setConfirmRemoveId] = useState<string | null>(null);

  const loadCoAgents = useCallback(() => {
    setLoading(true);
    getCoAgents(agentId)
      .then((list) => {
        setCoAgents(list);
        setLoading(false);
      })
      .catch((e: Error) => {
        setError(e.message);
        setLoading(false);
      });
  }, [agentId]);

  useEffect(() => {
    loadCoAgents();
  }, [loadCoAgents]);

  const handleAdd = async () => {
    const cid = newCoAgentId.trim();
    if (!cid) {
      setAddError(t("coAgents.idRequired"));
      return;
    }
    setAdding(true);
    setAddError(null);
    try {
      await addCoAgent(agentId, {
        co_agent_id: cid,
      });
      setShowAddModal(false);
      setNewCoAgentId("");
      loadCoAgents();
    } catch (e: unknown) {
      setAddError(String(e));
    } finally {
      setAdding(false);
    }
  };

  const handleRemove = async (coAgentId: string) => {
    await removeCoAgent(agentId, coAgentId);
    setConfirmRemoveId(null);
    loadCoAgents();
  };

  return (
    <div>
      <div className="flex items-center justify-between mb-6">
        <div>
          <h2 className="text-title-lg text-on-surface font-medium">
            {t("coAgents.title")}
          </h2>
          <p className="text-body-md text-on-surface-variant mt-1">
            {t("coAgents.description")}
          </p>
        </div>
        <button
          className="btn-filled"
          onClick={() => {
            setShowAddModal(true);
            setAddError(null);
            setNewCoAgentId("");
          }}
        >
          <span className="material-symbols-outlined text-xl">add</span>
          {t("coAgents.addButton")}
        </button>
      </div>

      {loading && (
        <div className="empty-state">
          <p className="text-body-lg text-on-surface-variant">{t("common.loading")}</p>
        </div>
      )}

      {error && (
        <div className="card-outlined border-error bg-error-container/30 p-4">
          <div className="flex items-center gap-2">
            <span className="material-symbols-outlined text-error">error</span>
            <p className="text-body-lg text-error-on-container">
              {t("common.error", { message: error })}
            </p>
          </div>
        </div>
      )}

      {!loading && !error && coAgents.length === 0 && (
        <div className="empty-state">
          <span className="material-symbols-outlined empty-state-icon">
            group
          </span>
          <p className="empty-state-text">{t("coAgents.noCoAgents")}</p>
          <p className="text-body-sm text-on-surface-variant mt-2">
            {t("coAgents.emptyDesc")}
          </p>
        </div>
      )}

      {!loading && !error && coAgents.length > 0 && (
        <div className="card-outlined overflow-hidden">
          <table className="w-full">
            <thead>
              <tr className="border-b border-outline-variant">
                <th className="text-left text-label-lg text-on-surface-variant px-4 py-3">
                  {t("coAgents.tableId")}
                </th>
                <th className="text-left text-label-lg text-on-surface-variant px-4 py-3">
                  {t("coAgents.tableAddedBy")}
                </th>
                <th className="text-left text-label-lg text-on-surface-variant px-4 py-3">
                  {t("coAgents.tableAddedAt")}
                </th>
                <th className="px-4 py-3"></th>
              </tr>
            </thead>
            <tbody>
              {coAgents.map((ca) => (
                <tr
                  key={ca.id}
                  className="border-b border-outline-variant last:border-0 hover:bg-surface-variant/30"
                >
                  <td className="px-4 py-3 text-body-lg text-on-surface font-mono">
                    {ca.co_agent_id}
                  </td>
                  <td className="px-4 py-3 text-body-sm text-on-surface-variant">
                    {ca.created_by}
                  </td>
                  <td className="px-4 py-3 text-body-sm text-on-surface-variant">
                    {ca.created_at}
                  </td>
                  <td className="px-4 py-3">
                    <button
                      className="btn-text text-error text-sm"
                      onClick={() => setConfirmRemoveId(ca.co_agent_id)}
                    >
                      <span className="material-symbols-outlined text-base">
                        delete
                      </span>
                      {t("common.remove")}
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}

      {/* Add Co-Agent Modal */}
      {showAddModal && (
        <div className="scrim">
          <div className="dialog">
            <h3 className="text-title-lg text-on-surface mb-4">
              {t("coAgents.addModalTitle")}
            </h3>
            <div className="space-y-4 mb-6">
              <div>
                <label className="block text-label-lg text-on-surface mb-2">
                  {t("coAgents.idLabel")}
                </label>
                <input
                  type="text"
                  className="input-outlined"
                  placeholder="e.g. helper-agent-1"
                  value={newCoAgentId}
                  onChange={(e) => setNewCoAgentId(e.target.value)}
                />
              </div>
              {addError && (
                <p className="text-body-md text-error">{addError}</p>
              )}
            </div>
            <div className="flex gap-3 justify-end">
              <button
                className="btn-outlined"
                onClick={() => setShowAddModal(false)}
              >
                {t("common.cancel")}
              </button>
              <button
                className="btn-filled"
                disabled={adding}
                onClick={handleAdd}
              >
                {adding ? t("common.adding") : t("common.add")}
              </button>
            </div>
          </div>
        </div>
      )}

      {/* Remove Confirmation Dialog */}
      {confirmRemoveId && (
        <div className="scrim">
          <div className="dialog">
            <div className="flex items-center gap-3 mb-4">
              <span className="material-symbols-outlined text-2xl text-error">
                warning
              </span>
              <h3 className="text-title-lg text-on-surface">
                {t("coAgents.confirmRemoveTitle")}
              </h3>
            </div>
            <p className="text-body-lg text-on-surface-variant mb-6">
              {t("coAgents.confirmRemoveMsg", { id: confirmRemoveId })}
            </p>
            <div className="flex gap-3 justify-end">
              <button
                className="btn-outlined"
                onClick={() => setConfirmRemoveId(null)}
              >
                {t("common.cancel")}
              </button>
              <button
                className="btn-danger"
                onClick={() => handleRemove(confirmRemoveId)}
              >
                {t("common.remove")}
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
