// Static server for the published browser build (or the fixture page).
//
// Serves the layout clients/avalonia/docs/browser-publish.md describes: Brotli
// (then gzip) siblings with Content-Encoding, `application/wasm`, immutable
// caching for fingerprinted files and revalidation for index.html/assets.json,
// and index.html for an unknown navigation path. Method borrowed from
// lab/SPK-07/serve.mjs; the code is not.
import fs from 'node:fs';
import http from 'node:http';
import path from 'node:path';

const TYPES = {
  '.br': 'application/octet-stream',
  '.css': 'text/css; charset=utf-8',
  '.dat': 'application/octet-stream',
  '.dll': 'application/octet-stream',
  '.gz': 'application/octet-stream',
  '.html': 'text/html; charset=utf-8',
  '.ico': 'image/x-icon',
  '.js': 'text/javascript; charset=utf-8',
  '.json': 'application/json; charset=utf-8',
  '.mjs': 'text/javascript; charset=utf-8',
  '.pdb': 'application/octet-stream',
  '.png': 'image/png',
  '.svg': 'image/svg+xml',
  '.wasm': 'application/wasm',
  '.woff2': 'font/woff2',
};

const REVALIDATE = /^(index\.html|assets\.json|spot-browser\.mjs|calc\.mjs)$/;

function send(res, status, body, headers = {}) {
  res.writeHead(status, { 'Cache-Control': 'no-cache', ...headers });
  res.end(body);
}

function handler(root) {
  return (request, response) => {
    let pathname;
    try {
      pathname = decodeURIComponent(new URL(request.url, 'http://127.0.0.1').pathname);
    } catch {
      return send(response, 400, 'bad request');
    }
    if (pathname.endsWith('/')) pathname += 'index.html';
    const target = path.resolve(root, '.' + path.normalize(pathname));
    if (target !== root && !target.startsWith(root + path.sep)) {
      return send(response, 403, 'forbidden');
    }
    let file = target;
    if (!fs.existsSync(file) || !fs.statSync(file).isFile()) {
      const wantsHtml = (request.headers.accept || '').includes('text/html');
      if (!wantsHtml) return send(response, 404, 'not found');
      file = path.join(root, 'index.html');
      if (!fs.existsSync(file)) return send(response, 404, 'not found');
    }
    const accepts = request.headers['accept-encoding'] || '';
    const encodings = accepts.includes('br') ? ['br', 'gz'] : accepts.includes('gzip') ? ['gz'] : [];
    let bodyPath = file;
    let encoding = null;
    for (const candidate of encodings) {
      if (fs.existsSync(`${file}.${candidate}`)) {
        bodyPath = `${file}.${candidate}`;
        encoding = candidate === 'br' ? 'br' : 'gzip';
        break;
      }
    }
    let body;
    try {
      body = fs.readFileSync(bodyPath);
    } catch {
      return send(response, 404, 'not found');
    }
    const name = path.basename(file);
    const headers = {
      'Content-Type': TYPES[path.extname(file).toLowerCase()] || 'application/octet-stream',
      'Content-Length': String(body.length),
      'Cache-Control': REVALIDATE.test(name) ? 'no-cache' : 'public, max-age=31536000, immutable',
    };
    if (encoding) {
      headers['Content-Encoding'] = encoding;
      headers.Vary = 'Accept-Encoding';
    }
    response.writeHead(200, headers);
    response.end(body);
  };
}

export async function serve(root) {
  const resolved = path.resolve(root);
  const server = http.createServer(handler(resolved));
  await new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(0, '127.0.0.1', resolve);
  });
  const { port } = server.address();
  return {
    port,
    url: `http://127.0.0.1:${port}`,
    close: () => new Promise((resolve) => server.close(resolve)),
  };
}
