import { expect, test, type Page } from '@playwright/test';

const USER_TEXT = 'hello from browser e2e';
const REPLY = 'e2e-reply-from-mock';
const OVERFLOW_SEED = 40;
const TAIL_SLOP_PX = 1;

async function leftoverPx(page: Page): Promise<number> {
  return page.getByTestId('session-log-list').evaluate((el) => {
    return el.scrollHeight - el.scrollTop - el.clientHeight;
  });
}

async function logMetrics(page: Page) {
  return page.getByTestId('session-log-list').evaluate((el) => ({
    scrollTop: el.scrollTop,
    clientHeight: el.clientHeight,
    scrollHeight: el.scrollHeight,
    leftover: el.scrollHeight - el.scrollTop - el.clientHeight,
  }));
}

async function assertPlainNonSecure(page: Page) {
  const origin = process.env.E2E_ORIGIN ?? '';
  expect(origin.startsWith('http://'), `origin must be plain HTTP: ${origin}`).toBe(true);
  expect(origin.includes('127.0.0.1') || origin.includes('localhost'), origin).toBe(false);

  await page.goto('/sessions');
  expect(page.url().startsWith('http://'), `page URL must stay plain HTTP: ${page.url()}`).toBe(
    true,
  );

  const isSecureContext = await page.evaluate(() => window.isSecureContext);
  console.log(`isSecureContext=${isSecureContext} url=${page.url()}`);
  expect(isSecureContext, 'Chromium must treat the e2e hostname as a non-secure context').toBe(
    false,
  );
}

async function sendAndSeePending(page: Page, text: string) {
  const composer = page.getByPlaceholder('メッセージを入力...');
  await expect(composer).toBeEnabled({ timeout: 70_000 });
  const sendRespP = page.waitForResponse(
    (res) => res.request().method() === 'POST' && res.url().includes('/messages'),
  );
  await composer.fill(text);
  await page.getByRole('button', { name: '送信' }).click();
  await expect(page.getByText(text)).toBeVisible();
  await expect(page.getByTestId('session-pending-spinner')).toBeVisible();
  const sendResp = await sendRespP;
  const sendBody = await sendResp.text();
  console.log(`sendStatus=${sendResp.status()} sendBody=${sendBody}`);
  expect(sendResp.status(), sendBody).toBe(202);
}

test('plain HTTP non-secure: new conversation button → send → pending spinner → SSE reply', async ({
  page,
}) => {
  await assertPlainNonSecure(page);

  await page.getByLabel('エージェントを選択').selectOption('e2eagent');
  await page.getByRole('button', { name: '新しい会話' }).click();
  await page.getByRole('dialog').getByRole('button', { name: '作成' }).click();
  await page.waitForURL(/\/sessions\/web-e2eagent-/);
  expect(page.url().startsWith('http://')).toBe(true);

  await sendAndSeePending(page, USER_TEXT);
  await expect(page.getByText(REPLY)).toBeVisible({ timeout: 40_000 });
});

test('plain HTTP non-secure: physical id send + overflow scroll follow/hold', async ({ page }) => {
  await assertPlainNonSecure(page);

  const created = (await page.evaluate(async () => {
    const res = await fetch('/api/agents/e2eagent/web-conversations', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: '{}',
    });
    return { status: res.status, body: await res.json() };
  })) as {
    status: number;
    body: { binding_id?: string; session_id?: string };
  };
  expect(created.status, JSON.stringify(created)).toBe(201);
  expect(created.body.binding_id, JSON.stringify(created.body)).toMatch(
    /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/,
  );
  expect(created.body.session_id, JSON.stringify(created.body)).toMatch(/^web-e2eagent-/);
  const physicalId = `extgate-${created.body.binding_id}`;
  const logicalId = created.body.session_id as string;
  console.log(`logical=${logicalId} physical=${physicalId}`);

  const seed = await page.evaluate(
    async ({ sessionId, count }) => {
      const res = await fetch('/__e2e/seed-logs', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ session_id: sessionId, count }),
      });
      return { status: res.status, body: await res.text() };
    },
    { sessionId: physicalId, count: OVERFLOW_SEED },
  );
  expect(seed.status, seed.body).toBe(200);

  await page.goto(`/sessions/${physicalId}`);
  expect(page.url()).toContain(physicalId);

  await expect(page.getByText('overflow-seed-000')).toBeVisible();
  await expect(page.getByText(`overflow-seed-${String(OVERFLOW_SEED - 1).padStart(3, '0')}`)).toBeVisible();

  const overflow = await logMetrics(page);
  console.log(`overflowMetrics=${JSON.stringify(overflow)}`);
  expect(overflow.scrollHeight, JSON.stringify(overflow)).toBeGreaterThan(overflow.clientHeight);

  await sendAndSeePending(page, USER_TEXT);
  await expect(page.getByText(REPLY)).toBeVisible({ timeout: 40_000 });
  await expect.poll(async () => leftoverPx(page), { timeout: 5_000 }).toBeLessThanOrEqual(TAIL_SLOP_PX);
  const tailAfterSend = await leftoverPx(page);
  console.log(`leftoverAfterSendPx=${tailAfterSend}`);
  expect(tailAfterSend, `new arrival must reach the tail; leftoverPx=${tailAfterSend}`).toBeLessThanOrEqual(
    TAIL_SLOP_PX,
  );
  const countAfterFollow = await page.getByTestId('session-log-list').locator('p').count();

  await page.getByTestId('session-log-list').evaluate((el) => {
    el.scrollTo({ top: 0, behavior: 'instant' });
  });
  await expect.poll(async () => leftoverPx(page)).toBeGreaterThan(80);
  const leftoverAfterUp = await leftoverPx(page);
  console.log(`leftoverAfterUpPx=${leftoverAfterUp}`);

  const posted = await page.evaluate(
    async ({ sessionId, text }) => {
      const res = await fetch(`/api/web-conversations/${sessionId}/messages`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          client_message_id: 'dddddddd-dddd-4ddd-8ddd-dddddddddddd',
          text,
          attachments: [],
        }),
      });
      return { status: res.status, body: await res.text() };
    },
    { sessionId: logicalId, text: 'second-from-api' },
  );
  expect(posted.status, posted.body).toBe(202);

  await expect
    .poll(async () => page.getByTestId('session-log-list').locator('p').count(), { timeout: 40_000 })
    .toBeGreaterThan(countAfterFollow);
  const leftoverHeld = await leftoverPx(page);
  console.log(`leftoverHeldPx=${leftoverHeld}`);
  expect(leftoverHeld, `must not follow while reading upward; leftoverPx=${leftoverHeld}`).toBeGreaterThan(80);
});
