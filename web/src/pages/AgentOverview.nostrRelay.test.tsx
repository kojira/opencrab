import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, screen, waitFor, fireEvent } from '@testing-library/react';

// nostr API をスパイに差し替える（呼び出し引数を検証する）。
vi.mock('../api/nostr', () => ({
  getNostrRelayConfig: vi.fn(),
  updateNostrRelayConfig: vi.fn(),
}));

import { getNostrRelayConfig, updateNostrRelayConfig } from '../api/nostr';
import { NostrRelaySection } from './AgentOverview';

const mockedGet = vi.mocked(getNostrRelayConfig);
const mockedUpdate = vi.mocked(updateNostrRelayConfig);

// setup.ts の react-i18next モックで t(key) はキー文字列を返す。ボタン名 = 翻訳キー。
const MASKED = 'https://discord.com/api/webhooks/1/[redacted]';

beforeEach(() => {
  mockedGet.mockReset();
  mockedUpdate.mockReset();
});

describe('NostrRelaySection webhook_url three-state (issue #252 段階 B)', () => {
  it('入力欄が空の保存では webhook_url を送らない（既存転記先を保持）', async () => {
    mockedGet.mockResolvedValue({
      configured: true,
      enabled: true,
      has_webhook: true,
      webhook_url_masked: MASKED,
    });
    mockedUpdate.mockResolvedValue({
      updated: true,
      enabled: false,
      has_webhook: true,
      webhook_url_masked: MASKED,
    });

    render(<NostrRelaySection agentId="a1" />);
    await waitFor(() => expect(mockedGet).toHaveBeenCalled());

    // enabled チェックだけ切り替える（入力欄は空のまま）。
    fireEvent.click(screen.getByRole('checkbox'));
    fireEvent.click(screen.getByRole('button', { name: 'common.save' }));

    await waitFor(() => expect(mockedUpdate).toHaveBeenCalled());
    const [agentId, body] = mockedUpdate.mock.calls[0];
    expect(agentId).toBe('a1');
    // ブロッカー回帰: 空欄保存で webhook_url を送らない → バックエンドで保持される。
    expect(body).not.toHaveProperty('webhook_url');
    expect(body.enabled).toBe(false);
  });

  it('入力欄に URL を入れて保存すると webhook_url を送る（設定）', async () => {
    mockedGet.mockResolvedValue({
      configured: false,
      enabled: false,
      has_webhook: false,
      webhook_url_masked: '',
    });
    mockedUpdate.mockResolvedValue({
      updated: true,
      enabled: true,
      has_webhook: true,
      webhook_url_masked: MASKED,
    });

    render(<NostrRelaySection agentId="a1" />);
    await waitFor(() => expect(mockedGet).toHaveBeenCalled());

    const url = 'https://discord.com/api/webhooks/123/tok';
    fireEvent.change(screen.getByRole('textbox'), { target: { value: url } });
    fireEvent.click(screen.getByRole('button', { name: 'common.save' }));

    await waitFor(() => expect(mockedUpdate).toHaveBeenCalled());
    const [, body] = mockedUpdate.mock.calls[0];
    expect(body.webhook_url).toBe(url);
  });

  it('「転記先を削除」は null を送って消去する（明示操作）', async () => {
    mockedGet.mockResolvedValue({
      configured: true,
      enabled: true,
      has_webhook: true,
      webhook_url_masked: MASKED,
    });
    mockedUpdate.mockResolvedValue({
      updated: true,
      enabled: true,
      has_webhook: false,
      webhook_url_masked: '',
    });
    const confirmSpy = vi.spyOn(window, 'confirm').mockReturnValue(true);

    render(<NostrRelaySection agentId="a1" />);
    await waitFor(() => expect(mockedGet).toHaveBeenCalled());

    fireEvent.click(screen.getByRole('button', { name: 'agentDetail.nostrRelayDelete' }));

    await waitFor(() => expect(mockedUpdate).toHaveBeenCalled());
    const [, body] = mockedUpdate.mock.calls[0];
    expect(body.webhook_url).toBeNull();
    confirmSpy.mockRestore();
  });

  it('応答の warning を表示する', async () => {
    mockedGet.mockResolvedValue({
      configured: true,
      enabled: false,
      has_webhook: false,
      webhook_url_masked: '',
    });
    mockedUpdate.mockResolvedValue({
      updated: true,
      enabled: true,
      has_webhook: false,
      webhook_url_masked: '',
      warning: '有効化されていますが転記先(webhook_url)が未設定のため、現在は転記されません',
    });

    render(<NostrRelaySection agentId="a1" />);
    await waitFor(() => expect(mockedGet).toHaveBeenCalled());

    fireEvent.click(screen.getByRole('checkbox'));
    fireEvent.click(screen.getByRole('button', { name: 'common.save' }));

    await waitFor(() =>
      expect(
        screen.getByText(
          '有効化されていますが転記先(webhook_url)が未設定のため、現在は転記されません',
        ),
      ).toBeInTheDocument(),
    );
  });
});
