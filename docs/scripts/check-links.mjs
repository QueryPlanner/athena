// Fails if any internal link in the static export points at a page or
// heading that does not exist. Run after `next build`.
import { existsSync, readFileSync, readdirSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

const OUT = fileURLToPath(new URL('../out/', import.meta.url));
// Links in a build with a basePath start with it; the files in out/ do not.
const BASE_PATH = process.env.DOCS_BASE_PATH ?? '';

function htmlFiles(dir) {
  return readdirSync(dir, { withFileTypes: true }).flatMap((entry) => {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) return htmlFiles(path);
    return entry.name.endsWith('.html') ? [path] : [];
  });
}

// "/docs/cli" is exported as out/docs/cli.html, "/" as out/index.html.
function pageFor(pathname) {
  const clean = pathname.replace(/\/$/, '');
  const candidates = [
    join(OUT, clean, 'index.html'),
    join(OUT, `${clean}.html`),
    join(OUT, clean),
  ];
  return candidates.find((file) => existsSync(file) && file.endsWith('.html'))
    ?? candidates.find((file) => existsSync(file));
}

function hasAnchor(file, id) {
  return readFileSync(file, 'utf8').includes(`id="${id}"`);
}

const broken = [];
for (const file of htmlFiles(OUT)) {
  const html = readFileSync(file, 'utf8');
  for (const [, href] of html.matchAll(/href="(\/[^"]*)"/g)) {
    if (href.startsWith('//') || href.startsWith('/_next/')) continue;
    const [withQuery, rawAnchor] = href.split('#');
    const withBase = withQuery.split('?')[0];
    if (BASE_PATH && !withBase.startsWith(`${BASE_PATH}/`) && withBase !== BASE_PATH) {
      broken.push(`${file.slice(OUT.length)} -> ${href} (missing base path ${BASE_PATH})`);
      continue;
    }
    const pathname = withBase.slice(BASE_PATH.length) || '/';
    const anchor = rawAnchor && decodeURIComponent(rawAnchor);
    const target = pageFor(decodeURIComponent(pathname));
    if (!target) broken.push(`${file.slice(OUT.length)} -> ${href} (no page)`);
    else if (anchor && target.endsWith('.html') && !hasAnchor(target, anchor)) {
      broken.push(`${file.slice(OUT.length)} -> ${href} (no heading)`);
    }
  }
}

if (broken.length > 0) {
  console.error(`Broken links:\n${[...new Set(broken)].join('\n')}`);
  process.exit(1);
}
console.log('All internal links resolve.');
