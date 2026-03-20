import { useState, useEffect, useCallback } from "react";
import { useOutletContext } from "react-router-dom";
import type { AgentDetail } from "../api/types";
import { getTrustedUsers, addTrustedUser, removeTrustedUser, updateTrustedUser } from "../api/trusted_users";
import type { TrustedUserDto } from "../api/trusted_users";

interface AgentContext {
  agent: AgentDetail;
  agentId: string;
}

const PERMISSIONS = ["user", "co-agent", "owner"];

export default function AgentTrustedUsers() {
  const { agentId } = useOutletContext<AgentContext>();
  const [users, setUsers] = useState<TrustedUserDto[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  const [showAddModal, setShowAddModal] = useState(false);
  const [newUserId, setNewUserId] = useState("");
  const [newPermission, setNewPermission] = useState("user");
  const [adding, setAdding] = useState(false);
  const [addError, setAddError] = useState<string | null>(null);

  const [confirmRemoveId, setConfirmRemoveId] = useState<string | null>(null);
  const [editingId, setEditingId] = useState<string | null>(null);
  const [editPermission, setEditPermission] = useState("user");

  const loadUsers = useCallback(() => {
    setLoading(true);
    getTrustedUsers(agentId)
      .then((list) => {
        setUsers(list);
        setLoading(false);
      })
      .catch((e: Error) => {
        setError(e.message);
        setLoading(false);
      });
  }, [agentId]);

  useEffect(() => {
    loadUsers();
  }, [loadUsers]);

  const handleAdd = async () => {
    const uid = newUserId.trim();
    if (!uid) {
      setAddError("Discord User ID is required.");
      return;
    }
    setAdding(true);
    setAddError(null);
    try {
      await addTrustedUser(agentId, { discord_user_id: uid, permission: newPermission });
      setShowAddModal(false);
      setNewUserId("");
      setNewPermission("user");
      loadUsers();
    } catch (e: unknown) {
      setAddError(String(e));
    } finally {
      setAdding(false);
    }
  };

  const handleRemove = async (id: string) => {
    await removeTrustedUser(agentId, id);
    setConfirmRemoveId(null);
    loadUsers();
  };

  const handleUpdatePermission = async (id: string) => {
    await updateTrustedUser(agentId, id, { permission: editPermission });
    setEditingId(null);
    loadUsers();
  };

  return (
    <div>
      <div className="flex items-center justify-between mb-6">
        <div>
          <h2 className="text-title-lg text-on-surface font-medium">
            Trusted Users
          </h2>
          <p className="text-body-md text-on-surface-variant mt-1">
            Discord users who can send DMs to this agent. If empty, only the owner can interact via DM.
          </p>
        </div>
        <button
          className="btn-filled"
          onClick={() => {
            setShowAddModal(true);
            setAddError(null);
            setNewUserId("");
            setNewPermission("user");
          }}
        >
          <span className="material-symbols-outlined text-xl">add</span>
          Add User
        </button>
      </div>

      {loading && (
        <div className="empty-state">
          <p className="text-body-lg text-on-surface-variant">Loading...</p>
        </div>
      )}

      {error && (
        <div className="card-outlined border-error bg-error-container/30 p-4">
          <div className="flex items-center gap-2">
            <span className="material-symbols-outlined text-error">error</span>
            <p className="text-body-lg text-error-on-container">Error: {error}</p>
          </div>
        </div>
      )}

      {!loading && !error && users.length === 0 && (
        <div className="empty-state">
          <span className="material-symbols-outlined empty-state-icon">
            shield_person
          </span>
          <p className="empty-state-text">No trusted users.</p>
          <p className="text-body-sm text-on-surface-variant mt-2">
            Add Discord user IDs to allow them to interact with this agent via DM.
          </p>
        </div>
      )}

      {!loading && !error && users.length > 0 && (
        <div className="card-outlined overflow-hidden">
          <table className="w-full">
            <thead>
              <tr className="border-b border-outline-variant">
                <th className="text-left text-label-lg text-on-surface-variant px-4 py-3">
                  Discord User ID
                </th>
                <th className="text-left text-label-lg text-on-surface-variant px-4 py-3">
                  Permission
                </th>
                <th className="text-left text-label-lg text-on-surface-variant px-4 py-3">
                  Added By
                </th>
                <th className="text-left text-label-lg text-on-surface-variant px-4 py-3">
                  Added At
                </th>
                <th className="px-4 py-3"></th>
              </tr>
            </thead>
            <tbody>
              {users.map((u) => (
                <tr
                  key={u.id}
                  className="border-b border-outline-variant last:border-0 hover:bg-surface-variant/30"
                >
                  <td className="px-4 py-3 text-body-lg text-on-surface font-mono">
                    {u.discord_user_id}
                  </td>
                  <td className="px-4 py-3 text-body-md text-on-surface-variant">
                    {editingId === u.id ? (
                      <div className="flex items-center gap-2">
                        <select
                          className="input-outlined text-sm py-1"
                          value={editPermission}
                          onChange={(e) => setEditPermission(e.target.value)}
                        >
                          {PERMISSIONS.map((p) => (
                            <option key={p} value={p}>{p}</option>
                          ))}
                        </select>
                        <button className="btn-tonal text-sm" onClick={() => handleUpdatePermission(u.id)}>Save</button>
                        <button className="btn-text text-sm" onClick={() => setEditingId(null)}>Cancel</button>
                      </div>
                    ) : (
                      <span
                        className="cursor-pointer hover:underline"
                        onClick={() => { setEditingId(u.id); setEditPermission(u.permission); }}
                      >
                        {u.permission}
                      </span>
                    )}
                  </td>
                  <td className="px-4 py-3 text-body-sm text-on-surface-variant">
                    {u.created_by}
                  </td>
                  <td className="px-4 py-3 text-body-sm text-on-surface-variant">
                    {u.created_at}
                  </td>
                  <td className="px-4 py-3">
                    <button
                      className="btn-text text-error text-sm"
                      onClick={() => setConfirmRemoveId(u.id)}
                    >
                      <span className="material-symbols-outlined text-base">delete</span>
                      Remove
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}

      {showAddModal && (
        <div className="scrim">
          <div className="dialog">
            <h3 className="text-title-lg text-on-surface mb-4">Add Trusted User</h3>
            <div className="space-y-4 mb-6">
              <div>
                <label className="block text-label-lg text-on-surface mb-2">
                  Discord User ID *
                </label>
                <input
                  type="text"
                  className="input-outlined"
                  placeholder="e.g. 123456789012345678"
                  value={newUserId}
                  onChange={(e) => setNewUserId(e.target.value)}
                />
              </div>
              <div>
                <label className="block text-label-lg text-on-surface mb-2">
                  Permission
                </label>
                <select
                  className="input-outlined"
                  value={newPermission}
                  onChange={(e) => setNewPermission(e.target.value)}
                >
                  {PERMISSIONS.map((p) => (
                    <option key={p} value={p}>{p}</option>
                  ))}
                </select>
              </div>
              {addError && (
                <p className="text-body-md text-error">{addError}</p>
              )}
            </div>
            <div className="flex gap-3 justify-end">
              <button className="btn-outlined" onClick={() => setShowAddModal(false)}>
                Cancel
              </button>
              <button className="btn-filled" disabled={adding} onClick={handleAdd}>
                {adding ? "Adding..." : "Add"}
              </button>
            </div>
          </div>
        </div>
      )}

      {confirmRemoveId && (
        <div className="scrim">
          <div className="dialog">
            <div className="flex items-center gap-3 mb-4">
              <span className="material-symbols-outlined text-2xl text-error">warning</span>
              <h3 className="text-title-lg text-on-surface">Remove User?</h3>
            </div>
            <p className="text-body-lg text-on-surface-variant mb-6">
              Remove this user from trusted users?
            </p>
            <div className="flex gap-3 justify-end">
              <button className="btn-outlined" onClick={() => setConfirmRemoveId(null)}>
                Cancel
              </button>
              <button className="btn-danger" onClick={() => handleRemove(confirmRemoveId)}>
                Remove
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
