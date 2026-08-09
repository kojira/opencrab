import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, screen, waitFor, fireEvent } from '@testing-library/react';

// エラー → インライン登録導線（#482 本体）が、サーバの未登録エラー文言に結合している
// ことを固定する。文言（marker）や spec 抽出の正規表現が変わると、この結合が黙って
// 壊れ、運用者はまた curl に戻る。ここでその結合を守る。

vi.mock('../api/agents', () => ({
  patchAgent: vi.fn(),
}));
vi.mock('../api/llm', () => ({
  getLlmModelChoices: vi.fn(),
}));
vi.mock('../api/modelPricing', () => ({
  putModelPricing: vi.fn(),
  listModelPricing: vi.fn(),
}));
// LlmModelSection は useAgentContext(useOutletContext) 経由で agent を読む。
// Outlet 無しで描画できるよう固定値に差し替える。
vi.mock('../hooks/useAgentContext', () => ({
  useAgentContext: () => ({
    agent: { model: 'chatgpt:gpt-5.6-terra', reasoning_effort: '', web_search: false },
    agentId: 'a1',
  }),
}));

import { patchAgent } from '../api/agents';
import { getLlmModelChoices } from '../api/llm';
import { LlmModelSection } from './AgentOverview';

const mockedPatch = vi.mocked(patchAgent);
const mockedChoices = vi.mocked(getLlmModelChoices);

// crates/server/src/process.rs `model_context_window_missing_message` の実文言を再現。
// 導線の検出はこの文字列（marker + `model "..."` の引用）に依存している。
const SPEC = 'chatgpt:gpt-5.6-terra';
const REAL_UNREGISTERED_MSG =
  `model "${SPEC}" has no context_window registered in model_pricing. ` +
  `Register it first: PUT /api/llm/model-pricing with body ` +
  `{"provider": "...", "model": "...", "input_price_per_1m": 0.0, ` +
  `"output_price_per_1m": 0.0, "context_window": <max tokens>}. ` +
  `Current registrations: GET /api/llm/model-pricing.`;

beforeEach(() => {
  mockedPatch.mockReset();
  mockedChoices.mockReset();
  mockedChoices.mockResolvedValue({ default_model: 'chatgpt:gpt-5.6-terra', choices: [SPEC] });
});

describe('LlmModelSection unregistered-model inline registration link (#482)', () => {
  it('opens a prefilled registration form when save fails with the unregistered error', async () => {
    mockedPatch.mockResolvedValue({ updated: false, error: REAL_UNREGISTERED_MSG });

    render(<LlmModelSection agentId="a1" />);
    await waitFor(() => expect(mockedChoices).toHaveBeenCalled());

    fireEvent.click(screen.getByRole('button', { name: 'common.save' }));
    await waitFor(() => expect(mockedPatch).toHaveBeenCalled());

    // インライン登録フォームが出る（導線の見出し）。
    await waitFor(() =>
      expect(screen.getByText('agentDetail.registerModelTitle')).toBeInTheDocument(),
    );
    // provider / model がエラー文の spec から prefill されている（手写し不要）。
    expect(screen.getByPlaceholderText('例: chatgpt / gemini')).toHaveValue('chatgpt');
    expect(screen.getByPlaceholderText('例: gpt-5.6-terra')).toHaveValue('gpt-5.6-terra');
  });

  it('does not open the form for an unrelated save error', async () => {
    mockedPatch.mockResolvedValue({ updated: false, error: 'some other failure' });

    render(<LlmModelSection agentId="a1" />);
    await waitFor(() => expect(mockedChoices).toHaveBeenCalled());

    fireEvent.click(screen.getByRole('button', { name: 'common.save' }));
    await waitFor(() => expect(mockedPatch).toHaveBeenCalled());

    // 未登録以外のエラーでは導線を出さず、素のエラーを表示する。
    expect(screen.getByText('some other failure')).toBeInTheDocument();
    expect(screen.queryByText('agentDetail.registerModelTitle')).not.toBeInTheDocument();
  });
});
