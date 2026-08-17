import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';

vi.mock('../../api/modelPricing', () => ({
  putModelPricing: vi.fn(),
}));

import { putModelPricing } from '../../api/modelPricing';
import ModelPricingForm from './ModelPricingForm';

const mockedPut = vi.mocked(putModelPricing);

beforeEach(() => {
  mockedPut.mockReset();
  mockedPut.mockResolvedValue({});
});

describe('ModelPricingForm', () => {
  it('saves parsed values via putModelPricing and reports back', async () => {
    const user = userEvent.setup();
    const onSaved = vi.fn();
    render(<ModelPricingForm onSaved={onSaved} />);

    await user.type(screen.getByPlaceholderText('例: chatgpt / gemini'), 'chatgpt');
    await user.type(screen.getByPlaceholderText('例: gpt-5.6-terra'), 'gpt-5.6-terra');
    await user.type(screen.getByPlaceholderText('例: 1050000'), '1050000');
    await user.type(screen.getByPlaceholderText('例: 2.0'), '2');
    await user.type(screen.getByPlaceholderText('例: 12.0'), '12');
    await user.click(screen.getByText('登録'));

    await waitFor(() => {
      expect(mockedPut).toHaveBeenCalledTimes(1);
    });
    // context_window は数値に、単価も数値に変換して送る。max_output_tokens は
    // 空欄なので null（#676: 送らないプロバイダのモデルは空欄で良い）。
    expect(mockedPut).toHaveBeenCalledWith({
      provider: 'chatgpt',
      model: 'gpt-5.6-terra',
      input_price_per_1m: 2,
      output_price_per_1m: 12,
      context_window: 1050000,
      max_output_tokens: null,
    });
    await waitFor(() => {
      expect(onSaved).toHaveBeenCalledWith(
        expect.objectContaining({ provider: 'chatgpt', model: 'gpt-5.6-terra', context_window: 1050000 }),
      );
    });
  });

  it('prefills initial values so editing does not require retyping', () => {
    render(
      <ModelPricingForm
        initial={{ provider: 'chatgpt', model: 'gpt-5.6-sol', context_window: 1050000, input_price_per_1m: 5, output_price_per_1m: 30 }}
        keysReadOnly
        onSaved={vi.fn()}
      />,
    );
    expect(screen.getByPlaceholderText('例: 1050000')).toHaveValue('1050000');
    expect(screen.getByDisplayValue('gpt-5.6-sol')).toBeInTheDocument();
  });

  it('rejects context_window <= 0 without calling the API', async () => {
    const user = userEvent.setup();
    render(<ModelPricingForm onSaved={vi.fn()} />);

    await user.type(screen.getByPlaceholderText('例: chatgpt / gemini'), 'chatgpt');
    await user.type(screen.getByPlaceholderText('例: gpt-5.6-terra'), 'gpt-5.6-terra');
    await user.type(screen.getByPlaceholderText('例: 1050000'), '0');
    await user.click(screen.getByText('登録'));

    expect(await screen.findByText(/context_window は正の整数/)).toBeInTheDocument();
    expect(mockedPut).not.toHaveBeenCalled();
  });
});
