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
check on every pull request that touches `docs/`. A push to `main` also
deploys `out/` to GitHub Pages at https://queryplanner.github.io/athena/.

CI builds with `DOCS_BASE_PATH=/athena`, because Pages serves the site under
the repository name. Local builds leave it unset and serve from `/`. To try
the Pages build locally:

    DOCS_BASE_PATH=/athena npm run build && DOCS_BASE_PATH=/athena npm run check-links

`DOCS_SITE_URL` sets the absolute base for Open Graph image URLs.
