// Loopback stub of the native API for the browser budgets (WP-UI-09.T5).
//
// It answers exactly the endpoints the shell calls from the server picker to a
// signed-in session, plus the metadata, list and document reads the form and
// list views make, from fixed JSON in data/. It exists so the signed-in
// budgets (browser.form.open, browser.list.scroll.frame, browser.child-edit.key)
// can run in CI with no real server and no secret.
//
// Safety, all enforced here and tested:
//   * refuses to start unless CI=true or SPOT_BUDGETS_STUB=1;
//   * binds 127.0.0.1 only (never 0.0.0.0, never a name);
//   * one fixed development credential pair, a test value of this stub that no
//     real server accepts and that is never part of the build;
//   * the session is a bearer token in a header: no cookie is ever set;
//   * CORS allows only the origin(s) it was started with, never "*".
//
// usage: node stub-backend/server.mjs [--allow-origin http://127.0.0.1:PORT]...
//        prints one JSON line {"url": "...", "port": N} when it listens.
import fs from 'node:fs';
import http from 'node:http';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));

// A published test value of this stub, see README.md. Not a secret.
export const TEST_USER = 'budgets@stub.test';
export const TEST_PASSWORD = 'stub-only-not-a-secret';
const TOKEN = 'stub-session-0000000000000000';
const API_VERSION = '0.9.0';
const USER = { id: '0192f3c4-0005-7000-8000-000000000001', email: TEST_USER, full_name: 'Budget Stub User' };
const EXPIRES_AT = '2099-01-01T00:00:00Z';

export function stubAllowed(env = process.env) {
  return env.CI === 'true' || env.SPOT_BUDGETS_STUB === '1';
}

const readJson = (name) => JSON.parse(fs.readFileSync(path.join(here, 'data', name), 'utf8'));

function loadData() {
  const rows = readJson('list-rows.json');
  const document = readJson('document.json');
  return {
    meta: readJson('meta.json'),
    rows,
    byId: new Map(rows.map((row) => [row.id, row])),
    document,
  };
}

let requestCounter = 0;
const requestId = () => `0192f3c4-0006-7000-8000-${String(++requestCounter).padStart(12, '0')}`;

export async function startStub({ allowOrigins = [], port = 0, env = process.env } = {}) {
  if (!stubAllowed(env)) {
    throw new Error('the budget stub backend refuses to run outside CI: set CI=true or SPOT_BUDGETS_STUB=1');
  }
  const data = loadData();
  const stats = { live: 0, ready: 0, logins: 0, badLogins: 0, sessions: 0, meta: 0, lists: 0, documents: 0, requests: 0 };
  const allowed = new Set(allowOrigins);

  const respond = (res, status, body, headers = {}) => {
    const payload = body === undefined ? '' : JSON.stringify(body);
    res.writeHead(status, {
      'Content-Type': 'application/json; charset=utf-8',
      'Cache-Control': 'no-store',
      'X-Request-Id': requestId(),
      ...headers,
    });
    res.end(status === 204 || status === 304 ? undefined : payload);
  };
  const fail = (res, status, code, message, headers = {}) =>
    respond(res, status, { error: { code, message, retryable: false, request_id: requestId() } }, headers);

  const authorized = (req) => (req.headers.authorization || '') === `Bearer ${TOKEN}`;

  const page = (rows, url, body) => {
    const limit = Math.min(500, Math.max(1, Number(body.limit ?? url.searchParams.get('limit') ?? 20) || 20));
    const cursor = body.cursor ?? url.searchParams.get('cursor');
    let start = 0;
    if (cursor) {
      start = Number.parseInt(Buffer.from(String(cursor), 'base64url').toString('utf8'), 10);
      if (!Number.isInteger(start) || start < 0 || start > rows.length) return null;
    }
    const slice = rows.slice(start, start + limit);
    const next = start + limit < rows.length ? Buffer.from(String(start + limit)).toString('base64url') : null;
    const out = { data: slice, next_cursor: next };
    if (body.with_total === true) out.total = rows.length;
    return out;
  };

  const readBody = (req) => new Promise((resolve, reject) => {
    const chunks = [];
    let size = 0;
    req.on('data', (chunk) => {
      size += chunk.length;
      if (size > 65536) { reject(new Error('too large')); req.destroy(); return; }
      chunks.push(chunk);
    });
    req.on('end', () => resolve(Buffer.concat(chunks).toString('utf8')));
    req.on('error', reject);
  });

  const route = async (req, res) => {
    stats.requests += 1;
    const origin = req.headers.origin;
    const cors = {};
    if (origin && allowed.has(origin)) {
      cors['Access-Control-Allow-Origin'] = origin;
      cors.Vary = 'Origin';
      cors['Access-Control-Expose-Headers'] = 'ETag, X-Request-Id, Retry-After';
    }
    const send = (status, body, headers = {}) => respond(res, status, body, { ...cors, ...headers });
    const reject = (status, code, message) => fail(res, status, code, message, cors);

    if (req.method === 'OPTIONS') {
      if (!origin || !allowed.has(origin)) return reject(403, 'api.invalid_request', 'The origin is not allowed.');
      res.writeHead(204, {
        ...cors,
        'Access-Control-Allow-Methods': 'GET, POST, OPTIONS',
        'Access-Control-Allow-Headers': req.headers['access-control-request-headers'] || 'authorization, content-type',
        'Access-Control-Max-Age': '600',
      });
      return res.end();
    }

    let url;
    try {
      url = new URL(req.url, 'http://127.0.0.1');
    } catch {
      return reject(400, 'api.invalid_request', 'Bad request target.');
    }
    const pathname = url.pathname;
    const segments = pathname.split('/').filter(Boolean).map((part) => {
      try { return decodeURIComponent(part); } catch { return part; }
    });

    // Test-only introspection for the driver and the tests: not part of the contract.
    if (pathname === '/__stub/state' && req.method === 'GET') {
      return send(200, { bind: server.address().address, stats: { ...stats } });
    }

    if (pathname === '/api/health/live') {
      if (req.method !== 'GET') return reject(405, 'api.method_not_allowed', 'GET only.');
      stats.live += 1;
      return send(200, { status: 'live', version: API_VERSION });
    }
    if (pathname === '/api/health/ready') {
      if (req.method !== 'GET') return reject(405, 'api.method_not_allowed', 'GET only.');
      stats.ready += 1;
      return send(200, { status: 'ready', checks: [{ name: 'stub', ok: true, detail: 'fixed data' }] });
    }

    if (pathname === '/api/v1/auth/login') {
      if (req.method !== 'POST') return reject(405, 'api.method_not_allowed', 'POST only.');
      let body;
      try {
        body = JSON.parse(await readBody(req));
      } catch {
        return reject(400, 'api.invalid_json', 'The body is not valid JSON.');
      }
      if (!body || typeof body.user !== 'string' || typeof body.password !== 'string') {
        return reject(400, 'api.invalid_request', 'user and password are required.');
      }
      if (body.user !== TEST_USER || body.password !== TEST_PASSWORD) {
        stats.badLogins += 1;
        return reject(401, 'auth.invalid_credentials', 'The credentials are wrong.');
      }
      stats.logins += 1;
      return send(200, { data: { token: TOKEN, expires_at: EXPIRES_AT, user: USER } });
    }

    // Everything below needs the session.
    if (segments[0] !== 'api' || segments[1] !== 'v1') return reject(404, 'api.unknown_route', 'No endpoint at this path.');
    if (!(req.headers.authorization || '')) return reject(401, 'auth.required', 'Sign in first.');
    if (!authorized(req)) return reject(401, 'auth.session_expired', 'The session has ended; sign in again.');

    if (pathname === '/api/v1/auth/logout') {
      if (req.method !== 'POST') return reject(405, 'api.method_not_allowed', 'POST only.');
      return send(204);
    }
    if (pathname === '/api/v1/auth/session') {
      if (req.method !== 'GET') return reject(405, 'api.method_not_allowed', 'GET only.');
      stats.sessions += 1;
      return send(200, { data: { expires_at: EXPIRES_AT, user: USER, site: 'budgets.stub.test' } });
    }

    if (segments[2] === 'meta' && segments.length === 4) {
      if (req.method !== 'GET') return reject(405, 'api.method_not_allowed', 'GET only.');
      const meta = data.meta[segments[3]];
      if (!meta) return reject(404, 'meta.unknown_doctype', 'No DocType with this name.');
      stats.meta += 1;
      const etag = `"${meta.version}"`;
      if (req.headers['if-none-match'] === etag) return send(304, undefined, { ETag: etag });
      return send(200, { data: meta }, { ETag: etag });
    }

    if (segments[2] === 'resource' && segments[3] === 'sales_invoice') {
      const rest = segments.slice(4);
      if (rest.length === 0 && req.method === 'GET') {
        const out = page(data.rows, url, {});
        if (!out) return reject(400, 'api.invalid_request', 'The cursor is not valid.');
        stats.lists += 1;
        return send(200, out);
      }
      if (rest.length === 1 && rest[0] === 'query' && req.method === 'POST') {
        let body;
        try {
          const text = await readBody(req);
          body = text ? JSON.parse(text) : {};
        } catch {
          return reject(400, 'api.invalid_json', 'The body is not valid JSON.');
        }
        const out = page(data.rows, url, body && typeof body === 'object' ? body : {});
        if (!out) return reject(400, 'api.invalid_request', 'The cursor is not valid.');
        stats.lists += 1;
        return send(200, out);
      }
      if (rest.length === 1 && req.method === 'GET') {
        const found = rest[0] === data.document.id ? data.document : data.byId.get(rest[0]);
        if (!found) return reject(404, 'document.not_found', 'No document with this id.');
        stats.documents += 1;
        return send(200, { data: found }, { ETag: `"${found.version}"` });
      }
      if (rest.length <= 1) return reject(405, 'api.method_not_allowed', 'This method is not served by the stub.');
    }
    if (segments[2] === 'resource' && data.meta[segments[3]] === undefined) {
      return reject(404, 'meta.unknown_doctype', 'No DocType with this name.');
    }
    return reject(404, 'api.unknown_route', 'No endpoint at this path.');
  };

  const server = http.createServer((req, res) => {
    route(req, res).catch(() => {
      if (!res.headersSent) fail(res, 500, 'http.internal', 'Stub error.');
      else res.end();
    });
  });
  await new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(port, '127.0.0.1', resolve);
  });
  const { port: bound } = server.address();
  return {
    port: bound,
    url: `http://127.0.0.1:${bound}`,
    stats: () => ({ ...stats }),
    close: () => new Promise((resolve) => { server.close(resolve); server.closeAllConnections?.(); }),
  };
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const args = process.argv.slice(2);
  const origins = [];
  for (let index = 0; index < args.length; index += 1) {
    if (args[index] === '--allow-origin') origins.push(args[++index]);
  }
  try {
    const stub = await startStub({ allowOrigins: origins });
    console.log(JSON.stringify({ url: stub.url, port: stub.port }));
    const stop = () => stub.close().then(() => process.exit(0));
    process.on('SIGTERM', stop);
    process.on('SIGINT', stop);
  } catch (error) {
    console.error(`stub-backend: ${error.message}`);
    process.exit(2);
  }
}
