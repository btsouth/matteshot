// SPDX-License-Identifier: MIT OR Apache-2.0
import assert from "node:assert/strict";
import test from "node:test";

import worker from "../src/index.js";

const TOKEN = "a-long-operator-token-0123456789";
const OTHER_TOKEN = "second-operator-token-9876543210";
const HOST = "share.example.com";

class MemoryR2 {
  constructor() {
    this.objects = new Map();
  }

  async get(key) {
    return this.objects.get(key) ?? null;
  }

  async put(key, body, options = {}) {
    const bytes = body instanceof ArrayBuffer ? new Uint8Array(body) : body;
    this.objects.set(key, {
      body: bytes,
      // R2 stamps this itself on every put.
      uploaded: new Date(),
      httpMetadata: options.httpMetadata || {},
      customMetadata: options.customMetadata || {},
    });
  }

  async delete(key) {
    this.objects.delete(key);
  }
}

function environment(overrides = {}) {
  return { SHARES: new MemoryR2(), UPLOAD_TOKENS: `${TOKEN}, ${OTHER_TOKEN}`, ...overrides };
}

function png() {
  return new Uint8Array([137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 0x49, 0x48, 0x44, 0x52, 1, 2, 3, 4]);
}

function mp4() {
  return new Uint8Array([0, 0, 0, 24, 0x66, 0x74, 0x79, 0x70, 0x6d, 0x70, 0x34, 0x32, 0, 0, 0, 0]);
}

function upload({ file = png(), type = "image/png", token = TOKEN, host = HOST } = {}) {
  const form = new FormData();
  form.set("file", new File([file], type === "image/png" ? "shot.png" : "clip.mp4", { type }));
  const headers = token === null ? {} : { Authorization: `Bearer ${token}` };
  return new Request(`https://${host}/v1/share`, { method: "POST", body: form, headers });
}

async function share(env, options) {
  const response = await worker.fetch(upload(options), env);
  return { response, body: await response.json() };
}

test("an operator token uploads a PNG and gets a link on this host", async () => {
  const env = environment();
  const { response, body } = await share(env);
  assert.equal(response.status, 200);
  assert.match(body.url, /^https:\/\/share\.example\.com\/s\/[2-9A-HJ-NP-Z]{12}$/);
  assert.equal(env.SHARES.objects.size, 1);
});

test("the link follows whatever host the server runs on", async () => {
  const { body } = await share(environment(), { host: "matteshot-share.someone.workers.dev" });
  assert.match(body.url, /^https:\/\/matteshot-share\.someone\.workers\.dev\/s\//);
});

test("each of several configured tokens is accepted", async () => {
  const { response } = await share(environment(), { token: OTHER_TOKEN });
  assert.equal(response.status, 200);
});

test("a missing or wrong token uploads nothing", async () => {
  for (const token of [null, "wrong-token-but-long-enough-000", TOKEN.slice(0, -1), `${TOKEN}x`]) {
    const env = environment();
    const { response } = await share(env, { token });
    assert.equal(response.status, 401, `token ${token}`);
    assert.equal(env.SHARES.objects.size, 0);
  }
});

test("no configured tokens means uploads are off, not open", async () => {
  for (const UPLOAD_TOKENS of [undefined, "", "short"]) {
    const env = environment({ UPLOAD_TOKENS });
    const { response } = await share(env);
    assert.equal(response.status, 503);
    assert.equal(env.SHARES.objects.size, 0);
  }
});

test("an MP4 with an ftyp box is accepted", async () => {
  const { response } = await share(environment(), { file: mp4(), type: "video/mp4" });
  assert.equal(response.status, 200);
});

test("only PNG and MP4 are accepted", async () => {
  const { response } = await share(environment(), { type: "image/gif", file: png() });
  assert.equal(response.status, 415);
});

test("bytes that do not match the declared type are refused", async () => {
  const html = new TextEncoder().encode("<!doctype html><script>alert(1)</script>");
  for (const [file, type] of [
    [html, "image/png"],
    [html, "video/mp4"],
    [new Uint8Array([137, 80, 78, 71, 13, 10, 26, 10, 1, 2, 3, 4, 5, 6, 7, 8]), "image/png"],
    [new Uint8Array([0xff, 0xff, 0xff, 0xff, 0x66, 0x74, 0x79, 0x70, 0, 0, 0, 0]), "video/mp4"],
  ]) {
    const env = environment();
    const { response } = await share(env, { file, type });
    assert.equal(response.status, 415);
    assert.equal(env.SHARES.objects.size, 0);
  }
});

test("an empty file or a missing file is a 400", async () => {
  const { response } = await share(environment(), { file: new Uint8Array() });
  assert.equal(response.status, 400);

  const request = new Request(`https://${HOST}/v1/share`, {
    method: "POST",
    body: new FormData(),
    headers: { Authorization: `Bearer ${TOKEN}` },
  });
  assert.equal((await worker.fetch(request, environment())).status, 400);
});

test("a file over the size limit is refused", async () => {
  const big = new Uint8Array(2048);
  big.set(png());
  const { response } = await share(environment({ MAX_UPLOAD_BYTES: "1024" }), { file: big });
  assert.equal(response.status, 413);
});

test("a shared PNG is served with its type, nosniff, and a sandbox", async () => {
  const env = environment();
  const { body } = await share(env);
  const id = body.url.split("/").pop();
  const raw = await worker.fetch(new Request(`https://${HOST}/s/${id}/raw`), env);
  assert.equal(raw.status, 200);
  assert.equal(raw.headers.get("Content-Type"), "image/png");
  assert.equal(raw.headers.get("X-Content-Type-Options"), "nosniff");
  assert.match(raw.headers.get("Content-Security-Policy"), /sandbox/);
  assert.match(raw.headers.get("X-Robots-Tag"), /noindex/);

  const page = await worker.fetch(new Request(`https://${HOST}/s/${id}`), env);
  assert.equal(page.status, 200);
  assert.match(await page.text(), new RegExp(`/s/${id}/raw`));
});

test("an upload past its expiry is gone, and caching never outlives it", async () => {
  const env = environment({ SHARE_TTL_DAYS: "30" });
  const { body } = await share(env);
  const id = body.url.split("/").pop();
  const object = env.SHARES.objects.get(id);

  object.uploaded = new Date(Date.now() - 29.5 * 24 * 60 * 60 * 1000);
  const fresh = await worker.fetch(new Request(`https://${HOST}/s/${id}/raw`), env);
  const maxAge = Number(fresh.headers.get("Cache-Control").match(/max-age=(\d+)/)[1]);
  assert.ok(maxAge <= 12 * 60 * 60 + 5, `max-age ${maxAge} outlives the expiry`);

  object.uploaded = new Date(Date.now() - 31 * 24 * 60 * 60 * 1000);
  assert.equal((await worker.fetch(new Request(`https://${HOST}/s/${id}/raw`), env)).status, 404);
  assert.equal((await worker.fetch(new Request(`https://${HOST}/s/${id}`), env)).status, 404);
});

test("an object whose age cannot be read is not served", async () => {
  const env = environment();
  await env.SHARES.put("ABCDEFGHJKMN", png(), { httpMetadata: { contentType: "image/png" } });
  env.SHARES.objects.get("ABCDEFGHJKMN").uploaded = undefined;
  const response = await worker.fetch(new Request(`https://${HOST}/s/ABCDEFGHJKMN/raw`), env);
  assert.equal(response.status, 404);
});

test("an operator can delete an upload before it expires", async () => {
  const env = environment();
  const { body } = await share(env);
  const id = body.url.split("/").pop();

  const anonymous = await worker.fetch(
    new Request(`https://${HOST}/v1/share/${id}`, { method: "DELETE" }),
    env,
  );
  assert.equal(anonymous.status, 401);
  assert.equal(env.SHARES.objects.size, 1);

  const deleted = await worker.fetch(
    new Request(`https://${HOST}/v1/share/${id}`, {
      method: "DELETE",
      headers: { Authorization: `Bearer ${OTHER_TOKEN}` },
    }),
    env,
  );
  assert.equal(deleted.status, 200);
  assert.equal(env.SHARES.objects.size, 0);
});

test("ids that could not have been minted are never looked up", async () => {
  const env = environment();
  for (const path of ["/s/../../etc", "/s/abcdefghjkmn", "/s/ABCDEFGHJKM0", "/s/ABC/raw"]) {
    const response = await worker.fetch(new Request(`https://${HOST}${path}`), env);
    assert.equal(response.status, 404, path);
  }
});

test("plain http is redirected and legacy TLS is refused", async () => {
  const redirected = await worker.fetch(new Request(`http://${HOST}/health`), environment());
  assert.equal(redirected.status, 308);
  assert.equal(redirected.headers.get("Location"), `https://${HOST}/health`);

  const request = new Request(`https://${HOST}/health`);
  Object.defineProperty(request, "cf", { value: { tlsVersion: "TLSv1.1" } });
  assert.equal((await worker.fetch(request, environment())).status, 426);
});

test("a rate limiter that says no stops the upload", async () => {
  const env = environment({ SHARE_LIMITER: { limit: async () => ({ success: false }) } });
  const request = upload();
  request.headers.set("CF-Connecting-IP", "203.0.113.9");
  const response = await worker.fetch(request, env);
  assert.equal(response.status, 429);
  assert.equal(env.SHARES.objects.size, 0);
});

test("the per-token limiter is keyed by token, not by the token itself", async () => {
  const keys = [];
  const env = environment({
    TOKEN_LIMITER: {
      limit: async ({ key }) => {
        keys.push(key);
        return { success: true };
      },
    },
  });
  await share(env);
  assert.equal(keys.length, 1);
  assert.ok(!keys[0].includes(TOKEN), "the raw token must not reach the limiter");
});

test("health reports whether uploads are on without revealing tokens", async () => {
  const on = await worker.fetch(new Request(`https://${HOST}/health`), environment());
  const body = await on.json();
  assert.deepEqual(body, { ok: true, service: "matteshot-share", storage: true, uploads: true });
  const off = await worker.fetch(
    new Request(`https://${HOST}/health`),
    environment({ UPLOAD_TOKENS: undefined }),
  );
  assert.equal((await off.json()).uploads, false);
});

test("robots are told to stay out", async () => {
  const response = await worker.fetch(new Request(`https://${HOST}/robots.txt`), environment());
  assert.equal(await response.text(), "User-agent: *\nDisallow: /\n");
});
