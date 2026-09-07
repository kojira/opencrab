import { useState, useEffect } from 'react';
import { useTranslation } from 'react-i18next';
import { patchAgent } from '../../api/agents';
import { getLlmModelChoices } from '../../api/llm';
import ModelPricingForm from '../../components/ui/ModelPricingForm';
import { useAgentContext } from '../../hooks/useAgentContext';

export function LlmModelSection({ agentId }: { agentId: string }) {
  const { t } = useTranslation();
  const { agent } = useAgentContext();
  const [defaultModel, setDefaultModel] = useState('');
  const [choices, setChoices] = useState<string[]>([]);
  const [selection, setSelection] = useState('');
  const [reasoningEffort, setReasoningEffort] = useState('');
  const [webSearch, setWebSearch] = useState(false);
  const [saving, setSaving] = useState(false);
  const [message, setMessage] = useState<string | null>(null);
  // 保存が「未登録モデル」エラーで弾かれたときに、その場で登録するための spec。
  const [unregisteredSpec, setUnregisteredSpec] = useState<string | null>(null);

  useEffect(() => {
    getLlmModelChoices()
      .then((c) => {
        setDefaultModel(c.default_model);
        setChoices(c.choices);
      })
      .catch(() => {
        setDefaultModel('');
        setChoices([]);
      });
  }, [agentId]);

  useEffect(() => {
    setSelection(agent?.model ?? '');
  }, [agent?.model]);

  useEffect(() => {
    setReasoningEffort(agent?.reasoning_effort ?? '');
  }, [agent?.reasoning_effort]);

  useEffect(() => {
    setWebSearch(agent?.web_search ?? false);
  }, [agent?.web_search]);

  // サーバーが「model_pricing に context_window が無い」ときに返す文言（process.rs:958）。
  const UNREGISTERED_MARKER = 'has no context_window registered in model_pricing';

  // patchAgent を実行し、失敗ならエラー文字列を返す（成功なら null）。
  const runPatch = async (): Promise<string | null> => {
    const res = await patchAgent(agentId, {
      model: selection === '' ? null : selection,
      // 既定選択時は空文字を送る（サーバー側で NULL に正規化）。null は
      // serde の都合で「変更なし」に潰れてクリアできないため。
      reasoning_effort: reasoningEffort,
      web_search: webSearch,
    });
    if (res.updated) return null;
    return res.error ?? t('agentDetail.modelSaveFailed');
  };

  const save = async () => {
    setSaving(true);
    setMessage(null);
    setUnregisteredSpec(null);
    try {
      const err = await runPatch();
      if (!err) {
        setMessage(t('agentDetail.modelSaved'));
      } else if (err.includes(UNREGISTERED_MARKER)) {
        // エラー文から失敗した spec を拾う（無ければ選択中の spec）。その場に
        // 登録フォームを出す導線。ターミナルで curl を叩く必要をなくす。
        const m = err.match(/model "([^"]+)"/);
        setUnregisteredSpec(m ? m[1] : selection);
        setMessage(t('agentDetail.modelUnregisteredHint'));
      } else {
        setMessage(err);
      }
    } catch (e) {
      setMessage(String(e));
    } finally {
      setSaving(false);
    }
  };

  // 登録が済んだら、元々やろうとしていたモデル保存を自動で再試行する。
  // 「登録できたのか / 保存できたのか」を分けて見せる（登録は成功したが保存が
  // 別理由で失敗する経路があるため）。
  const onPricingRegistered = async () => {
    setUnregisteredSpec(null);
    setSaving(true);
    try {
      const err = await runPatch();
      if (!err) {
        setMessage(t('agentDetail.modelRegisteredAndSaved'));
      } else {
        setMessage(t('agentDetail.modelRegisteredButSaveFailed', { error: err }));
      }
    } catch (e) {
      setMessage(t('agentDetail.modelRegisteredButSaveFailed', { error: String(e) }));
    } finally {
      setSaving(false);
    }
  };

  return (
    <div className="card-outlined mt-6">
      <h2 className="section-title flex items-center gap-2">
        <span className="material-symbols-outlined text-xl text-primary">smart_toy</span>
        {t('agentDetail.llmModel')}
      </h2>
      <p className="text-body-sm text-on-surface-variant mb-3">
        {t('agentDetail.llmModelDesc', { default: defaultModel || '—' })}
      </p>
      {message && (
        <p className="text-body-sm mb-2 text-on-surface-variant">{message}</p>
      )}
      <div className="flex flex-col sm:flex-row gap-3 items-stretch sm:items-end">
        <div className="flex-1">
          <label className="text-label-lg text-on-surface-variant block mb-1">
            {t('agentDetail.modelSelect')}
          </label>
          <select
            className="input w-full"
            value={selection}
            onChange={(e) => setSelection(e.target.value)}
          >
            <option value="">{t('agentDetail.useServerDefault')}</option>
            {choices.map((m) => (
              <option key={m} value={m}>
                {m}
              </option>
            ))}
          </select>
        </div>
        <div className="sm:w-48">
          <label className="text-label-lg text-on-surface-variant block mb-1">
            {t('agentDetail.thinkingLevel')}
          </label>
          <select
            className="input w-full"
            value={reasoningEffort}
            onChange={(e) => setReasoningEffort(e.target.value)}
          >
            <option value="">{t('agentDetail.thinkingDefault')}</option>
            <option value="minimal">minimal</option>
            <option value="low">low</option>
            <option value="medium">medium</option>
            <option value="high">high</option>
            <option value="xhigh">xhigh</option>
          </select>
        </div>
        <button
          type="button"
          className="btn-filled"
          disabled={saving}
          onClick={() => void save()}
        >
          {saving ? t('common.saving') : t('common.save')}
        </button>
      </div>
      <label className="mt-3 flex items-start gap-2 cursor-pointer">
        <input
          type="checkbox"
          className="mt-1"
          checked={webSearch}
          onChange={(e) => setWebSearch(e.target.checked)}
        />
        <span>
          <span className="text-label-lg text-on-surface block">
            {t('agentDetail.webSearch')}
          </span>
          <span className="text-body-sm text-on-surface-variant">
            {t('agentDetail.webSearchDesc')}
          </span>
        </span>
      </label>

      {unregisteredSpec && (
        <div className="mt-4">
          <p className="text-label-lg text-on-surface mb-2">
            {t('agentDetail.registerModelTitle', { spec: unregisteredSpec })}
          </p>
          <ModelPricingForm
            initial={splitModelSpec(unregisteredSpec)}
            submitLabel={t('agentDetail.registerAndSave')}
            onSaved={onPricingRegistered}
            onCancel={() => setUnregisteredSpec(null)}
          />
        </div>
      )}
    </div>
  );
}

// "provider:model" 形式の spec を登録フォームの初期値に分解する。
function splitModelSpec(spec: string): { provider?: string; model?: string } {
  const i = spec.indexOf(':');
  if (i < 0) return { model: spec };
  return { provider: spec.slice(0, i), model: spec.slice(i + 1) };
}

