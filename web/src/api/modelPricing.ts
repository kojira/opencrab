import { api } from './client';

// ============ モデル単価・コンテキスト長 (model_pricing) ============
//
// 文脈予算（context_window × compaction_ratio）の唯一の出所。サーバ側 API は
// GET/PUT /api/llm/model-pricing（crates/server/src/api/model_pricing.rs）に既存で、
// ここはそのフロント用クライアント。

export interface ModelPricing {
  provider: string;
  model: string;
  input_price_per_1m: number;
  output_price_per_1m: number;
  /** そのモデルの最大コンテキスト長（トークン）。未登録行では null。 */
  context_window: number | null;
  /**
   * #676: そのモデルの出力トークン上限（実能力値・トークン）。未登録行では null。
   * max_tokens を送るプロバイダ（openai形式/anthropic 等）のモデルにだけ必要で、
   * 送らないプロバイダ（chatgpt/codex/cursor/acp）では null のままで良い（任意）。
   */
  max_output_tokens: number | null;
}

export interface ModelPricingListResponse {
  models: ModelPricing[];
  /**
   * server-global の compaction_ratio（context_window のうち会話履歴に使う割合）。
   * 実効予算 = context_window × compaction_ratio。旧サーバはこのフィールドを
   * 返さないため undefined になりうる（その場合 UI は実効予算を出さない）。
   */
  compaction_ratio?: number;
}

/**
 * PUT ボディ。context_window は必須・正の整数（サーバ側で 0 以下は 400）。
 * #676: max_output_tokens は任意（nullable）。指定するなら正の整数（サーバ側で 0 以下は 400）。
 */
export interface PutModelPricingBody {
  provider: string;
  model: string;
  input_price_per_1m: number;
  output_price_per_1m: number;
  context_window: number;
  max_output_tokens?: number | null;
}

export function listModelPricing(): Promise<ModelPricingListResponse> {
  return api.get<ModelPricingListResponse>('/llm/model-pricing');
}

export function putModelPricing(body: PutModelPricingBody): Promise<unknown> {
  return api.put<unknown>('/llm/model-pricing', body);
}
