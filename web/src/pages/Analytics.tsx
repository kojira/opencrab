import { useState, useEffect } from 'react';
import { useTranslation } from 'react-i18next';
import { getAgents } from '../api/agents';
import { getMetricsSummary, getMetricsDetail } from '../api/analytics';
import type {
  AgentSummary,
  LlmMetricsSummaryDto,
  LlmMetricsDetailDto,
} from '../api/types';

function formatNumber(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}K`;
  return String(n);
}

function MetricCard({
  icon,
  label,
  value,
}: {
  icon: string;
  label: string;
  value: string;
}) {
  return (
    <div className="card-elevated">
      <div className="flex items-center gap-2 mb-2">
        <span className="material-symbols-outlined text-lg text-primary">
          {icon}
        </span>
        <p className="text-label-lg text-on-surface-variant">{label}</p>
      </div>
      <p className="text-headline-sm text-on-surface font-semibold">{value}</p>
    </div>
  );
}

export default function Analytics() {
  const { t } = useTranslation();
  const [agents, setAgents] = useState<AgentSummary[] | null>(null);
  const [selectedAgent, setSelectedAgent] = useState<string | null>(null);
  const [selectedPeriod, setSelectedPeriod] = useState('week');
  const [summary, setSummary] = useState<LlmMetricsSummaryDto | null>(null);
  const [detail, setDetail] = useState<LlmMetricsDetailDto[] | null>(null);

  useEffect(() => {
    getAgents().then(setAgents).catch(() => {});
  }, []);

  useEffect(() => {
    if (!selectedAgent) {
      setSummary(null);
      setDetail(null);
      return;
    }
    getMetricsSummary(selectedAgent, selectedPeriod)
      .then(setSummary)
      .catch(() => setSummary(null));
    getMetricsDetail(selectedAgent, selectedPeriod)
      .then(setDetail)
      .catch(() => setDetail(null));
  }, [selectedAgent, selectedPeriod]);

  return (
    <div className="max-w-7xl mx-auto">
      <h1 className="page-title mb-6">{t('analytics.title')}</h1>

      {/* Controls */}
      <div className="card-elevated mb-6">
        <div className="flex gap-4">
          <div className="flex-1">
            <label className="block text-label-lg text-on-surface mb-2">
              <span className="flex items-center gap-1.5">
                <span className="material-symbols-outlined text-lg">
                  smart_toy
                </span>
                {t('analytics.agent')}
              </span>
            </label>
            {agents ? (
              <select
                className="select-outlined"
                onChange={(e) =>
                  setSelectedAgent(e.target.value || null)
                }
              >
                <option value="">{t('common.selectAgentPlaceholder')}</option>
                {agents.map((a) => (
                  <option key={a.id} value={a.id}>
                    {a.name}
                  </option>
                ))}
              </select>
            ) : (
              <p className="text-body-md text-on-surface-variant">
                {t('common.loading')}
              </p>
            )}
          </div>
          <div>
            <label className="block text-label-lg text-on-surface mb-2">
              <span className="flex items-center gap-1.5">
                <span className="material-symbols-outlined text-lg">
                  calendar_today
                </span>
                {t('analytics.period')}
              </span>
            </label>
            <div className="segmented-group">
              {[
                { value: 'day', label: t('analytics.24h') },
                { value: 'week', label: t('analytics.7days') },
                { value: 'month', label: t('analytics.30days') },
              ].map((p) => (
                <button
                  key={p.value}
                  className={
                    selectedPeriod === p.value
                      ? 'segmented-btn-active'
                      : 'segmented-btn'
                  }
                  onClick={() => setSelectedPeriod(p.value)}
                >
                  {p.label}
                </button>
              ))}
            </div>
          </div>
        </div>
      </div>

      {selectedAgent ? (
        <>
          {summary && (
            <div className="grid grid-cols-2 md:grid-cols-5 gap-4 mb-6">
              <MetricCard
                icon="api"
                label={t('analytics.apiCalls')}
                value={String(summary.count)}
              />
              <MetricCard
                icon="token"
                label={t('analytics.totalTokens')}
                value={formatNumber(summary.total_tokens)}
              />
              <MetricCard
                icon="payments"
                label={t('analytics.totalCost')}
                value={`$${summary.total_cost.toFixed(4)}`}
              />
              <MetricCard
                icon="speed"
                label={t('analytics.avgLatency')}
                value={`${summary.avg_latency.toFixed(0)}ms`}
              />
              <MetricCard
                icon="grade"
                label={t('analytics.avgQuality')}
                value={summary.avg_quality.toFixed(2)}
              />
            </div>
          )}

          <div className="card-outlined overflow-hidden">
            <div className="px-6 py-4 border-b border-outline-variant">
              <h2 className="section-title mb-0 flex items-center gap-2">
                <span className="material-symbols-outlined text-xl text-primary">
                  table_chart
                </span>
                {t('analytics.usageByModel')}
              </h2>
            </div>

            {detail === null ? (
              <div className="empty-state">
                <p className="text-body-lg text-on-surface-variant">
                  {t('common.loading')}
                </p>
              </div>
            ) : detail.length === 0 ? (
              <div className="empty-state">
                <span className="material-symbols-outlined empty-state-icon">
                  table_rows
                </span>
                <p className="empty-state-text">
                  {t('analytics.noUsageData')}
                </p>
              </div>
            ) : (
              <div className="overflow-x-auto">
                <table className="data-table">
                  <thead>
                    <tr>
                      <th>{t('analytics.provider')}</th>
                      <th>{t('analytics.model')}</th>
                      <th className="text-right">{t('analytics.requests')}</th>
                      <th className="text-right">{t('analytics.tokens')}</th>
                      <th className="text-right">{t('analytics.cost')}</th>
                      <th className="text-right">{t('analytics.avgLatency')}</th>
                    </tr>
                  </thead>
                  <tbody>
                    {detail.map((model, i) => (
                      <tr key={i}>
                        <td>{model.provider}</td>
                        <td className="font-mono">{model.model}</td>
                        <td className="text-right">{model.request_count}</td>
                        <td className="text-right">
                          {formatNumber(model.total_tokens)}
                        </td>
                        <td className="text-right">
                          ${model.total_cost.toFixed(4)}
                        </td>
                        <td className="text-right">
                          {model.avg_latency.toFixed(0)}ms
                        </td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            )}
          </div>
        </>
      ) : (
        <div className="empty-state">
          <span className="material-symbols-outlined empty-state-icon">
            analytics
          </span>
          <p className="empty-state-text">
            {t('analytics.selectAgent')}
          </p>
        </div>
      )}
    </div>
  );
}
