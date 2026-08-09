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
