import { useState } from 'react';
import { putModelPricing, ModelPricing } from '../../api/modelPricing';

const inputCls =
  'rounded-lg border border-outline bg-surface px-3 py-2 text-sm text-on-surface focus:outline-none focus:ring-2 focus:ring-primary w-full';
const btnPrimary =
  'rounded-lg bg-primary text-on-primary px-3 py-1.5 text-sm font-medium hover:opacity-90 disabled:opacity-50';
const btnGhost =
  'rounded-lg border border-outline px-3 py-1.5 text-sm text-on-surface-variant hover:bg-surface-variant disabled:opacity-50';

export interface ModelPricingFormInitial {
  provider?: string;
  model?: string;
  input_price_per_1m?: number;
  output_price_per_1m?: number;
  context_window?: number | null;
  /** #676: 出力トークン上限（任意）。max_tokens を送るプロバイダのモデルにだけ必要。 */
  max_output_tokens?: number | null;
}

/**
 * モデル単価・コンテキスト長の追加/編集フォーム。
 *
 * システム設定ページの一覧からの追加/編集にも、モデル保存の未登録エラーからの
 * インライン登録導線（AgentOverview）にも同じものを使う。provider は特定の名前
 * （chatgpt 等）を焼き付けず入力欄にして、他プロバイダーが同じ表に入っても耐える。
 */
export default function ModelPricingForm({
  initial,
  keysReadOnly = false,
  submitLabel = '登録',
  onSaved,
  onCancel,
}: {
  initial?: ModelPricingFormInitial;
  /** 編集時に provider/model を固定して、うっかり別行を作らせない。 */
  keysReadOnly?: boolean;
  submitLabel?: string;
  onSaved: (saved: ModelPricing) => void;
  onCancel?: () => void;
}) {
  const [provider, setProvider] = useState(initial?.provider ?? '');
  const [model, setModel] = useState(initial?.model ?? '');
  const [contextWindow, setContextWindow] = useState(
    initial?.context_window != null ? String(initial.context_window) : '',
  );
  const [inputPrice, setInputPrice] = useState(
    initial?.input_price_per_1m != null ? String(initial.input_price_per_1m) : '',
  );
  const [outputPrice, setOutputPrice] = useState(
    initial?.output_price_per_1m != null ? String(initial.output_price_per_1m) : '',
  );
  const [maxOutputTokens, setMaxOutputTokens] = useState(
    initial?.max_output_tokens != null ? String(initial.max_output_tokens) : '',
  );
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const save = async () => {
    setError(null);
    const p = provider.trim();
    const m = model.trim();
    if (!p || !m) {
      setError('provider と model は必須です');
      return;
    }
    // context_window は文脈予算の唯一の出所。0 や負数は compute_context_budget が
    // 使えない値なので弾く（それ以外の正当値は弾かない）。
    const cw = Number(contextWindow);
    if (!Number.isFinite(cw) || !Number.isInteger(cw) || cw <= 0) {
      setError('context_window は正の整数トークン数で入力してください');
      return;
    }
    const inp = inputPrice.trim() === '' ? 0 : Number(inputPrice);
    const out = outputPrice.trim() === '' ? 0 : Number(outputPrice);
    if (!Number.isFinite(inp) || inp < 0 || !Number.isFinite(out) || out < 0) {
      setError('単価は 0 以上の数値で入力してください');
      return;
    }
    // #676: max_output_tokens は任意。空欄なら null（未登録）。入れるなら正の整数のみ。
    let mot: number | null = null;
    if (maxOutputTokens.trim() !== '') {
      const v = Number(maxOutputTokens);
      if (!Number.isFinite(v) || !Number.isInteger(v) || v <= 0) {
        setError('max_output_tokens は空欄か、正の整数トークン数で入力してください');
        return;
      }
      mot = v;
    }
    setSaving(true);
    try {
      await putModelPricing({
        provider: p,
        model: m,
        input_price_per_1m: inp,
        output_price_per_1m: out,
        context_window: cw,
        max_output_tokens: mot,
      });
      onSaved({
        provider: p,
        model: m,
        input_price_per_1m: inp,
        output_price_per_1m: out,
        context_window: cw,
        max_output_tokens: mot,
      });
    } catch (e) {
      setError(String(e));
    } finally {
      setSaving(false);
    }
  };

  return (
    <div className="space-y-3 rounded-lg border border-outline bg-surface-variant/30 p-3">
      <div className="grid gap-3 sm:grid-cols-2">
        <div>
          <label className="mb-1 block text-xs font-medium text-on-surface-variant">
            provider
          </label>
          <input
            value={provider}
            onChange={(e) => setProvider(e.target.value)}
            readOnly={keysReadOnly}
            placeholder="例: chatgpt / gemini"
            className={`${inputCls} ${keysReadOnly ? 'opacity-70' : ''}`}
          />
        </div>
        <div>
          <label className="mb-1 block text-xs font-medium text-on-surface-variant">
            model
          </label>
          <input
            value={model}
            onChange={(e) => setModel(e.target.value)}
            readOnly={keysReadOnly}
            placeholder="例: gpt-5.6-terra"
            className={`${inputCls} ${keysReadOnly ? 'opacity-70' : ''}`}
          />
        </div>
        <div className="sm:col-span-2">
          <label className="mb-1 block text-xs font-medium text-on-surface-variant">
            context_window（最大コンテキスト長・トークン）
          </label>
          <input
            value={contextWindow}
            onChange={(e) => setContextWindow(e.target.value)}
            inputMode="numeric"
            placeholder="例: 1050000"
            className={inputCls}
          />
          <p className="mt-1 text-xs text-on-surface-variant">
            文脈予算 = context_window × compaction_ratio（既定 0.5）。小さすぎると注入が
            切り詰められます。値は
            <strong>モデル提供元の公式ドキュメント</strong>を見てください（集約サイトの数字は
            当てになりません）。
          </p>
        </div>
        <div className="sm:col-span-2">
          <label className="mb-1 block text-xs font-medium text-on-surface-variant">
            max_output_tokens（出力トークン上限・任意）
          </label>
          <input
            value={maxOutputTokens}
            onChange={(e) => setMaxOutputTokens(e.target.value)}
            inputMode="numeric"
            placeholder="例: 128000（空欄可）"
            className={inputCls}
          />
          <p className="mt-1 text-xs text-on-surface-variant">
            エンジンが各リクエストの出力上限に使います。<strong>max_tokens を送るプロバイダ</strong>
            （openai 形式 / anthropic 等）のモデルでは<strong>必須</strong>——未登録だと使用時に
            エラーで止まります。送らないプロバイダ（chatgpt / codex / cursor / acp）は空欄で構いません。
            値は<strong>モデル提供元の公式ドキュメント</strong>を見てください（集約サイトの数字は
            当てになりません）。
          </p>
        </div>
        <div>
          <label className="mb-1 block text-xs font-medium text-on-surface-variant">
            入力単価（per 1M トークン）
          </label>
          <input
            value={inputPrice}
            onChange={(e) => setInputPrice(e.target.value)}
            inputMode="decimal"
            placeholder="例: 2.0"
            className={inputCls}
          />
        </div>
        <div>
          <label className="mb-1 block text-xs font-medium text-on-surface-variant">
            出力単価（per 1M トークン）
          </label>
          <input
            value={outputPrice}
            onChange={(e) => setOutputPrice(e.target.value)}
            inputMode="decimal"
            placeholder="例: 12.0"
            className={inputCls}
          />
        </div>
      </div>
      <p className="text-xs text-on-surface-variant">
        入力量の帯で単価が変わるモデルがあります（一定量を超えると単価が上がる等）。
        この表は<strong>単一レートしか持てない</strong>ので、どちらの単価を入れるかは
        運用者の判断です。
      </p>
      {error && <p className="text-sm text-red-500">エラー: {error}</p>}
      <div className="flex gap-2">
        <button onClick={save} disabled={saving} className={btnPrimary}>
          {saving ? '保存中...' : submitLabel}
        </button>
        {onCancel && (
          <button onClick={onCancel} disabled={saving} className={btnGhost}>
            キャンセル
          </button>
        )}
      </div>
    </div>
  );
}
