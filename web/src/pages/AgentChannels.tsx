import { useState, useEffect, useCallback } from 'react';
import { useAgentContext } from '../hooks/useAgentContext';
import { listChannelConfigs, upsertChannelConfig, deleteChannelConfig } from '../api/channel_configs';
import { listSchedules, createSchedule, updateSchedule, deleteSchedule } from '../api/schedules';
import { getSessions } from '../api/sessions';
import type { ChannelConfigDto, ScheduleDto, SessionDto, UpdateScheduleRequest } from '../api/types';
import { conversationTitle } from '../lib/conversationTitle';
import ConfirmDialog from '../components/ui/ConfirmDialog';
import { useTranslation } from "react-i18next";

export default function AgentChannels() {
  const { agentId } = useAgentContext();
  const { t } = useTranslation();
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
      <TimeTriggersSection agentId={agentId} />

      <div className="flex flex-wrap items-center gap-3">
        <label className="text-label-lg text-on-surface-variant">{t("channels.guildIdLabel")}</label>
        <input
          type="text"
          className="input-outlined flex-1 min-w-0"
          value={guildId}
          onChange={e => setGuildId(e.target.value)}
          placeholder={t("channels.guildIdPlaceholder")}
        />
        <button className="btn-filled" onClick={load} disabled={!guildId || loading}>
          {t("channels.loadButton")}
        </button>
      </div>

      {error && (
        <div className="card-outlined border-error bg-error-container/30 p-4">
          <p className="text-body-lg text-error-on-container">{error}</p>
        </div>
      )}

      {loading && <p className="text-body-lg text-on-surface-variant">{t("common.loading")}</p>}

      {configs.length > 0 && (
        <div className="card-outlined overflow-x-auto">
          <table className="w-full text-body-md">
            <thead>
              <tr className="border-b border-outline-variant">
                <th className="p-3 text-left text-label-lg">{t("channels.tableChannel")}</th>
                <th className="p-3 text-center text-label-lg">{t("channels.tableReadable")}</th>
                <th className="p-3 text-center text-label-lg">{t("channels.tableWritable")}</th>
                <th className="p-3 text-center text-label-lg">{t("channels.tableWhitelisted")}</th>
                <th className="p-3 text-center text-label-lg">{t("channels.tableActions")}</th>
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
                  <td className="p-3 text-center space-x-2">
                    <button
                      className="btn-tonal text-body-sm"
                      onClick={() => handleSave(config)}
                    >
                      {t("common.save")}
                    </button>
                    <button
                      className="btn-text text-error text-body-sm"
                      onClick={() => handleDelete(config.channel_id)}
                    >
                      {t("common.delete")}
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
          <p className="text-body-lg text-on-surface-variant">{t("channels.noConfigs")}</p>
        </div>
      )}
    </div>
  );
}

type TriggerKind = 'interval' | 'fixed';

interface TriggerForm {
  sessionId: string;
  kind: TriggerKind;
  intervalSecs: string;
  cronExpr: string;
  timezone: string;
  message: string;
  enabled: boolean;
}

const DEFAULT_TIMEZONE = 'Asia/Tokyo';

const EMPTY_FORM: TriggerForm = {
  sessionId: '',
  kind: 'interval',
  intervalSecs: '',
  cronExpr: '',
  timezone: DEFAULT_TIMEZONE,
  message: '',
  enabled: true,
};

/** 間隔トリガーは `@every` で始まる cron_expr（設計 §2.1）。 */
function isIntervalExpr(cronExpr: string): boolean {
  return cronExpr.trim().startsWith('@every');
}

/** `@every 1h30m` などを秒数に直す（編集フォームの初期値用）。解釈できなければ空文字。 */
function everyExprToSecs(cronExpr: string): string {
  const rest = cronExpr.trim().replace(/^@every/, '').trim();
  const units: Record<string, number> = { d: 86400, h: 3600, m: 60, s: 1 };
  const parts = rest.match(/\d+[dhms]/g);
  if (!parts || parts.join('') !== rest) return '';
  return String(parts.reduce((sum, p) => sum + Number(p.slice(0, -1)) * units[p.slice(-1)], 0));
}

function formFromSchedule(s: ScheduleDto): TriggerForm {
  const interval = isIntervalExpr(s.cron_expr);
  return {
    sessionId: s.session_id,
    kind: interval ? 'interval' : 'fixed',
    intervalSecs: interval ? everyExprToSecs(s.cron_expr) : '',
    cronExpr: interval ? '' : s.cron_expr,
    timezone: s.timezone,
    message: s.message,
    enabled: s.enabled,
  };
}

function cronExprFromForm(form: TriggerForm): string {
  return form.kind === 'interval' ? `@every ${form.intervalSecs.trim()}s` : form.cronExpr.trim();
}

function formatTime(value: string | null, none: string): string {
  return value ? new Date(value).toLocaleString() : none;
}

/**
 * セッションごとの時刻トリガー（`agent_schedules`・#612 設計 §2.1）。
 * 既存の `GET/POST /api/agents/{id}/schedules`, `PATCH/DELETE /api/schedules/{sid}` を呼ぶだけ。
 */
export function TimeTriggersSection({ agentId }: { agentId: string }) {
  const { t } = useTranslation();
  const [schedules, setSchedules] = useState<ScheduleDto[]>([]);
  const [sessions, setSessions] = useState<SessionDto[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // null = フォームを閉じている / 'new' = 追加 / number = その id を編集中。
  const [editing, setEditing] = useState<'new' | number | null>(null);
  const [form, setForm] = useState<TriggerForm>(EMPTY_FORM);
  const [saving, setSaving] = useState(false);
  const [deleteTarget, setDeleteTarget] = useState<ScheduleDto | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const [scheduleRes, page] = await Promise.all([listSchedules(agentId), getSessions()]);
      setSchedules(scheduleRes.schedules);
      setSessions(page.filter((s) => s.agent_ids.includes(agentId)));
    } catch (e: unknown) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  }, [agentId]);

  useEffect(() => {
    void load();
  }, [load]);

  const sessionTitle = (sessionId: string): string => {
    const session = sessions.find((s) => s.id === sessionId);
    return session ? conversationTitle(session.id, session.theme, t('sessions.newConversation')) : sessionId;
  };

  const reportFailure = (e: unknown) => {
    const message = e instanceof Error ? e.message : String(e);
    setError(t('channels.triggers.rejected', { message }));
  };

  const openNew = () => {
    setForm(EMPTY_FORM);
    setEditing('new');
    setError(null);
  };

  const openEdit = (s: ScheduleDto) => {
    setForm(formFromSchedule(s));
    setEditing(s.id);
    setError(null);
  };

  const submit = async () => {
    setSaving(true);
    setError(null);
    try {
      const cronExpr = cronExprFromForm(form);
      if (editing === 'new') {
        await createSchedule(agentId, {
          session_id: form.sessionId,
          cron_expr: cronExpr,
          ...(form.kind === 'fixed' ? { timezone: form.timezone.trim() } : {}),
          message: form.message,
          enabled: form.enabled,
        });
      } else if (typeof editing === 'number') {
        // 変えた項目だけ送る（同じ間隔を `@every Ns` に書き直して次回の起点を動かさない）。
        const original = schedules.find((s) => s.id === editing);
        const before = original ? formFromSchedule(original) : null;
        const timingChanged =
          !before ||
          before.kind !== form.kind ||
          (form.kind === 'interval'
            ? before.intervalSecs !== form.intervalSecs.trim()
            : before.cronExpr !== form.cronExpr.trim());
        const patch: UpdateScheduleRequest = {};
        if (timingChanged) patch.cron_expr = cronExpr;
        if (form.kind === 'fixed' && before?.timezone !== form.timezone.trim()) {
          patch.timezone = form.timezone.trim();
        }
        if (before?.message !== form.message) patch.message = form.message;
        if (before?.enabled !== form.enabled) patch.enabled = form.enabled;
        await updateSchedule(editing, patch);
      }
      setEditing(null);
      await load();
    } catch (e: unknown) {
      reportFailure(e);
    } finally {
      setSaving(false);
    }
  };

  const toggleEnabled = async (s: ScheduleDto, enabled: boolean) => {
    setError(null);
    try {
      await updateSchedule(s.id, { enabled });
      await load();
    } catch (e: unknown) {
      reportFailure(e);
    }
  };

  const confirmDelete = async () => {
    if (!deleteTarget) return;
    const target = deleteTarget;
    setDeleteTarget(null);
    setError(null);
    try {
      await deleteSchedule(target.id);
      await load();
    } catch (e: unknown) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };

  const bySession = schedules.reduce<Record<string, ScheduleDto[]>>((acc, s) => {
    (acc[s.session_id] ??= []).push(s);
    return acc;
  }, {});

  const canSubmit =
    !saving &&
    form.sessionId !== '' &&
    form.message.trim() !== '' &&
    (form.kind === 'interval'
      ? /^[1-9]\d*$/.test(form.intervalSecs.trim())
      : form.cronExpr.trim() !== '' && form.timezone.trim() !== '');

  return (
    <div className="card-outlined">
      <div className="flex items-center justify-between gap-3 flex-wrap">
        <h2 className="section-title flex items-center gap-2 mb-0">
          <span className="material-symbols-outlined text-xl text-primary">schedule</span>
          {t('channels.triggers.title')}
        </h2>
        <button type="button" className="btn-filled" onClick={openNew} disabled={saving}>
          <span className="material-symbols-outlined text-xl">add</span>
          {t('channels.triggers.add')}
        </button>
      </div>
      <p className="text-body-sm text-on-surface-variant mt-2 mb-4">
        {t('channels.triggers.description')}
      </p>

      {error && (
        <div className="card-outlined border-error bg-error-container/30 p-4 mb-4" role="alert">
          <p className="text-body-lg text-error-on-container">{error}</p>
        </div>
      )}

      {editing !== null && (
        <div className="card-outlined p-4 mb-4 space-y-3">
          <div>
            <label className="text-label-lg text-on-surface-variant block mb-1" htmlFor="trigger-session">
              {t('channels.triggers.session')}
            </label>
            <select
              id="trigger-session"
              className="select-outlined"
              value={form.sessionId}
              disabled={editing !== 'new'}
              onChange={(e) => setForm({ ...form, sessionId: e.target.value })}
            >
              <option value="">{t('channels.triggers.selectSession')}</option>
              {sessions.map((s) => (
                <option key={s.id} value={s.id}>
                  {sessionTitle(s.id)}
                </option>
              ))}
            </select>
            {editing === 'new' && sessions.length === 0 && (
              <p className="text-body-sm text-on-surface-variant mt-1">{t('channels.triggers.noSessions')}</p>
            )}
          </div>
          <div>
            <label className="text-label-lg text-on-surface-variant block mb-1" htmlFor="trigger-kind">
              {t('channels.triggers.kind')}
            </label>
            <select
              id="trigger-kind"
              className="select-outlined"
              value={form.kind}
              onChange={(e) => setForm({ ...form, kind: e.target.value as TriggerKind })}
            >
              <option value="interval">{t('channels.triggers.interval')}</option>
              <option value="fixed">{t('channels.triggers.fixedTime')}</option>
            </select>
          </div>
          {form.kind === 'interval' ? (
            <div>
              <label className="text-label-lg text-on-surface-variant block mb-1" htmlFor="trigger-interval">
                {t('channels.triggers.intervalSecs')}
              </label>
              <input
                id="trigger-interval"
                type="number"
                min={1}
                className="input-outlined"
                value={form.intervalSecs}
                onChange={(e) => setForm({ ...form, intervalSecs: e.target.value })}
              />
            </div>
          ) : (
            <>
              <div>
                <label className="text-label-lg text-on-surface-variant block mb-1" htmlFor="trigger-cron">
                  {t('channels.triggers.cronExpr')}
                </label>
                <input
                  id="trigger-cron"
                  type="text"
                  className="input-outlined font-mono"
                  value={form.cronExpr}
                  onChange={(e) => setForm({ ...form, cronExpr: e.target.value })}
                />
              </div>
              <div>
                <label className="text-label-lg text-on-surface-variant block mb-1" htmlFor="trigger-timezone">
                  {t('channels.triggers.timezone')}
                </label>
                <input
                  id="trigger-timezone"
                  type="text"
                  className="input-outlined"
                  value={form.timezone}
                  onChange={(e) => setForm({ ...form, timezone: e.target.value })}
                />
              </div>
            </>
          )}
          <div>
            <label className="text-label-lg text-on-surface-variant block mb-1" htmlFor="trigger-message">
              {t('channels.triggers.prompt')}
            </label>
            <textarea
              id="trigger-message"
              className="input-outlined w-full"
              rows={4}
              value={form.message}
              onChange={(e) => setForm({ ...form, message: e.target.value })}
            />
          </div>
          <label className="flex items-center gap-2">
            <input
              type="checkbox"
              checked={form.enabled}
              onChange={(e) => setForm({ ...form, enabled: e.target.checked })}
            />
            <span className="text-label-lg">{t('channels.triggers.enabled')}</span>
          </label>
          <div className="flex gap-2 justify-end">
            <button type="button" className="btn-outlined" onClick={() => setEditing(null)} disabled={saving}>
              {t('common.cancel')}
            </button>
            <button type="button" className="btn-filled" onClick={() => void submit()} disabled={!canSubmit}>
              {t('common.save')}
            </button>
          </div>
        </div>
      )}

      {loading && <p className="text-body-lg text-on-surface-variant">{t('common.loading')}</p>}

      {!loading && schedules.length === 0 && (
        <p className="text-body-lg text-on-surface-variant">{t('channels.triggers.empty')}</p>
      )}

      {Object.entries(bySession).map(([sessionId, rows]) => (
        <div key={sessionId} className="mb-4">
          <h3 className="text-title-sm text-on-surface mb-2">{sessionTitle(sessionId)}</h3>
          <div className="overflow-x-auto">
            <table className="w-full text-body-md">
              <thead>
                <tr className="border-b border-outline-variant">
                  <th className="p-3 text-left text-label-lg">{t('channels.triggers.kind')}</th>
                  <th className="p-3 text-left text-label-lg">{t('channels.triggers.prompt')}</th>
                  <th className="p-3 text-center text-label-lg">{t('channels.triggers.enabled')}</th>
                  <th className="p-3 text-left text-label-lg">{t('channels.triggers.next')}</th>
                  <th className="p-3 text-left text-label-lg">{t('channels.triggers.lastFired')}</th>
                  <th className="p-3 text-center text-label-lg">{t('channels.tableActions')}</th>
                </tr>
              </thead>
              <tbody>
                {rows.map((s) => {
                  const interval = isIntervalExpr(s.cron_expr);
                  return (
                    <tr key={s.id} className="border-b border-outline-variant/50 align-top">
                      <td className="p-3">
                        <div>{interval ? t('channels.triggers.interval') : t('channels.triggers.fixedTime')}</div>
                        <div className="text-body-sm text-on-surface-variant font-mono">{s.cron_expr}</div>
                        {!interval && <div className="text-body-sm text-on-surface-variant">{s.timezone}</div>}
                      </td>
                      <td className="p-3 max-w-xs">
                        <div className="truncate" title={s.message}>{s.message}</div>
                        {s.gated_reason && <div className="text-body-sm text-error mt-1">{s.gated_reason}</div>}
                      </td>
                      <td className="p-3 text-center">
                        <input
                          type="checkbox"
                          aria-label={t('channels.triggers.enabled')}
                          checked={s.enabled}
                          onChange={(e) => void toggleEnabled(s, e.target.checked)}
                        />
                      </td>
                      <td className="p-3">
                        {s.enabled ? formatTime(s.next_fire_at, t('channels.triggers.notYet')) : '-'}
                      </td>
                      <td className="p-3">{formatTime(s.last_fired_at, t('channels.triggers.notYet'))}</td>
                      <td className="p-3 text-center space-x-2 whitespace-nowrap">
                        <button type="button" className="btn-tonal text-body-sm" onClick={() => openEdit(s)}>
                          {t('common.edit')}
                        </button>
                        <button
                          type="button"
                          className="btn-text text-error text-body-sm"
                          onClick={() => setDeleteTarget(s)}
                        >
                          {t('common.delete')}
                        </button>
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        </div>
      ))}

      {deleteTarget && (
        <ConfirmDialog
          title={t('channels.triggers.deleteTitle')}
          message={t('channels.triggers.deleteConfirm')}
          onConfirm={() => void confirmDelete()}
          onCancel={() => setDeleteTarget(null)}
        />
      )}
    </div>
  );
}
