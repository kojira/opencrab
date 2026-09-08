import type { LlmLogStat } from "./model";
import { formatNumber } from "./model";
import { Collapsible } from "./Content";

// ── Stats section ─────────────────────────────────────────────────

function StatsSection({ stats }: { stats: LlmLogStat[] }) {
  if (stats.length === 0) return null;

  const totalCalls = stats.reduce((s, d) => s + d.count, 0);
  const totalTokens = stats.reduce((s, d) => s + d.total_tokens, 0);
  const totalErrors = stats.reduce((s, d) => s + d.error_count, 0);
  const totalCacheRead = stats.reduce((s, d) => s + d.cache_read_tokens, 0);
  const totalCacheCreation = stats.reduce((s, d) => s + d.cache_creation_tokens, 0);
  const weightedLatency = stats.reduce((s, d) => s + d.avg_latency_ms * d.count, 0);
  const avgLatency = totalCalls > 0 ? Math.round(weightedLatency / totalCalls) : 0;
  const maxDayTokens = Math.max(...stats.map((d) => d.total_tokens), 1);

  return (
    <div className="card-elevated space-y-3">
      <h3 className="text-title-md text-on-surface font-semibold flex items-center gap-2">
        <span className="material-symbols-outlined text-primary">analytics</span>
        過去30日の統計
      </h3>

      {/* Summary row */}
      <div className="flex flex-wrap gap-4 text-label-sm">
        <span className="inline-flex items-center gap-1 px-2 py-1 rounded-full bg-primary-container text-on-primary-container">
          <span className="material-symbols-outlined text-sm">call_made</span>
          合計呼び出し: <strong>{formatNumber(totalCalls)}</strong>
        </span>
        <span className="inline-flex items-center gap-1 px-2 py-1 rounded-full bg-secondary-container text-on-secondary-container">
          <span className="material-symbols-outlined text-sm">data_usage</span>
          合計トークン: <strong>{formatNumber(totalTokens)}</strong>
        </span>
        <span className="inline-flex items-center gap-1 px-2 py-1 rounded-full bg-tertiary-container text-on-tertiary-container">
          <span className="material-symbols-outlined text-sm">speed</span>
          平均レイテンシ: <strong>{formatNumber(avgLatency)}ms</strong>
        </span>
        {totalErrors > 0 && (
          <span className="inline-flex items-center gap-1 px-2 py-1 rounded-full bg-error-container text-on-error-container">
            <span className="material-symbols-outlined text-sm">error</span>
            エラー: <strong>{formatNumber(totalErrors)}</strong>
          </span>
        )}
        {totalCacheRead > 0 && (
          <span className="inline-flex items-center gap-1 px-2 py-1 rounded-full bg-green-100 dark:bg-green-900 text-green-800 dark:text-green-200">
            <span className="material-symbols-outlined text-sm">cached</span>
            キャッシュヒット: <strong>{formatNumber(totalCacheRead)}</strong>
          </span>
        )}
        {totalCacheCreation > 0 && (
          <span className="inline-flex items-center gap-1 px-2 py-1 rounded-full bg-yellow-100 dark:bg-yellow-900 text-yellow-800 dark:text-yellow-200">
            <span className="material-symbols-outlined text-sm">save</span>
            キャッシュ書込: <strong>{formatNumber(totalCacheCreation)}</strong>
          </span>
        )}
      </div>

      {/* Bar chart */}
      <Collapsible title="日別トークン使用量" icon="📊" defaultOpen={false}>
        <div className="p-3 space-y-1">
          {stats.map((day) => (
            <div key={day.date} className="flex items-center gap-2 text-label-sm">
              <span className="w-20 text-on-surface-variant shrink-0 font-mono">
                {day.date.slice(5)}
              </span>
              <div className="flex-1 h-4 bg-surface-container-high rounded overflow-hidden">
                <div
                  className="h-full bg-primary rounded transition-all"
                  style={{ width: `${Math.max((day.total_tokens / maxDayTokens) * 100, 1)}%` }}
                  title={`${formatNumber(day.total_tokens)} tokens`}
                />
              </div>
              <span className="w-20 text-right text-on-surface-variant shrink-0">
                {formatNumber(day.total_tokens)}
              </span>
              {day.error_count > 0 && (
                <span className="text-error text-label-sm">({day.error_count} err)</span>
              )}
            </div>
          ))}
        </div>
      </Collapsible>
    </div>
  );
}

export { StatsSection };
