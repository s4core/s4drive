# Third-party browser assets

These files are copied byte-for-byte from the pinned npm packages in package-lock.json.
They run locally without a CDN; Notes integrates them in a later phase. No frontend build is required.

## Assets

| Package | Version | License | Source | Asset | SHA-256 |
|---|---|---|---|---|---|
| markdown-it | 15.0.2 | MIT | [markdown-it](https://github.com/markdown-it/markdown-it/tree/15.0.2) | `markdown-it.min.js` | `635972b985228e8af9f0143647c68616b7a3bb09f6946e7e4a52e43dcf5e7be5` |
| dompurify | 3.4.16 | (MPL-2.0 OR Apache-2.0) | [dompurify](https://github.com/cure53/DOMPurify/tree/3.4.16) | `purify.min.js` | `2c90a9b46d6463f26038a29b686e82bc91de01fdac9d5229e7cfe3b360134ea2` |

## Reproduction and review

Run `npm ci --ignore-scripts`, then copy the following files without modification:

- `node_modules/markdown-it/dist/browser/markdown-it.umd.min.js` → `markdown-it.min.js`.
- `node_modules/dompurify/dist/purify.min.js` → `purify.min.js`.
- The package LICENSE files and DOMPurify LICENSE-MPL are included alongside the assets.

Verify with `sha256sum src-tauri/dist/vendor/*.min.js`. Updating a package requires updating its browser file, licenses, version and hash together.

Reviewed 2026-10-05: the upstream security pages and npm audit for this exact lockfile.
[markdown-it security](https://github.com/markdown-it/markdown-it/security), [DOMPurify advisories](https://github.com/cure53/DOMPurify/security/advisories).
The installation audit reported zero known vulnerabilities. This records the review date, not a guarantee against future advisories.
DOMPurify offers Apache-2.0 or MPL-2.0; redistribution here uses Apache-2.0 and includes both upstream license files.
Minified browser files retain their upstream license banners and source-map comments; source maps are development aids and are not shipped.
