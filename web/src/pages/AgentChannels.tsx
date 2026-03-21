import { useState, useEffect, useCallback } from 'react';
import { useAgentContext } from '../hooks/useAgentContext';
import { listChannelConfigs, upsertChannelConfig, deleteChannelConfig } from '../api/channel_configs';
import type { ChannelConfigDto } from '../api/types';

export default function AgentChannels() {
  const { agentId } = useAgentContext();
  const [guildId, setGuildId] = useState('');
  const [configs, setConfigs] = useState<ChannelConfigDto[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    if (!guildId) return;
    setLoading(true);
    setError(null);
    try {
      const res = await listChannelConfigs(agentId, guildId);
      setConfigs(res.configs);
    } catch (e: unknown) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  }, [agentId, guildId]);

  useEffect(() => {
    if (guildId) load();
  }, [load, guildId]);

  const handleSave = async (config: ChannelConfigDto) => {
    try {
      await upsertChannelConfig(agentId, config);
      await load();
    } catch (e: unknown) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };

  const handleDelete = async (channelId: string) => {
    try {
      await deleteChannelConfig(agentId, channelId);
      await load();
    } catch (e: unknown) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };

  const handleFieldChange = (idx: number, field: keyof ChannelConfigDto, value: unknown) => {
    setConfigs(prev => prev.map((c, i) => i === idx ? { ...c, [field]: value } : c));
  };

  return (
    <div className="space-y-6">
      <div className="flex items-center gap-4">
        <label className="text-label-lg text-on-surface-variant">Guild ID</label>
        <input
          type="text"
          className="input-outlined flex-1"
          value={guildId}
          onChange={e => setGuildId(e.target.value)}
          placeholder="Enter guild ID"
        />
        <button className="btn-filled" onClick={load} disabled={!guildId || loading}>
          Load
        </button>
      </div>

      {error && (
        <div className="card-outlined border-error bg-error-container/30 p-4">
          <p className="text-body-lg text-error-on-container">{error}</p>
        </div>
      )}

      {loading && <p className="text-body-lg text-on-surface-variant">Loading...</p>}

      {configs.length > 0 && (
        <div className="card-outlined overflow-x-auto">
          <table className="w-full text-body-md">
            <thead>
              <tr className="border-b border-outline-variant">
                <th className="p-3 text-left text-label-lg">Channel</th>
                <th className="p-3 text-center text-label-lg">Readable</th>
                <th className="p-3 text-center text-label-lg">Writable</th>
                <th className="p-3 text-center text-label-lg">Whitelisted</th>
                <th className="p-3 text-center text-label-lg">Heartbeat</th>
                <th className="p-3 text-center text-label-lg">Interval (sec)</th>
                <th className="p-3 text-center text-label-lg">Actions</th>
              </tr>
            </thead>
            <tbody>
              {configs.map((config, idx) => (
                <tr key={config.channel_id} className="border-b border-outline-variant/50">
                  <td className="p-3">
                    <div className="text-body-md">{config.channel_name || config.channel_id}</div>
                    <div className="text-body-sm text-on-surface-variant">{config.channel_id}</div>
                  </td>
                  <td className="p-3 text-center">
                    <input
                      type="checkbox"
                      checked={config.readable}
                      onChange={e => handleFieldChange(idx, 'readable', e.target.checked)}
                    />
                  </td>
                  <td className="p-3 text-center">
                    <input
                      type="checkbox"
                      checked={config.writable}
                      onChange={e => handleFieldChange(idx, 'writable', e.target.checked)}
                    />
                  </td>
                  <td className="p-3 text-center">
                    <input
                      type="checkbox"
                      checked={config.whitelisted}
                      onChange={e => handleFieldChange(idx, 'whitelisted', e.target.checked)}
                    />
                  </td>
                  <td className="p-3 text-center">
                    <input
                      type="checkbox"
                      checked={config.heartbeat_enabled}
                      onChange={e => handleFieldChange(idx, 'heartbeat_enabled', e.target.checked)}
                    />
                  </td>
                  <td className="p-3 text-center">
                    <input
                      type="number"
                      className="input-outlined w-24 text-center"
                      value={config.heartbeat_interval_secs ?? ''}
                      onChange={e => handleFieldChange(idx, 'heartbeat_interval_secs', e.target.value ? Number(e.target.value) : null)}
                      placeholder="Global"
                    />
                  </td>
                  <td className="p-3 text-center space-x-2">
                    <button
                      className="btn-tonal text-body-sm"
                      onClick={() => handleSave(config)}
                    >
                      Save
                    </button>
                    <button
                      className="btn-text text-error text-body-sm"
                      onClick={() => handleDelete(config.channel_id)}
                    >
                      Delete
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}

      {!loading && configs.length === 0 && guildId && (
        <div className="empty-state">
          <p className="text-body-lg text-on-surface-variant">No channel configs found for this guild.</p>
        </div>
      )}
    </div>
  );
}
