/** DESIGN-WEBGATE §7.4a: ハーネスを立てて Playwright を 1 本走らせる。 */
import { spawn } from 'node:child_process';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { startHarness } from './harness.mjs';

const WEB_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');

const harness = await startHarness();
process.env.E2E_ORIGIN = harness.origin;

const child = spawn('npx', ['playwright', 'test'], {
  cwd: WEB_ROOT,
  stdio: 'inherit',
  env: { ...process.env, E2E_ORIGIN: harness.origin },
});

const shutdown = async (code) => {
  await harness.stop();
  process.exit(code ?? 1);
};

child.on('exit', (code) => {
  void shutdown(code);
});
child.on('error', (err) => {
  console.error(err);
  void shutdown(1);
});

process.on('SIGINT', () => {
  child.kill('SIGINT');
});
process.on('SIGTERM', () => {
  child.kill('SIGTERM');
});
