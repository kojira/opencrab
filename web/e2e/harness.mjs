/** DESIGN-WEBGATE §7.4a: 実 core + 実 UDS + 実 gateway + ビルド済み UI を平文 HTTP で立てる。 */
import { spawn } from 'node:child_process';
import {
  createReadStream,
  createWriteStream,
  existsSync,
  mkdirSync,
  rmSync,
  statSync,
  writeFileSync,
} from 'node:fs';
import http from 'node:http';
import { createConnection } from 'node:net';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const WEB_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const REPO_ROOT = path.resolve(WEB_ROOT, '..');

export const AGENT = 'e2eagent';
export const AUTHOR = 'e2e-owner';
export const INSTANCE = '11111111-1111-4111-8111-111111111111';
export const TOKEN = 'e2e-operator-token';
export const REPLY = 'e2e-reply-from-mock';
export const CONFIG_B64 = 'eyJhdXRob3JfaWQiOiJlMmUtb3duZXIifQ==';

function targetDir() {
  return process.env.CARGO_TARGET_DIR || path.join(REPO_ROOT, 'target');
}

function binPath(name) {
  return path.join(targetDir(), 'debug', name);
}

function run(cmd, args, cwd) {
  return new Promise((resolve, reject) => {
    const child = spawn(cmd, args, { cwd, stdio: 'inherit', env: process.env });
    child.on('error', reject);
    child.on('exit', (code) => {
      if (code === 0) resolve();
      else reject(new Error(`${cmd} ${args.join(' ')} exited ${code}`));
    });
  });
}

function listen() {
  return new Promise((resolve, reject) => {
    const server = http.createServer();
    server.listen(0, '127.0.0.1', () => {
      const addr = server.address();
      const port = typeof addr === 'object' && addr ? addr.port : 0;
      server.close((err) => {
        if (err) reject(err);
        else resolve(port);
      });
    });
    server.on('error', reject);
  });
}

function waitTcp(port, timeoutMs) {
  const start = Date.now();
  return new Promise((resolve, reject) => {
    const attempt = () => {
      const sock = createConnection({ host: '127.0.0.1', port }, () => {
        sock.end();
        resolve();
      });
      sock.on('error', () => {
        sock.destroy();
        if (Date.now() - start > timeoutMs) {
          reject(new Error(`127.0.0.1:${port} did not accept within ${timeoutMs}ms`));
        } else {
          setTimeout(attempt, 50);
        }
      });
    };
    attempt();
  });
}

function httpJson(port, method, pathname, { auth, body, timeoutMs } = {}) {
  const payload = body ?? '';
  return new Promise((resolve, reject) => {
    const req = http.request(
      {
        host: '127.0.0.1',
        port,
        method,
        path: pathname,
        headers: {
          'Content-Length': Buffer.byteLength(payload),
          ...(payload ? { 'Content-Type': 'application/json' } : {}),
          ...(auth ? { Authorization: `Bearer ${auth}` } : {}),
        },
      },
      (res) => {
        const chunks = [];
        res.on('data', (c) => chunks.push(c));
        res.on('end', () => {
          resolve({ status: res.statusCode ?? 0, body: Buffer.concat(chunks).toString('utf8') });
        });
      },
    );
    req.on('error', reject);
    req.setTimeout(timeoutMs ?? 10_000, () => {
      req.destroy(new Error(`HTTP ${method} ${pathname} timed out`));
    });
    if (payload) req.write(payload);
    req.end();
  });
}

async function waitHttp(port, pathname, timeoutMs) {
  const start = Date.now();
  while (Date.now() - start < timeoutMs) {
    try {
      const r = await httpJson(port, 'GET', pathname, { timeoutMs: 1000 });
      if (r.status > 0) return;
    } catch {
      /* retry */
    }
    await new Promise((r) => setTimeout(r, 50));
  }
  throw new Error(`HTTP ${pathname} on :${port} did not start`);
}

function spawnMockLlm(port) {
  const server = http.createServer((req, res) => {
    const chunks = [];
    req.on('data', (c) => chunks.push(c));
    req.on('end', () => {
      const url = req.url ?? '';
      if (url.includes('chat/completions')) {
        const body = JSON.stringify({
          id: 'e2e',
          choices: [
            {
              index: 0,
              message: { role: 'assistant', content: REPLY },
              finish_reason: 'stop',
            },
          ],
        });
        setTimeout(() => {
          res.writeHead(200, {
            'Content-Type': 'application/json',
            'Content-Length': Buffer.byteLength(body),
            Connection: 'close',
          });
          res.end(body);
        }, 1500);
        return;
      }
      const body = JSON.stringify({ data: [] });
      res.writeHead(200, {
        'Content-Type': 'application/json',
        'Content-Length': Buffer.byteLength(body),
        Connection: 'close',
      });
      res.end(body);
    });
  });
  return new Promise((resolve) => {
    server.listen(port, '127.0.0.1', () => resolve(server));
  });
}

function writeServerConfig(root, db, sock, httpPort, llmPort) {
  const cfgDir = path.join(root, 'config');
  mkdirSync(cfgDir, { recursive: true });
  writeFileSync(
    path.join(cfgDir, 'default.toml'),
    `
[agent]
heartbeat_interval_secs = 1800
heartbeat_enabled = false
workspace_path = "data/agents/{agent_id}/workspace"
max_workspace_size_mb = 100

[subtask]
auto_dispatch = false

[llm]
default_provider = "openai"
default_model = "e2e-mock"

[llm.self_selection]
enabled = false
allowed_aliases = []

[llm.fallback]
chain = []

[llm.providers.openai]
api_key = "dummy"
base_url = "http://127.0.0.1:${llmPort}/v1"
organization = ""

[gateway.rest]
enabled = true
port = ${httpPort}

[gateway.discord]
enabled = false
token = ""
guild_ids = []
agent_ids = []
owner_discord_id = ""

[dashboard]
enabled = false
port = 3000

[database]
path = "${db}"

[gate]
listen_socket = "${sock}"

[tools]
enabled = false

[llm_log_archive]
enabled = false
`,
  );
}

function mime(filePath) {
  switch (path.extname(filePath)) {
    case '.html':
      return 'text/html; charset=utf-8';
    case '.js':
      return 'text/javascript; charset=utf-8';
    case '.css':
      return 'text/css; charset=utf-8';
    case '.json':
      return 'application/json';
    case '.svg':
      return 'image/svg+xml';
    case '.ico':
      return 'image/x-icon';
    default:
      return 'application/octet-stream';
  }
}

function proxyTo(req, res, port) {
  const up = http.request(
    {
      host: '127.0.0.1',
      port,
      method: req.method,
      path: req.url,
      headers: { ...req.headers, host: `127.0.0.1:${port}` },
    },
    (incoming) => {
      res.writeHead(incoming.statusCode ?? 502, incoming.headers);
      incoming.pipe(res);
    },
  );
  up.on('error', (err) => {
    if (!res.headersSent) res.writeHead(502, { 'Content-Type': 'text/plain' });
    res.end(`proxy: ${err.message}`);
  });
  req.pipe(up);
}

function startOrigin({ uiPort, corePort, gwPort, distDir }) {
  const server = http.createServer((req, res) => {
    const url = req.url ?? '/';
    if (url.startsWith('/api/web-conversations')) {
      proxyTo(req, res, gwPort);
      return;
    }
    if (url.startsWith('/api') || url === '/health') {
      proxyTo(req, res, corePort);
      return;
    }
    const pathname = decodeURIComponent(url.split('?')[0] ?? '/');
    const rel = pathname === '/' ? 'index.html' : pathname.replace(/^\/+/, '');
    const abs = path.resolve(distDir, rel);
    if (abs.startsWith(distDir + path.sep) && existsSync(abs) && statSync(abs).isFile()) {
      res.writeHead(200, { 'Content-Type': mime(abs) });
      createReadStream(abs).pipe(res);
      return;
    }
    const index = path.join(distDir, 'index.html');
    res.writeHead(200, { 'Content-Type': 'text/html; charset=utf-8' });
    createReadStream(index).pipe(res);
  });
  return new Promise((resolve) => {
    server.listen(uiPort, '127.0.0.1', () => resolve(server));
  });
}

function spawnLogged(bin, args, cwd, env, logPath) {
  const out = createWriteStream(logPath, { flags: 'a' });
  const child = spawn(bin, args, {
    cwd,
    env,
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  child.stdout.pipe(out);
  child.stderr.pipe(out);
  return child;
}

async function ensureBins() {
  const server = binPath('opencrab-server');
  const gw = binPath('web-gateway');
  if (!existsSync(server)) {
    await run('cargo', ['build', '-p', 'opencrab-server', '--bin', 'opencrab-server'], REPO_ROOT);
  }
  if (!existsSync(gw)) {
    await run(
      'cargo',
      ['build', '-p', 'opencrab-web-gateway', '--bin', 'web-gateway'],
      REPO_ROOT,
    );
  }
  if (!existsSync(server) || !existsSync(gw)) {
    throw new Error(`missing binaries: ${server} / ${gw}`);
  }
}

async function ensureUiBuild() {
  const dist = path.join(WEB_ROOT, 'dist', 'index.html');
  if (!existsSync(dist)) {
    await run('npm', ['run', 'build'], WEB_ROOT);
  }
  if (!existsSync(dist)) {
    throw new Error(`vite dist missing at ${dist}`);
  }
}

export async function startHarness() {
  await ensureBins();
  await ensureUiBuild();

  const llmPort = await listen();
  const corePort = await listen();
  const gwPort = await listen();
  const uiPort = await listen();

  const root = path.join(tmpdir(), `oc-pw-${process.pid}`);
  rmSync(root, { recursive: true, force: true });
  mkdirSync(root, { recursive: true });
  const db = path.join(root, 'e2e.db');
  const sock = `/tmp/wg-pw-${process.pid}.sock`;
  rmSync(sock, { force: true });
  writeServerConfig(root, db, sock, corePort, llmPort);

  const mock = await spawnMockLlm(llmPort);
  const children = [];
  const servers = [mock];

  const coreEnv = {
    ...process.env,
    OPENCRAB_GATE_OPERATOR_TOKEN: TOKEN,
    RUST_LOG: 'opencrab=info,opencrab_server=info,opencrab_extgate=info',
  };
  delete coreEnv.OPENCRAB_SECRET_MASTER_KEY;
  const core = spawnLogged(
    binPath('opencrab-server'),
    [],
    root,
    coreEnv,
    path.join(root, 'core.log'),
  );
  children.push(core);
  await waitHttp(corePort, '/health', 30_000);

  const created = await httpJson(corePort, 'POST', '/api/agents', {
    body: JSON.stringify({ id: AGENT, name: 'e2e', persona_name: 'e2e' }),
  });
  if (created.status !== 200) {
    throw new Error(`create agent ${created.status} ${created.body}`);
  }

  const pricing = await httpJson(corePort, 'PUT', '/api/llm/model-pricing', {
    body: JSON.stringify({
      provider: 'openai',
      model: 'e2e-mock',
      context_window: 8192,
      max_output_tokens: 1024,
    }),
  });
  if (pricing.status !== 200) {
    throw new Error(`model-pricing ${pricing.status} ${pricing.body}`);
  }

  const agent = await httpJson(corePort, 'GET', `/api/agents/${AGENT}`);
  const subject = JSON.parse(agent.body).subject_id;
  if (!subject || subject <= 0) {
    throw new Error(`subject_id missing: ${agent.body}`);
  }

  const inst = await httpJson(corePort, 'PUT', `/api/gate-instances/${INSTANCE}`, {
    auth: TOKEN,
    body: JSON.stringify({
      kind_id: 'web',
      subject_id: subject,
      enabled: true,
      config_b64: CONFIG_B64,
    }),
  });
  if (inst.status !== 200 && inst.status !== 201) {
    throw new Error(`instance PUT ${inst.status} ${inst.body}`);
  }

  const placement = path.join(root, 'placement.json');
  writeFileSync(
    placement,
    JSON.stringify({
      http_bind: `127.0.0.1:${gwPort}`,
      core_socket: sock,
      instances: [{ instance_id: INSTANCE, revision: 1, author_id: AUTHOR }],
    }),
  );

  const gw = spawnLogged(
    binPath('web-gateway'),
    [placement],
    root,
    { ...process.env, RUST_LOG: 'web_gateway=info,opencrab_web_gateway=info' },
    path.join(root, 'gateway.log'),
  );
  children.push(gw);
  await waitTcp(gwPort, 15_000);

  const origin = await startOrigin({
    uiPort,
    corePort,
    gwPort,
    distDir: path.join(WEB_ROOT, 'dist'),
  });
  servers.push(origin);
  const originUrl = `http://127.0.0.1:${uiPort}`;
  if (!originUrl.startsWith('http://')) {
    throw new Error(`origin is not plain HTTP: ${originUrl}`);
  }

  console.log(`[e2e harness] origin=${originUrl} core=${corePort} gateway=${gwPort} llm=${llmPort}`);

  return {
    origin: originUrl,
    corePort,
    gwPort,
    root,
    async stop() {
      for (const child of children) {
        if (!child.killed) child.kill('SIGTERM');
      }
      for (const child of children) {
        await new Promise((resolve) => {
          const t = setTimeout(() => {
            child.kill('SIGKILL');
            resolve();
          }, 2000);
          child.on('exit', () => {
            clearTimeout(t);
            resolve();
          });
        });
      }
      for (const s of servers) {
        await new Promise((resolve) => s.close(() => resolve()));
      }
      rmSync(sock, { force: true });
    },
  };
}
