// client.ts: the transport-injected client (WP-WEB-10.T6).
// Generated from contracts/native-api/openapi.yaml, contract version 1.2.3.
// Generated, do not edit. Regenerate with tools/contract-clients/typescript/generate.py.

import { operations } from "./operations";
import type { OperationId, Operations } from "./operations";

/** Response headers as the transport reports them (any case). */
export type ResponseHeaders = { [name: string]: string };

export interface HttpRequest {
  method: string;
  url: string;
  headers: { [name: string]: string };
  body: Uint8Array | string | null;
}

export interface HttpResponse {
  status: number;
  headers: ResponseHeaders;
  body: Uint8Array | string | null;
}

/** The only door to the network. The client never calls `fetch` itself. */
export interface Transport {
  send(request: HttpRequest): Promise<HttpResponse>;
}

export interface FormPart {
  name: string;
  filename?: string;
  contentType?: string;
  data: Uint8Array | string;
}

/** The parts of a `multipart/form-data` request body. */
export type FormParts = FormPart[];

interface LooseArgs {
  path?: object;
  query?: object;
  headers?: object;
  body?: unknown;
}

interface LooseEntry {
  method: string;
  path: string;
  queryParams: ReadonlyArray<{ name: string }>;
  headerParams: ReadonlyArray<{ name: string }>;
  request: { contentType: string } | null;
  success: ReadonlyArray<{ status: number; contentType: string | null }>;
  errors: { readonly [status: string]: string | null };
}

type Dict = { [key: string]: unknown };

function utf8Encode(text: string): Uint8Array {
  const out: number[] = [];
  for (const ch of text) {
    let cp = ch.codePointAt(0) as number;
    if (cp >= 0xd800 && cp <= 0xdfff) cp = 0xfffd;
    if (cp < 0x80) {
      out.push(cp);
    } else if (cp < 0x800) {
      out.push(0xc0 | (cp >> 6), 0x80 | (cp & 0x3f));
    } else if (cp < 0x10000) {
      out.push(0xe0 | (cp >> 12), 0x80 | ((cp >> 6) & 0x3f), 0x80 | (cp & 0x3f));
    } else {
      out.push(0xf0 | (cp >> 18), 0x80 | ((cp >> 12) & 0x3f), 0x80 | ((cp >> 6) & 0x3f), 0x80 | (cp & 0x3f));
    }
  }
  return Uint8Array.from(out);
}

function utf8Decode(bytes: Uint8Array): string {
  const chunks: string[] = [];
  let pending: number[] = [];
  let i = 0;
  while (i < bytes.length) {
    const b = bytes[i];
    let cp = 0xfffd;
    let len = 1;
    if (b < 0x80) {
      cp = b;
    } else if (b >= 0xc2 && b < 0xe0 && i + 1 < bytes.length && (bytes[i + 1] & 0xc0) === 0x80) {
      cp = ((b & 0x1f) << 6) | (bytes[i + 1] & 0x3f);
      len = 2;
    } else if (b >= 0xe0 && b < 0xf0 && i + 2 < bytes.length && (bytes[i + 1] & 0xc0) === 0x80 && (bytes[i + 2] & 0xc0) === 0x80) {
      cp = ((b & 0x0f) << 12) | ((bytes[i + 1] & 0x3f) << 6) | (bytes[i + 2] & 0x3f);
      len = 3;
      if (cp < 0x800 || (cp >= 0xd800 && cp <= 0xdfff)) cp = 0xfffd;
    } else if (
      b >= 0xf0 && b < 0xf5 && i + 3 < bytes.length &&
      (bytes[i + 1] & 0xc0) === 0x80 && (bytes[i + 2] & 0xc0) === 0x80 && (bytes[i + 3] & 0xc0) === 0x80
    ) {
      cp = ((b & 0x07) << 18) | ((bytes[i + 1] & 0x3f) << 12) | ((bytes[i + 2] & 0x3f) << 6) | (bytes[i + 3] & 0x3f);
      len = 4;
      if (cp < 0x10000 || cp > 0x10ffff) cp = 0xfffd;
    }
    pending.push(cp);
    i += len;
    if (pending.length >= 4096) {
      chunks.push(String.fromCodePoint(...pending));
      pending = [];
    }
  }
  if (pending.length > 0) chunks.push(String.fromCodePoint(...pending));
  return chunks.join("");
}

function concat(chunks: Uint8Array[]): Uint8Array {
  let total = 0;
  for (const c of chunks) total += c.length;
  const out = new Uint8Array(total);
  let at = 0;
  for (const c of chunks) {
    out.set(c, at);
    at += c.length;
  }
  return out;
}

function buildPath(template: string, values: object | undefined): string {
  const map = (values ?? {}) as Dict;
  return template.replace(/\{([^}]+)\}/g, (_match: string, name: string): string => {
    const v = map[name];
    if (v === undefined || v === null) throw new Error("missing path parameter " + name);
    return encodeURIComponent(String(v));
  });
}

function buildQuery(specs: ReadonlyArray<{ name: string }>, values: object | undefined): string {
  const map = (values ?? {}) as Dict;
  const pairs: string[] = [];
  for (const spec of specs) {
    const v = map[spec.name];
    if (v === undefined || v === null) continue;
    const items: unknown[] = Array.isArray(v) ? v : [v];
    for (const item of items) {
      pairs.push(encodeURIComponent(spec.name) + "=" + encodeURIComponent(String(item)));
    }
  }
  return pairs.length === 0 ? "" : "?" + pairs.join("&");
}

function quoteParam(text: string): string {
  return text.replace(/["\r\n]/g, (c: string): string => (c === '"' ? "%22" : c === "\r" ? "%0D" : "%0A"));
}

function encodeMultipart(parts: FormParts, boundary: string): Uint8Array {
  const chunks: Uint8Array[] = [];
  for (const part of parts) {
    let head = "--" + boundary + '\r\nContent-Disposition: form-data; name="' + quoteParam(part.name) + '"';
    if (part.filename !== undefined) head += '; filename="' + quoteParam(part.filename) + '"';
    head += "\r\n";
    if (part.contentType !== undefined) head += "Content-Type: " + part.contentType.replace(/[\r\n]/g, " ") + "\r\n";
    chunks.push(utf8Encode(head + "\r\n"));
    chunks.push(typeof part.data === "string" ? utf8Encode(part.data) : part.data);
    chunks.push(utf8Encode("\r\n"));
  }
  chunks.push(utf8Encode("--" + boundary + "--\r\n"));
  return concat(chunks);
}

function encodeForm(body: unknown): string {
  const map = (body ?? {}) as Dict;
  const pairs: string[] = [];
  for (const key of Object.keys(map)) {
    const v = map[key];
    if (v === undefined || v === null) continue;
    const items: unknown[] = Array.isArray(v) ? v : [v];
    for (const item of items) pairs.push(encodeURIComponent(key) + "=" + encodeURIComponent(String(item)));
  }
  return pairs.join("&");
}

function encodeBody(contentType: string, body: unknown): { contentType: string; body: Uint8Array | string } {
  if (contentType === "application/json") return { contentType, body: JSON.stringify(body) };
  if (contentType === "application/x-www-form-urlencoded") return { contentType, body: encodeForm(body) };
  if (contentType === "multipart/form-data") {
    const boundary = "spot" + Math.random().toString(36).slice(2) + Date.now().toString(36);
    return { contentType: "multipart/form-data; boundary=" + boundary, body: encodeMultipart(body as FormParts, boundary) };
  }
  if (typeof body === "string" || body instanceof Uint8Array) return { contentType, body };
  throw new Error("request body for " + contentType + " must be a string or Uint8Array");
}

function headerValue(headers: ResponseHeaders, name: string): string | undefined {
  const wanted = name.toLowerCase();
  for (const key of Object.keys(headers)) {
    if (key.toLowerCase() === wanted) return headers[key];
  }
  return undefined;
}

function isJson(contentType: string): boolean {
  const base = contentType.split(";")[0].trim().toLowerCase();
  return base === "application/json" || base.endsWith("+json");
}

function isText(contentType: string): boolean {
  const base = contentType.split(";")[0].trim().toLowerCase();
  return (
    base.startsWith("text/") ||
    base === "application/x-ndjson" ||
    base === "application/xml" ||
    base === "application/samlmetadata+xml"
  );
}

function bodyText(body: Uint8Array | string | null): string {
  if (body === null) return "";
  return typeof body === "string" ? body : utf8Decode(body);
}

function decodeSuccess(contentType: string | null, response: HttpResponse): unknown {
  if (contentType === null) return null;
  if (isJson(contentType)) {
    const text = bodyText(response.body);
    return text.trim() === "" ? null : JSON.parse(text);
  }
  if (isText(contentType)) return bodyText(response.body);
  if (response.body === null) return new Uint8Array(0);
  return typeof response.body === "string" ? utf8Encode(response.body) : response.body;
}

function decodeProblem(response: HttpResponse): unknown {
  const text = bodyText(response.body);
  if (text.trim() === "") return null;
  const contentType = headerValue(response.headers, "content-type");
  if (contentType !== undefined && isJson(contentType)) {
    try {
      return JSON.parse(text);
    } catch {
      return text;
    }
  }
  return text;
}

function buildResult(entry: LooseEntry, response: HttpResponse): unknown {
  const status = response.status;
  const headers = response.headers;
  for (const s of entry.success) {
    if (s.status === status) return { status, headers, body: decodeSuccess(s.contentType, response) };
  }
  const declared = Object.prototype.hasOwnProperty.call(entry.errors, String(status));
  if (declared) return { status, headers, problem: decodeProblem(response) };
  return { status, headers, unexpected: true, problem: decodeProblem(response) };
}

/**
 * The native API client. One method per operation, each taking one typed
 * argument object and returning a discriminated result; an HTTP status never
 * throws. The `transport` is the only thing that touches the network.
 */
export class NativeApi {
  constructor(
    private readonly transport: Transport,
    private readonly baseUrl: string = "",
  ) {}

  private async call(id: OperationId, args: LooseArgs | undefined): Promise<unknown> {
    const entry = operations[id] as unknown as LooseEntry;
    const a: LooseArgs = args ?? {};
    const url = this.baseUrl + buildPath(entry.path, a.path) + buildQuery(entry.queryParams, a.query);
    const headers: { [name: string]: string } = {};
    const given = (a.headers ?? {}) as Dict;
    for (const spec of entry.headerParams) {
      const v = given[spec.name];
      if (v !== undefined && v !== null) headers[spec.name] = String(v);
    }
    let body: Uint8Array | string | null = null;
    if (entry.request !== null && a.body !== undefined) {
      const encoded = encodeBody(entry.request.contentType, a.body);
      headers["Content-Type"] = encoded.contentType;
      body = encoded.body;
    }
    const response = await this.transport.send({ method: entry.method, url, headers, body });
    return buildResult(entry, response);
  }

  health(): Promise<Operations["health"]["result"]> {
    return this.call("health", undefined) as Promise<Operations["health"]["result"]>;
  }

  /** Attach files. */
  item_attach(args: Operations["item_attach"]["args"]): Promise<Operations["item_attach"]["result"]> {
    return this.call("item_attach", args) as Promise<Operations["item_attach"]["result"]>;
  }

  item_delete(args: Operations["item_delete"]["args"]): Promise<Operations["item_delete"]["result"]> {
    return this.call("item_delete", args) as Promise<Operations["item_delete"]["result"]>;
  }

  /** Fetch one item. */
  item_get(args: Operations["item_get"]["args"]): Promise<Operations["item_get"]["result"]> {
    return this.call("item_get", args) as Promise<Operations["item_get"]["result"]>;
  }

  /** Replace an item. */
  item_replace(args: Operations["item_replace"]["args"]): Promise<Operations["item_replace"]["result"]> {
    return this.call("item_replace", args) as Promise<Operations["item_replace"]["result"]>;
  }
}
