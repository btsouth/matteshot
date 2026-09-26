// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Self-hosted share server for Matteshot builds made with `--features share`.
// A Cloudflare Worker backed by one R2 bucket. See docs/self-hosting-share.md.
//
// Uploads need a bearer token the operator issued (UPLOAD_TOKENS). Anyone
// with a link can view that one upload until it expires. Nothing is listable,
// nothing is indexed, and every upload is deleted after SHARE_TTL_DAYS.

// Screenshots are always PNG (output::save_png); recordings are always MP4
// (record.rs). Anything else did not come from Matteshot's own export path.
const ALLOWED_CONTENT_TYPES = new Map([
  ["image/png", "image"],
  ["video/mp4", "video"],
]);

// Cloudflare's Free plan caps a request body at 100 MB. Refuse the same size
// here so the app and the server tell the same story.
const DEFAULT_MAX_UPLOAD_BYTES = 100 * 1024 * 1024;
const DEFAULT_TTL_DAYS = 30;
const DAY_MS = 24 * 60 * 60 * 1000;

// No 0/O/1/I/L, so a link read aloud cannot be mis-transcribed. 12 characters
// from 31 is about 59 bits, far more than an unlistable bucket needs.
const ID_ALPHABET = "23456789ABCDEFGHJKMNPQRSTUVWXYZ";
const ID_LENGTH = 12;
const ID_PATTERN = /^[2-9A-HJ-NP-Z]{12}$/;

const TOO_MANY_REQUESTS = { error: "Too many requests. Try again in a minute." };

/// A caller's fault, not ours. Carries the status to answer with, so bad
/// input reports as the 4xx it is instead of landing in the 500 handler.
class RequestError extends Error {
  constructor(message, status) {
    super(message);
    this.name = "RequestError";
    this.status = status;
  }
}

export default {
  async fetch(request, env) {
    const url = new URL(request.url);
    try {
      if (url.protocol !== "https:") {
        url.protocol = "https:";
        return new Response(null, {
          status: 308,
          headers: { ...securityHeaders(), Location: url.toString() },
        });
      }
      const tlsVersion = request.cf?.tlsVersion;
      if (tlsVersion === "TLSv1" || tlsVersion === "TLSv1.1") {
        return json({ error: "TLS 1.2 or newer is required." }, 426);
      }

      if (request.method === "GET" && url.pathname === "/health") {
        return json({
          ok: true,
          service: "matteshot-share",
          storage: Boolean(env.SHARES),
          uploads: uploadTokens(env).length > 0,
        });
      }

      if (request.method === "POST" && url.pathname === "/v1/share") {
        const limited = await rateLimit(env.SHARE_LIMITER, clientAddress(request));
        if (limited) return limited;
        return await createShare(request, env, url);
      }

      const deleteMatch = url.pathname.match(/^\/v1\/share\/([^/]+)$/);
      if (request.method === "DELETE" && deleteMatch) {
        return await deleteShare(request, env, deleteMatch[1]);
      }

      const previewMatch = url.pathname.match(/^\/s\/([^/]+)$/);
      if (request.method === "GET" && previewMatch) {
        return await previewPage(previewMatch[1], env);
      }

      const rawMatch = url.pathname.match(/^\/s\/([^/]+)\/raw$/);
      if (request.method === "GET" && rawMatch) {
        return await rawObject(rawMatch[1], env);
      }

      if (request.method === "GET" && url.pathname === "/robots.txt") {
        return new Response("User-agent: *\nDisallow: /\n", {
          headers: { ...securityHeaders(), "Content-Type": "text/plain; charset=utf-8" },
        });
      }

      return json({ error: "Not found." }, 404);
    } catch (error) {
      const caller = error instanceof RequestError;
      if (!caller) {
        console.error("share request failed", { name: error?.name, message: error?.message });
      }
      return json(
        { error: caller ? error.message : "The share server could not complete the request." },
        caller ? error.status : 500,
      );
    }
  },
};

function clientAddress(request) {
  return request.headers.get("CF-Connecting-IP");
}

/// Per-colo and best effort, like every Workers rate limit binding. Every
/// unusual case (missing binding, missing address, a throwing limiter) lets
/// the request through: the token is the real gate, this only blunts a leaked
/// token hammering the endpoint.
async function rateLimit(limiter, key) {
  if (!limiter?.limit || !key) return null;
  try {
    const { success } = await limiter.limit({ key });
    return success === false ? json(TOO_MANY_REQUESTS, 429) : null;
  } catch (error) {
    console.error("rate limit check failed", error?.message);
    return null;
  }
}

/// UPLOAD_TOKENS is a secret holding one or more tokens, separated by commas
/// or newlines, so a token can be rotated or revoked on its own. Tokens under
/// 24 characters are ignored rather than trusted.
function uploadTokens(env) {
  return String(env.UPLOAD_TOKENS ?? "")
    .split(/[\s,]+/)
    .map((token) => token.trim())
    .filter((token) => token.length >= 24);
}

async function sha256(text) {
  return new Uint8Array(await crypto.subtle.digest("SHA-256", new TextEncoder().encode(text)));
}

/// Compares digests rather than the tokens, so the comparison takes the same
/// time whatever the token's length or how much of it matched.
async function authorized(request, env) {
  const header = request.headers.get("Authorization") ?? "";
  const match = header.match(/^Bearer\s+(\S+)$/);
  if (!match) return null;
  const offered = await sha256(match[1]);
  let found = null;
  for (const token of uploadTokens(env)) {
    const expected = await sha256(token);
    let difference = 0;
    for (let index = 0; index < expected.length; index += 1) {
      difference |= expected[index] ^ offered[index];
    }
    if (difference === 0) found = hex(expected).slice(0, 16);
  }
  return found;
}

function hex(bytes) {
  return Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("");
}

function maxUploadBytes(env) {
  const configured = Number(env.MAX_UPLOAD_BYTES);
  return Number.isFinite(configured) && configured > 0
    ? Math.min(configured, DEFAULT_MAX_UPLOAD_BYTES)
    : DEFAULT_MAX_UPLOAD_BYTES;
}

function ttlMs(env) {
  const days = Number(env.SHARE_TTL_DAYS);
  return (Number.isFinite(days) && days > 0 ? Math.min(days, 365) : DEFAULT_TTL_DAYS) * DAY_MS;
}

async function createShare(request, env, url) {
  if (!env.SHARES) throw new RequestError("Share storage is not configured.", 503);
  if (uploadTokens(env).length === 0) {
    throw new RequestError("Uploads are turned off on this server.", 503);
  }
  const tokenId = await authorized(request, env);
  if (!tokenId) throw new RequestError("This upload token is not accepted here.", 401);
  const limited = await rateLimit(env.TOKEN_LIMITER, tokenId);
  if (limited) return limited;

  const limit = maxUploadBytes(env);
  const contentLength = Number(request.headers.get("Content-Length") || 0);
  if (contentLength > limit + 64 * 1024) {
    throw new RequestError("That file is too large to share.", 413);
  }

  let form;
  try {
    form = await request.formData();
  } catch {
    throw new RequestError("Expected a multipart upload.", 400);
  }

  const file = form.get("file");
  if (!(file instanceof Blob)) throw new RequestError("No file was attached.", 400);
  if (file.size === 0) throw new RequestError("The attached file is empty.", 400);
  if (file.size > limit) throw new RequestError("That file is too large to share.", 413);

  const kind = ALLOWED_CONTENT_TYPES.get(file.type);
  if (!kind) throw new RequestError("Only PNG screenshots and MP4 recordings can be shared.", 415);
  if (!(await hasExpectedHeader(file, kind))) {
    throw new RequestError("The attached file does not match its declared type.", 415);
  }

  const id = generateShareId();
  await env.SHARES.put(id, await file.arrayBuffer(), {
    httpMetadata: { contentType: file.type },
    customMetadata: { kind, createdAt: new Date().toISOString(), token: tokenId },
  });

  return json({ url: `https://${url.host}/s/${id}` });
}

/// PNG: the 8-byte signature followed by an IHDR chunk of the right length.
/// MP4: an `ftyp` box first, with a sane box size. Neither proves the whole
/// file decodes, but together with the fixed Content-Type and `nosniff` on
/// the way out, a browser will only ever treat the bytes as an image or video.
async function hasExpectedHeader(file, kind) {
  const header = new Uint8Array(await file.slice(0, 24).arrayBuffer());
  if (kind === "image") {
    const signature = [137, 80, 78, 71, 13, 10, 26, 10];
    const ihdr = [0, 0, 0, 13, 0x49, 0x48, 0x44, 0x52];
    return (
      header.length >= 16 &&
      signature.every((byte, index) => header[index] === byte) &&
      ihdr.every((byte, index) => header[8 + index] === byte)
    );
  }
  if (header.length < 12) return false;
  const boxSize = ((header[0] << 24) | (header[1] << 16) | (header[2] << 8) | header[3]) >>> 0;
  const isFtyp =
    header[4] === 0x66 && header[5] === 0x74 && header[6] === 0x79 && header[7] === 0x70;
  return isFtyp && boxSize >= 16 && boxSize <= 1024;
}

/// Lets an operator take a link down before it expires. Any accepted upload
/// token can delete any upload: tokens belong to the operator's own people.
async function deleteShare(request, env, id) {
  if (!env.SHARES) throw new RequestError("Share storage is not configured.", 503);
  if (!(await authorized(request, env))) {
    throw new RequestError("This upload token is not accepted here.", 401);
  }
  if (!ID_PATTERN.test(id)) throw new RequestError("Not found.", 404);
  await env.SHARES.delete(id);
  return json({ deleted: true });
}

function generateShareId() {
  // Rejection sampling keeps every character equally likely; a plain modulo
  // over 256 would favour the first few letters of the alphabet.
  const limit = 256 - (256 % ID_ALPHABET.length);
  let id = "";
  while (id.length < ID_LENGTH) {
    const bytes = new Uint8Array(ID_LENGTH * 2);
    crypto.getRandomValues(bytes);
    for (const byte of bytes) {
      if (byte < limit && id.length < ID_LENGTH) id += ID_ALPHABET[byte % ID_ALPHABET.length];
    }
  }
  return id;
}

/// When the upload expires, in milliseconds since the epoch. R2 stamps
/// `uploaded` itself, so that is the authority; `createdAt` is the fallback.
/// An object with neither counts as expired: a server that cannot tell how
/// old something is should not keep handing it out.
function expiresAt(object, env) {
  const uploaded =
    object.uploaded instanceof Date
      ? object.uploaded.getTime()
      : Date.parse(object.customMetadata?.createdAt ?? "");
  return Number.isFinite(uploaded) ? uploaded + ttlMs(env) : 0;
}

async function liveObject(id, env) {
  if (!env.SHARES) throw new RequestError("Share storage is not configured.", 503);
  if (!ID_PATTERN.test(id)) return null;
  const object = await env.SHARES.get(id);
  if (!object || Date.now() >= expiresAt(object, env)) return null;
  return object;
}

async function previewPage(id, env) {
  const object = await liveObject(id, env);
  if (!object) return html(expiredPage(), 404);

  const kind = object.customMetadata?.kind === "video" ? "video" : "image";
  // The image is the raw file at its native resolution. Clicking toggles its
  // box between fitting the viewport and 1:1 pixels, in place.
  const media =
    kind === "video"
      ? `<video src="/s/${id}/raw" controls playsinline preload="metadata"></video>`
      : `<input id="zoom" type="checkbox" hidden />
  <label for="zoom"><img src="/s/${id}/raw" id="shot" alt="Shared screenshot" /></label>`;

  return html(`<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8" />
<meta name="viewport" content="width=device-width, initial-scale=1" />
<meta name="robots" content="noindex" />
<title>Shared with Matteshot</title>
<style>
  :root { color-scheme: dark; }
  body { margin: 0; min-height: 100vh; display: flex; flex-direction: column; align-items: center;
    justify-content: center; gap: 20px; background: #151311; color: #e4e0db;
    font: 15px/1.5 -apple-system, "Segoe UI", sans-serif; padding: 24px; box-sizing: border-box;
    overflow: auto; }
  video { max-width: min(96vw, 1900px); max-height: 88vh; border-radius: 10px;
    box-shadow: 0 20px 60px rgba(0, 0, 0, 0.45); }
  #shot { max-width: 96vw; max-height: 88vh; border-radius: 10px;
    box-shadow: 0 20px 60px rgba(0, 0, 0, 0.45); cursor: zoom-in; }
  #zoom:checked + label #shot { max-width: none; max-height: none; cursor: zoom-out; }
  a.download { color: #faa560; text-decoration: none; font-weight: 600; }
  a.download:hover { text-decoration: underline; }
  footer { color: #938c84; font-size: 13px; }
  footer a { color: inherit; }
</style>
</head>
<body>
  ${media}
  <a class="download" href="/s/${id}/raw" download>Download original</a>
  <footer>Shared with <a href="https://matteshot.app">Matteshot</a></footer>
</body>
</html>`);
}

async function rawObject(id, env) {
  const object = await liveObject(id, env);
  if (!object) return json({ error: "This link is no longer available." }, 404);
  // Immutable once uploaded, so cache hard, but never past the expiry: a copy
  // fetched the day before must not stay viewable for another full term.
  const maxAge = Math.max(0, Math.floor((expiresAt(object, env) - Date.now()) / 1000));
  const contentType = object.httpMetadata?.contentType;
  return new Response(object.body, {
    headers: {
      ...securityHeaders(),
      "Content-Type": ALLOWED_CONTENT_TYPES.has(contentType) ? contentType : "application/octet-stream",
      "Cache-Control": `public, max-age=${maxAge}, immutable`,
      "Content-Security-Policy": "default-src 'none'; sandbox",
      "X-Robots-Tag": "noindex, nofollow, noarchive",
    },
  });
}

function expiredPage() {
  return `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8" />
<meta name="viewport" content="width=device-width, initial-scale=1" />
<meta name="robots" content="noindex" />
<title>Link expired</title>
<style>
  body { margin: 0; min-height: 100vh; display: flex; align-items: center; justify-content: center;
    background: #151311; color: #e4e0db; font: 15px/1.5 -apple-system, "Segoe UI", sans-serif; }
  main { text-align: center; }
</style>
</head>
<body>
  <main>
    <p>This share link has expired or does not exist.</p>
  </main>
</body>
</html>`;
}

function securityHeaders() {
  return {
    "Strict-Transport-Security": "max-age=31536000; includeSubDomains",
    "X-Content-Type-Options": "nosniff",
    "X-Frame-Options": "DENY",
    "Referrer-Policy": "no-referrer",
    "Permissions-Policy": "camera=(), microphone=(), geolocation=(), payment=(), usb=()",
    "Cross-Origin-Resource-Policy": "same-origin",
  };
}

function json(value, status = 200) {
  return new Response(JSON.stringify(value), {
    status,
    headers: {
      ...securityHeaders(),
      "Content-Type": "application/json; charset=utf-8",
      "Cache-Control": "no-store",
      "Content-Security-Policy": "default-src 'none'; frame-ancestors 'none'; sandbox",
    },
  });
}

function html(body, status = 200) {
  return new Response(body, {
    status,
    headers: {
      ...securityHeaders(),
      "Content-Type": "text/html; charset=utf-8",
      "Cache-Control": "no-store",
      "Content-Security-Policy":
        "default-src 'none'; img-src 'self'; media-src 'self'; style-src 'unsafe-inline'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'",
      "X-Robots-Tag": "noindex, nofollow, noarchive",
    },
  });
}
