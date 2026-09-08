import { defineConfig, devices } from '@playwright/test';

const origin = process.env.E2E_ORIGIN ?? '';
const host = process.env.E2E_HOST ?? 'qc-e2e.test';

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
    launchOptions: {
      args: [`--host-resolver-rules=MAP ${host} 127.0.0.1`],
    },
  },
  projects: [{ name: 'chromium', use: { ...devices['Desktop Chrome'] } }],
});
