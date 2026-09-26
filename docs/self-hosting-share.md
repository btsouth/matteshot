# Self-hosting Share

Share uploads a screenshot or recording and copies a short link to it. It is not part of the default Matteshot build, and there is no public Matteshot upload service. Running an open upload service means answering for whatever strangers put on it, so Share is something you run for yourself or your team, on your own Cloudflare account, with tokens you hand out.

What you get:

- a Cloudflare Worker (`share-server/`) and an R2 bucket you own
- uploads accepted only with a token you issued, rate limited per client address and per token
- only PNG screenshots and MP4 recordings, checked by their file header as well as their declared type, up to 100 MB
- unlisted links (`https://<your host>/s/<12 characters>`) that are never indexed and stop working after 30 days, plus a way to take one down early
- no accounts, no analytics, and no request logs that contain link ids

## 1. Deploy the server

You need a Cloudflare account (the Free plan works) and Node.js 20 or newer.

```bash
cd share-server
npm ci
npx wrangler login
npx wrangler r2 bucket create matteshot-shares
npx wrangler r2 bucket lifecycle add matteshot-shares --id expire-shares --expire-days 30 --prefix ""
```

The lifecycle rule is what actually deletes old uploads from the bucket. The server also refuses to serve anything older than `SHARE_TTL_DAYS`, so a bucket without the rule stops showing old uploads but keeps the bytes. Keep the two numbers the same.

Edit `wrangler.toml`: the Worker `name`, the `bucket_name` if you chose another, and a custom domain under `[[routes]]` if you want one. Then create the upload tokens and deploy:

```bash
# Long random tokens, one per person or machine, separated by commas.
node -e "console.log(require('crypto').randomBytes(24).toString('base64url'))"
npx wrangler secret put UPLOAD_TOKENS
npx wrangler deploy
```

Check it: `https://<your host>/health` should answer `{"ok":true,"service":"matteshot-share","storage":true,"uploads":true}`. It never shows the tokens.

With no `UPLOAD_TOKENS` set, the server accepts no uploads at all.

## 2. Build Matteshot with Share

```powershell
cargo build --release --features share
```

Build the installer from that binary if you want one (`ISCC.exe installer\matteshot.iss`). Official releases do not include Share, and the auto-updater would replace your build with an official one on the next release, so turn off **Install updates automatically** in Settings on machines running your build.

## 3. Point Matteshot at your server

Quit Matteshot from the tray, then add two keys to `%APPDATA%\matteshot\config.json`:

```json
{
  "share_server": "https://share.example.com",
  "share_token": "the-token-you-created-for-this-machine"
}
```

`share_server` must be a plain `https://host` or `https://host:port`, the same host the server answers on, with nothing after it. Start Matteshot again. Share now shows up in the picker (**S**), both editors, and History's right-click menu. The link is opened in your browser and copied to the clipboard.

Matteshot refuses a link that points anywhere other than your configured host, never follows redirects, and never sends the token anywhere but that host.

## Operating it

- **Revoke a token:** remove it from `UPLOAD_TOKENS` (`npx wrangler secret put UPLOAD_TOKENS` with the new list). Uploads made with it keep working until they expire.
- **Take a link down early:**

  ```bash
  curl -X DELETE -H "Authorization: Bearer <any valid token>" https://share.example.com/v1/share/<id>
  ```

- **Change retention:** set `SHARE_TTL_DAYS` in `wrangler.toml` and the lifecycle rule to the same number, then deploy.
- **Limits:** `SHARE_LIMITER` caps uploads per client address and `TOKEN_LIMITER` per token, both per minute and per Cloudflare location. `MAX_UPLOAD_BYTES` can lower the 100 MB cap.
- **Abuse:** anyone holding a link can see that one upload, so treat tokens like passwords. If a token leaks, revoke it and delete what it uploaded; every object records a short fingerprint of the token that uploaded it in its `token` metadata, which `npx wrangler r2 object get` shows.

## API

| Method | Path | Auth | Result |
|---|---|---|---|
| `POST` | `/v1/share` | `Authorization: Bearer <token>` | multipart form with one `file` field (`image/png` or `video/mp4`); answers `{"url": "https://<host>/s/<id>"}` |
| `DELETE` | `/v1/share/<id>` | `Authorization: Bearer <token>` | `{"deleted": true}` |
| `GET` | `/s/<id>` | none | preview page |
| `GET` | `/s/<id>/raw` | none | the original file |
| `GET` | `/health` | none | whether storage and uploads are configured |

Errors are JSON `{"error": "..."}` with 400 for a malformed upload, 401 for a missing or unknown token, 413 for a file that is too large, 415 for a type or header that does not match, 429 when rate limited, and 503 when storage or tokens are not configured.

## Tests

```bash
cd share-server
npm test
```
