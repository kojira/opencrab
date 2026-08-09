import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, screen, waitFor } from '@testing-library/react';

vi.mock('../api/modelPricing', () => ({
  listModelPricing: vi.fn(),
  putModelPricing: vi.fn(),
}));

import { listModelPricing } from '../api/modelPricing';
import { ModelPricingSection } from './SystemSettings';

const mockedList = vi.mocked(listModelPricing);

beforeEach(() => {
  mockedList.mockReset();
});

describe('ModelPricingSection list', () => {
  it('renders registered rows with formatted context_window', async () => {
    mockedList.mockResolvedValue({
      models: [
        {
          provider: 'chatgpt',
          model: 'gpt-5.6-sol',
          input_price_per_1m: 5,
          output_price_per_1m: 30,
          context_window: 1050000,
        },
      ],
    });
    render(<ModelPricingSection />);

    await waitFor(() => {
      expect(screen.getByText('gpt-5.6-sol')).toBeInTheDocument();
    });
    expect(screen.getByText('chatgpt')).toBeInTheDocument();
    // context_window は桁区切りで表示される（実効予算の異常に気づきやすくするため）
    expect(screen.getByText('1,050,000')).toBeInTheDocument();
  });

  it('shows an empty state when nothing is registered', async () => {
    mockedList.mockResolvedValue({ models: [] });
    render(<ModelPricingSection />);

    await waitFor(() => {
      expect(screen.getByText(/登録済みのモデルはありません/)).toBeInTheDocument();
    });
  });

  it('shows the effective budget (context_window × compaction_ratio) per row', async () => {
    // 掛け算せず並べて眺めるだけで「400,000 の行だけ実効予算が小さい」に
    // 気づけるのが狙い（#484）。ratio=0.5 → 525,000 / 200,000。
    mockedList.mockResolvedValue({
      models: [
        {
          provider: 'chatgpt',
          model: 'gpt-5.6-sol',
          input_price_per_1m: 5,
          output_price_per_1m: 30,
          context_window: 1050000,
        },
        {
          provider: 'chatgpt',
          model: 'gpt-5.6-luna',
          input_price_per_1m: 5,
          output_price_per_1m: 30,
          context_window: 400000,
        },
      ],
      compaction_ratio: 0.5,
    });
    render(<ModelPricingSection />);

    await waitFor(() => {
      expect(screen.getByText('gpt-5.6-sol')).toBeInTheDocument();
    });
    // 実効予算列は桁区切りで表示される
    expect(screen.getByText('525,000')).toBeInTheDocument();
    expect(screen.getByText('200,000')).toBeInTheDocument();
  });

  it('omits the effective budget when the server does not report compaction_ratio', async () => {
    // 旧サーバは compaction_ratio を返さない。0.5 を勝手に補って偽の予算を
    // 出すと異常検知の意味が壊れるので、'—' にする。
    mockedList.mockResolvedValue({
      models: [
        {
          provider: 'chatgpt',
          model: 'gpt-5.6-sol',
          input_price_per_1m: 5,
          output_price_per_1m: 30,
          context_window: 1050000,
        },
      ],
    });
    render(<ModelPricingSection />);

    await waitFor(() => {
      expect(screen.getByText('1,050,000')).toBeInTheDocument();
    });
    // 実効予算は算出されない（525,000 を捏造しない）
    expect(screen.queryByText('525,000')).not.toBeInTheDocument();
    expect(screen.getByText('—')).toBeInTheDocument();
  });

  it('flags rows whose context_window is missing', async () => {
    mockedList.mockResolvedValue({
      models: [
        {
          provider: 'chatgpt',
          model: 'legacy',
          input_price_per_1m: 0,
          output_price_per_1m: 0,
          context_window: null,
        },
      ],
    });
    render(<ModelPricingSection />);

    await waitFor(() => {
      expect(screen.getByText('未登録')).toBeInTheDocument();
    });
  });
});
