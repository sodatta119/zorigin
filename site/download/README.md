# Put installers here

The landing page's download buttons link to files in this folder. Place the
built installers here (these exact names, or edit the `href`s in `site/index.html`):

- `zap-macos.dmg`   — universal mac (`./scripts/build-dist.sh` → `dist/zap-macos.dmg`)
- `zap-windows.zip`  — from a tagged CI release (`.github/workflows/release.yml`)
- `zap-linux.deb`    — from a tagged CI release

The binaries themselves are git-ignored (build artifacts) — you copy them in at
deploy time. When you deploy `site/` to Cloudflare Pages / Netlify, drop the
installers into this folder first so the buttons resolve.
