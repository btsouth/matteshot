# Matteshot share server

A Cloudflare Worker that stores Matteshot uploads in R2 and serves them at short, expiring links. Builds of Matteshot made with `--features share` upload to it with an operator-issued token.

Setup, configuration and the API are in [docs/self-hosting-share.md](../docs/self-hosting-share.md).

```bash
npm ci
npm test
```

Licensed under MIT OR Apache-2.0, like the rest of Matteshot.
