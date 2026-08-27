import { defineConfig, devices } from '@playwright/test';

const origin = process.env.E2E_ORIGIN ?? '';

export default defineConfig({
  testDir: './e2e',
  testMatch: '**/*.spec.ts',
  timeout: 120_000,
  expect: { timeout: 15_000 },
  fullyParallel: false,
  workers: 1,
  retries: 0,
  reporter: 'list',
  use: {
    baseURL: origin,
    locale: 'ja-JP',
    timezoneId: 'Asia/Tokyo',
    headless: true,
    ignoreHTTPSErrors: false,
    trace: 'off',
  },
  projects: [{ name: 'chromium', use: { ...devices['Desktop Chrome'] } }],
});
