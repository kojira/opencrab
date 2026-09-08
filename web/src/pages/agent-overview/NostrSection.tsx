import { useState, useEffect, useCallback } from 'react';
import { useTranslation } from 'react-i18next';
import {
  getNostrConfig,
  updateNostrConfig,
  deleteNostrConfig,
  generateNostrKey,
  listSessionWatches,
  createSessionWatch,
  deleteSessionWatch,
  type NostrConfigDto,
  type SessionWatchDto,
} from '../../api/nostr';

export function NostrSection({ agentId }: { agentId: string }) {
  const { t } = useTranslation();
  const [cfg, setCfg] = useState<NostrConfigDto | null>(null);
  const [secretKey, setSecretKey] = useState('');
  const [vanityPrefix, setVanityPrefix] = useState('');
  const [relays, setRelays] = useState('');
  const [authors, setAuthors] = useState('');
  const [keywords, setKeywords] = useState('');
  const [kinds, setKinds] = useState('');
  const [enabled, setEnabled] = useState(false);
  const [saving, setSaving] = useState(false);
  const [message, setMessage] = useState<string | null>(null);
  const [watches, setWatches] = useState<SessionWatchDto[]>([]);
  const [watchSessionId, setWatchSessionId] = useState('');
  const [watchInterval, setWatchInterval] = useState('');
  const [watchAuthors, setWatchAuthors] = useState('');
  const [watchKeywords, setWatchKeywords] = useState('');
  const [watchKinds, setWatchKinds] = useState('');

  const load = useCallback(async () => {
    try {
      const c = await getNostrConfig(agentId);
      setCfg(c);
      setRelays(c.relays.join(', '));
      setAuthors(c.filter.authors.join(', '));
      setKeywords(c.filter.keywords.join(', '));
      setKinds(c.filter.kinds.join(', '));
      setEnabled(c.enabled);
    } catch {
      setCfg(null);
      return;
    }
    try {
      const listed = await listSessionWatches(agentId);
      setWatches(listed.watches);
    } catch (e) {
      setMessage(String(e));
    }
  }, [agentId]);

  useEffect(() => {
    void load();
  }, [load]);

  const splitList = (s: string) =>
    s
      .split(',')
      .map((x) => x.trim())
      .filter((x) => x.length > 0);

  const save = async (nextEnabled: boolean) => {
    setSaving(true);
    setMessage(null);
    try {
      const res = await updateNostrConfig(agentId, {
        secret_key: secretKey.trim() === '' ? undefined : secretKey.trim(),
        relays: splitList(relays),
        authors: splitList(authors),
        keywords: splitList(keywords),
        kinds: splitList(kinds)
          .map((k) => parseInt(k, 10))
          .filter((n) => !Number.isNaN(n)),
        enabled: nextEnabled,
      });
      setEnabled(res.enabled);
      setSecretKey('');
      setMessage(t('common.save') + ' OK');
      await load();
    } catch (e) {
      setMessage(String(e));
    } finally {
      setSaving(false);
    }
  };

  const remove = async () => {
    setSaving(true);
    try {
      await deleteNostrConfig(agentId);
      setMessage('deleted');
      await load();
    } catch (e) {
      setMessage(String(e));
    } finally {
      setSaving(false);
    }
  };

  const generate = async () => {
    // 既存鍵があるなら上書き確認（アイデンティティ喪失を防ぐ）。
    const overwrite = Boolean(cfg?.has_secret_key);
    if (overwrite && !window.confirm(t('agentDetail.nostrGenerateOverwriteConfirm'))) {
      return;
    }
    setSaving(true);
    setMessage(null);
    try {
      const res = await generateNostrKey(agentId, {
        prefix: vanityPrefix.trim() === '' ? undefined : vanityPrefix.trim(),
        overwrite,
      });
      setVanityPrefix('');
      setSecretKey('');
      setMessage(t('agentDetail.nostrGenerated', { npub: res.npub }));
      await load();
    } catch (e) {
      setMessage(String(e));
    } finally {
      setSaving(false);
    }
  };

  const addWatch = async () => {
    const interval = Number.parseInt(watchInterval.trim(), 10);
    if (watchSessionId.trim() === '') {
      setMessage(t('agentDetail.nostrWatchSessionRequired'));
      return;
    }
    if (!Number.isInteger(interval) || interval <= 0) {
      setMessage(t('agentDetail.nostrWatchIntervalRequired'));
      return;
    }
    setSaving(true);
    setMessage(null);
    try {
      await createSessionWatch(agentId, {
        session_id: watchSessionId.trim(),
        interval_secs: interval,
        filter: {
          authors: splitList(watchAuthors),
          keywords: splitList(watchKeywords),
          kinds: splitList(watchKinds)
            .map((k) => parseInt(k, 10))
            .filter((n) => !Number.isNaN(n)),
        },
      });
      setWatchSessionId('');
      setWatchInterval('');
      setWatchAuthors('');
      setWatchKeywords('');
      setWatchKinds('');
      await load();
    } catch (e) {
      setMessage(String(e));
    } finally {
      setSaving(false);
    }
  };

  const removeWatch = async (watchId: number) => {
    setSaving(true);
    setMessage(null);
    try {
      await deleteSessionWatch(agentId, watchId);
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
        <span className="material-symbols-outlined text-xl text-primary">hub</span>
        {t('agentDetail.nostr')}
      </h2>
      <p className="text-body-sm text-on-surface-variant mb-3">
        {t('agentDetail.nostrDesc')}
        {cfg?.running && (
          <span className="ml-2 text-tertiary">● {t('agentDetail.nostrRunning')}</span>
        )}
      </p>
      {message && <p className="text-body-sm mb-2 text-on-surface-variant">{message}</p>}
      <div className="space-y-3">
        <div>
          <label className="text-label-lg text-on-surface-variant block mb-1">
            {t('agentDetail.nostrSecretKey')}
          </label>
          <input
            className="input w-full"
            type="password"
            placeholder={cfg?.has_secret_key ? cfg.secret_key_masked : 'nsec1...'}
            value={secretKey}
            onChange={(e) => setSecretKey(e.target.value)}
          />
          <p className="text-body-sm text-on-surface-variant mt-2">
            {t('agentDetail.nostrGenerateHint')}
          </p>
          <div className="flex flex-wrap items-center gap-2 mt-1">
            <input
              className="input flex-1 min-w-[8rem]"
              placeholder={t('agentDetail.nostrVanityPlaceholder')}
              value={vanityPrefix}
              onChange={(e) => setVanityPrefix(e.target.value)}
            />
            <button
              type="button"
              className="btn-outlined"
              disabled={saving}
              onClick={() => void generate()}
            >
              {t('agentDetail.nostrGenerate')}
            </button>
          </div>
        </div>
        <div>
          <label className="text-label-lg text-on-surface-variant block mb-1">
            {t('agentDetail.nostrRelays')}
          </label>
          <input
            className="input w-full"
            placeholder="wss://relay.example.com, wss://relay2.example.com"
            value={relays}
            onChange={(e) => setRelays(e.target.value)}
          />
        </div>
        <div className="grid grid-cols-1 sm:grid-cols-3 gap-3">
          <div>
            <label className="text-label-lg text-on-surface-variant block mb-1">
              {t('agentDetail.nostrAuthors')}
            </label>
            <input
              className="input w-full"
              placeholder="npub1..., npub1..."
              value={authors}
              onChange={(e) => setAuthors(e.target.value)}
            />
          </div>
          <div>
            <label className="text-label-lg text-on-surface-variant block mb-1">
              {t('agentDetail.nostrKeywords')}
            </label>
            <input
              className="input w-full"
              placeholder="opencrab, ..."
              value={keywords}
              onChange={(e) => setKeywords(e.target.value)}
            />
          </div>
          <div>
            <label className="text-label-lg text-on-surface-variant block mb-1">
              {t('agentDetail.nostrKinds')}
            </label>
            <input
              className="input w-full"
              placeholder="1"
              value={kinds}
              onChange={(e) => setKinds(e.target.value)}
            />
          </div>
        </div>
        <p className="text-body-sm text-on-surface-variant">{t('agentDetail.nostrFilterHint')}</p>
        <div className="border-t border-outline-variant/40 pt-3 space-y-2">
          <h3 className="text-label-lg">{t('agentDetail.nostrWatches')}</h3>
          <p className="text-body-sm text-on-surface-variant">{t('agentDetail.nostrWatchesDesc')}</p>
          {watches.length > 0 && (
            <ul className="space-y-1 text-body-sm">
              {watches.map((w) => (
                <li key={w.id} className="flex flex-wrap items-center gap-2">
                  <span>
                    {w.session_id} / {w.interval_secs}s
                  </span>
                  <button
                    type="button"
                    className="btn-text text-error"
                    disabled={saving}
                    onClick={() => void removeWatch(w.id)}
                  >
                    {t('common.delete')}
                  </button>
                </li>
              ))}
            </ul>
          )}
          <div className="grid grid-cols-1 sm:grid-cols-2 gap-3">
            <div>
              <label className="text-label-lg text-on-surface-variant block mb-1">
                {t('agentDetail.nostrWatchSession')}
              </label>
              <input
                className="input w-full"
                placeholder={`nostr-${agentId}`}
                value={watchSessionId}
                onChange={(e) => setWatchSessionId(e.target.value)}
              />
            </div>
            <div>
              <label className="text-label-lg text-on-surface-variant block mb-1">
                {t('agentDetail.nostrWatchInterval')}
              </label>
              <input
                className="input w-full"
                inputMode="numeric"
                value={watchInterval}
                onChange={(e) => setWatchInterval(e.target.value)}
              />
            </div>
          </div>
          <div className="grid grid-cols-1 sm:grid-cols-3 gap-3">
            <input
              className="input w-full"
              placeholder={t('agentDetail.nostrAuthors')}
              value={watchAuthors}
              onChange={(e) => setWatchAuthors(e.target.value)}
            />
            <input
              className="input w-full"
              placeholder={t('agentDetail.nostrKeywords')}
              value={watchKeywords}
              onChange={(e) => setWatchKeywords(e.target.value)}
            />
            <input
              className="input w-full"
              placeholder={t('agentDetail.nostrKinds')}
              value={watchKinds}
              onChange={(e) => setWatchKinds(e.target.value)}
            />
          </div>
          <button
            type="button"
            className="btn-outlined"
            disabled={saving}
            onClick={() => void addWatch()}
          >
            {t('agentDetail.nostrWatchAdd')}
          </button>
        </div>
        <div className="flex flex-wrap gap-2">
          <button
            type="button"
            className="btn-filled"
            disabled={saving}
            onClick={() => void save(true)}
          >
            {enabled ? t('agentDetail.nostrSaveRestart') : t('agentDetail.nostrEnable')}
          </button>
          {enabled && (
            <button
              type="button"
              className="btn-outlined"
              disabled={saving}
              onClick={() => void save(false)}
            >
              {t('agentDetail.nostrDisable')}
            </button>
          )}
          {cfg?.configured && (
            <button
              type="button"
              className="btn-text text-error"
              disabled={saving}
              onClick={() => void remove()}
            >
              {t('common.delete')}
            </button>
          )}
        </div>
      </div>
    </div>
  );
}
