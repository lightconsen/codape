# codape.com

The landing page for [Codape](../../README.md), served from
[GitHub Pages](https://pages.github.com/) at **https://codape.com**.

React 19 + Vite + Tailwind CSS v4, with shadcn/ui-style primitives over Radix.
The page is a static build; there is no server component and no analytics.

## Design

A pixel-art page in the spirit of a CRT readout — not a copy of any one site.

| | |
|---|---|
| **Field** | Near-black warm brown `#0c0a07` |
| **Ink** | Bone `#f3ead9` |
| **Accent** | Amber phosphor `#f2a93b` |
| **Secondary accent** | Copper `#8c6a2f` (structure only, never competes) |
| **Display font** | Silkscreen for labels and numerals, Pixelify Sans for headings |
| **Body font** | Geist; Geist Mono for anything the machine says |
| **Shape** | No rounded corners anywhere. The only shadow is a hard offset block. |

Two rules keep it readable: pixel type is for display and metadata only —
never for body copy — and motion steps rather than eases, so it never reads as
a smooth modern UI wearing a costume.

The page is three blocks: a hero with a replaying terminal session,
capabilities, and install. Nothing else — no third-party scripts, no fonts
fetched from a CDN at runtime, no analytics.

The mark is the repository's `logo.png`, reduced to a 56-cell grid and upscaled
with nearest-neighbour sampling. Regenerate it after the logo changes:

```bash
cd site && pnpm brand   # needs Pillow + numpy
```

That writes `public/brand/mark-*.png`, the favicons, `apple-touch-icon.png` and
the 1200×630 `og.png`.

## Develop

```bash
cd site
pnpm install
pnpm dev        # http://localhost:5173
pnpm build      # type-check, then build to dist/
pnpm lint
pnpm preview    # serve dist/ locally
```

`dev` and `build` first run `scripts/sync-install-script.mjs`, which copies
`../scripts/install.sh` to `public/install.sh`. That is what makes
`curl -fsSL https://codape.com/install.sh | sh` work: the site ships the
installer instead of pointing at raw.githubusercontent, and copies it at build
time so the two cannot drift. `public/install.sh` is generated, not committed.

## Deploy

`.github/workflows/pages.yml` builds `site/` and publishes `dist/` to GitHub
Pages on every push to `main` that touches `site/**`, plus on manual dispatch.

One-time repository setup:

1. **Settings → Pages → Build and deployment → Source: GitHub Actions.**
2. **Custom domain: `codape.com`** (the build also ships a `CNAME` file).
3. At the DNS provider (Cloudflare), point the apex at GitHub:

   | Type | Name | Value |
   |---|---|---|
   | A | `@` | `185.199.108.153` |
   | A | `@` | `185.199.109.153` |
   | A | `@` | `185.199.110.153` |
   | A | `@` | `185.199.111.153` |
   | AAAA | `@` | `2606:50c0:8000::153` |
   | AAAA | `@` | `2606:50c0:8001::153` |
   | AAAA | `@` | `2606:50c0:8002::153` |
   | AAAA | `@` | `2606:50c0:8003::153` |
   | CNAME | `www` | `lightconsen.github.io` |

   In Cloudflare these records must be **DNS only** (grey cloud) until the
   certificate is issued, or Pages cannot complete the ACME challenge. Turn
   proxying on afterwards if you want it.
4. Tick **Enforce HTTPS** once the certificate is ready.

`codape.com` is the apex, so the site builds for the root path — no `base`
override in `vite.config.ts`.
