import { useState, useEffect, useCallback } from 'react';
import { useTranslation } from 'react-i18next';
import { useAgentContext } from '../hooks/useAgentContext';
import { getSleepLogs, restoreSkill, type SleepLog } from '../api/sleep';

const ACTION_STYLE: Record<string, string> = {
  retired: 'badge-neutral',
  refined: 'badge-info',
  created: 'bg-tertiary-container text-tertiary-on-container badge',
  merged: 'badge-info',
  kept: 'badge-neutral',
};

export default function AgentSleep() {
  const { agentId } = useAgentContext();
  const { t } = useTranslation();
  const [logs, setLogs] = useState<SleepLog[]>([]);
  const [loading, setLoading] = useState(true);
  const [restoring, setRestoring] = useState<string | null>(null);

  const load = useCallback(() => {
    setLoading(true);
    getSleepLogs(agentId)
      .then((r) => setLogs(r.logs))
      .catch(() => {})
      .finally(() => setLoading(false));
  }, [agentId]);

  useEffect(() => {
    load();
  }, [load]);

  const handleRestore = async (skillId: string) => {
    setRestoring(skillId);
    try {
      await restoreSkill(agentId, skillId);
      load();
    } catch {
      /* noop */
    } finally {
      setRestoring(null);
    }
  };

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between">
        <div>
          <h2 className="text-lg font-semibold text-on-surface">{t('sleep.title')}</h2>
          <p className="text-body-sm text-on-surface-variant">{t('sleep.subtitle')}</p>
        </div>
        <button className="btn-text text-sm" onClick={load}>
          <span className="material-symbols-outlined text-base">refresh</span>
          {t('common.refresh', 'Refresh')}
        </button>
      </div>

      {loading && <p className="text-body-md text-on-surface-variant">{t('common.loading')}</p>}

      {!loading && logs.length === 0 && (
        <div className="card-outlined text-center py-8">
          <span className="material-symbols-outlined text-3xl text-on-surface-variant/50">
            bedtime
          </span>
          <p className="text-body-md text-on-surface-variant mt-2">{t('sleep.empty')}</p>
        </div>
      )}

      <div className="space-y-3">
        {logs.map((log) => {
          const a = log.audit;
          const curation = a?.skill_curation ?? [];
          const changed = curation.filter((c) => c.action !== 'kept');
          return (
            <div key={log.id} className="card-outlined">
              <div className="flex items-center justify-between mb-2 flex-wrap gap-2">
                <div className="flex items-center gap-2">
                  <span className="material-symbols-outlined text-primary text-lg">bedtime</span>
                  <span className="text-body-sm text-on-surface-variant">
                    {log.created_at ? new Date(log.created_at).toLocaleString() : '-'}
                  </span>
                  {a?.trigger && <span className="badge-neutral">{a.trigger}</span>}
                </div>
                <span className="text-body-sm text-on-surface-variant">
                  {t('sleep.cost', {
                    calls: a?.cost?.llm_calls ?? 0,
                    ms: a?.cost?.latency_ms ?? 0,
                  })}
                </span>
              </div>

              {changed.length === 0 ? (
                <p className="text-body-sm text-on-surface-variant ml-7">{t('sleep.noChanges')}</p>
              ) : (
                <ul className="space-y-2 ml-7">
                  {changed.map((c, i) => (
                    <li key={i} className="flex items-start gap-2 flex-wrap">
                      <span className={ACTION_STYLE[c.action] ?? 'badge-neutral'}>{c.action}</span>
                      <span className="text-body-md text-on-surface font-medium">{c.skill}</span>
                      {c.reason && (
                        <span className="text-body-sm text-on-surface-variant">— {c.reason}</span>
                      )}
                      {c.action === 'retired' && c.skill_id && (
                        <button
                          className="btn-text text-xs text-primary"
                          disabled={restoring === c.skill_id}
                          onClick={() => handleRestore(c.skill_id!)}
                        >
                          {restoring === c.skill_id ? t('common.loading') : t('sleep.restore')}
                        </button>
                      )}
                    </li>
                  ))}
                </ul>
              )}

              {a?.llm_log_ids && a.llm_log_ids.length > 0 && (
                <p className="text-label-sm text-on-surface-variant/70 ml-7 mt-2">
                  {t('sleep.rawLogHint')}
                </p>
              )}
            </div>
          );
        })}
      </div>
    </div>
  );
}
