# athena docs

The documentation site for athena, built with
[Fumadocs](https://fumadocs.dev) on Next.js as a static export.

    npm install
    npm run dev            # http://localhost:3000
    npm run build          # static site in out/
    npm run check-links    # after build: every internal link resolves
    npm run types:check

Pages are MDX files in `content/docs/`. The sidebar order and sections are
in `content/docs/meta.json`; a new page must be added there. Site name and
GitHub links are in `lib/shared.ts`, the landing page is `app/(home)/page.tsx`.

The content follows the repository's README.md and TESTING.md. When a
behaviour changes, update both.

CI (`.github/workflows/docs.yml`) runs the typecheck, the build and the link
check on every pull request that touches `docs/`.
