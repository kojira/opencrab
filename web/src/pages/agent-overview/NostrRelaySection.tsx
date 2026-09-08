import { useState, useEffect, useCallback } from 'react';
import { useTranslation } from 'react-i18next';
import {
  getNostrRelayConfig,
  updateNostrRelayConfig,
  type NostrRelayConfigDto,
} from '../../api/nostr';

/**
 * Nostr 受信 → Discord 転記先の設定（issue #252 段階 B）。
 *
 * 自分宛の Nostr 受信（メンション/リプライ/DM）を、指定した Discord チャンネルの
 * webhook へ転記する。webhook URL の生値は API から返らない（伏字のみ）ため、入力欄は
 * 常に空で、現在値は伏字表示する。
 *
 * webhook_url は三状態（省略=保持 / null=消去 / 文字列=設定）。保存で入力欄が空なら
 * webhook_url を**送らず現状維持**する（enabled トグルだけの保存で既存転記先が消えない）。
 * 消去は「転記先を削除」ボタンの明示操作に分離する。
 */
export function NostrRelaySection({ agentId }: { agentId: string }) {
  const { t } = useTranslation();
  const [cfg, setCfg] = useState<NostrRelayConfigDto | null>(null);
  const [enabled, setEnabled] = useState(false);
  const [webhookUrl, setWebhookUrl] = useState('');
  const [saving, setSaving] = useState(false);
  const [message, setMessage] = useState<string | null>(null);
  const [warning, setWarning] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      const c = await getNostrRelayConfig(agentId);
      setCfg(c);
      setEnabled(c.enabled);
      // 生 URL は取得できない（伏字のみ）。入力欄は空のまま上書き入力させる。
      setWebhookUrl('');
    } catch {
      setCfg(null);
    }
  }, [agentId]);

  useEffect(() => {
    void load();
  }, [load]);

  // 入力欄が空なら webhook_url を送らず（保持）、入力があればそれを設定する。
  const save = async () => {
    setSaving(true);
    setMessage(null);
    setWarning(null);
    try {
      const trimmed = webhookUrl.trim();
      const res = await updateNostrRelayConfig(agentId, {
        enabled,
        // 空欄 = 現状維持なのでフィールド自体を送らない（undefined は JSON から除かれる）。
        ...(trimmed === '' ? {} : { webhook_url: trimmed }),
      });
      setEnabled(res.enabled);
      setWebhookUrl('');
      setMessage(t('common.save') + ' OK');
      if (res.warning) setWarning(res.warning);
      await load();
    } catch (e) {
      setMessage(String(e));
    } finally {
      setSaving(false);
    }
  };

  // 転記先を明示的に消去する（null を送る）。誤操作を避けるため確認する。
  const clearWebhook = async () => {
    if (!window.confirm(t('agentDetail.nostrRelayDeleteConfirm'))) return;
    setSaving(true);
    setMessage(null);
    setWarning(null);
    try {
      const res = await updateNostrRelayConfig(agentId, {
        enabled,
        webhook_url: null,
      });
      setEnabled(res.enabled);
      setWebhookUrl('');
      setMessage(t('common.save') + ' OK');
      if (res.warning) setWarning(res.warning);
      await load();
    } catch (e) {
      setMessage(String(e));
    } finally {
      setSaving(false);
    }
  };

  return (
    <div className="card-outlined mt-6">
      <h2 className="section-title flex items-center gap-2">
        <span className="material-symbols-outlined text-xl text-primary">forward_to_inbox</span>
        {t('agentDetail.nostrRelay')}
      </h2>
      <p className="text-body-sm text-on-surface-variant mb-3">
        {t('agentDetail.nostrRelayDesc')}
      </p>
      {message && <p className="text-body-sm mb-2 text-on-surface-variant">{message}</p>}
      {warning && <p className="text-body-sm mb-2 text-error">{warning}</p>}
      <div className="space-y-3">
        <label className="flex items-center gap-2">
          <input
            type="checkbox"
            checked={enabled}
            onChange={(e) => setEnabled(e.target.checked)}
          />
          <span className="text-label-lg">{t('agentDetail.nostrRelayEnabled')}</span>
        </label>
        <div>
          <label className="text-label-lg text-on-surface-variant block mb-1">
            {t('agentDetail.nostrRelayWebhook')}
          </label>
          {cfg?.has_webhook && (
            <p className="text-body-sm text-on-surface-variant mb-1">
              {t('agentDetail.nostrRelayCurrent', { url: cfg.webhook_url_masked })}
            </p>
          )}
          <input
            className="input w-full"
            placeholder="https://discord.com/api/webhooks/..."
            value={webhookUrl}
            onChange={(e) => setWebhookUrl(e.target.value)}
          />
          <p className="text-body-sm text-on-surface-variant mt-1">
            {t('agentDetail.nostrRelayWebhookHint')}
          </p>
        </div>
        <div className="flex flex-wrap gap-2">
          <button
            type="button"
            className="btn-filled"
            disabled={saving}
            onClick={() => void save()}
          >
            {t('common.save')}
          </button>
          {cfg?.has_webhook && (
            <button
              type="button"
              className="btn-text text-error"
              disabled={saving}
              onClick={() => void clearWebhook()}
            >
              {t('agentDetail.nostrRelayDelete')}
            </button>
          )}
        </div>
      </div>
    </div>
  );
}
