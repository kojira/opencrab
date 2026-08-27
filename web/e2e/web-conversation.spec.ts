import { expect, test } from '@playwright/test';

const USER_TEXT = 'hello from browser e2e';
const REPLY = 'e2e-reply-from-mock';

test('plain HTTP: new conversation → send → pending → SSE reply', async ({ page }) => {
  const origin = process.env.E2E_ORIGIN ?? '';
  expect(origin.startsWith('http://'), `origin must be plain HTTP: ${origin}`).toBe(true);

  await page.goto('/sessions');
  expect(page.url().startsWith('http://'), `page URL must stay plain HTTP: ${page.url()}`).toBe(
    true,
  );

  await page.getByLabel('エージェントを選択').selectOption('e2eagent');
  await page.getByRole('button', { name: '新しい会話' }).click();
  await page.getByRole('dialog').getByRole('button', { name: '作成' }).click();

  await page.waitForURL(/\/sessions\/web-e2eagent-/);
  expect(page.url().startsWith('http://')).toBe(true);

  const composer = page.getByPlaceholder('メッセージを入力...');
  await expect(composer).toBeEnabled({ timeout: 70_000 });

  await composer.fill(USER_TEXT);
  await page.getByRole('button', { name: '送信' }).click();

  await expect(page.getByText(USER_TEXT)).toBeVisible();
  await expect(page.locator('[aria-live="polite"]').first()).toBeVisible();

  await expect(page.getByText(REPLY)).toBeVisible({ timeout: 40_000 });
  await expect(page.getByText(REPLY)).toBeInViewport();
  await expect(page.getByTestId('session-log-tail')).toBeInViewport();
  const leftoverPx = await page.getByTestId('session-log-list').evaluate((el) => {
    return el.scrollHeight - el.scrollTop - el.clientHeight;
  });
  expect(leftoverPx, `log tail leftover px=${leftoverPx}`).toBeLessThanOrEqual(80);
});
