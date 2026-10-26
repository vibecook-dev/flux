#!/usr/bin/env node
// Publishes the built site (dist/) to the gh-pages branch, which GitHub Pages
// serves. The branch holds one commit, the site as built, replaced each time.
// Build it for its address first:
//
//   SITE_BASE=/flux/ SITE_URL=https://vibecook-dev.github.io/flux/ pnpm build
//   node scripts/publish-pages.mjs [--remote URL] [--message TEXT]
//
// The remote defaults to the repository's origin. Git's identity comes from
// your git config or the GIT_AUTHOR_* / GIT_COMMITTER_* variables.
import { spawnSync } from 'node:child_process';
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const site = join(dirname(fileURLToPath(import.meta.url)), '..');
const dist = join(site, 'dist');
const arg = (name) => {
  const i = process.argv.indexOf(name);
  return i > 0 ? process.argv[i + 1] : undefined;
};

function run(cmd, args, opts = {}) {
  const r = spawnSync(cmd, args, { encoding: 'utf8', ...opts });
  if (r.status !== 0) {
    console.error(`${cmd} ${args.join(' ')} failed:\n${r.stderr || r.stdout}`);
    process.exit(1);
  }
  return r.stdout.trim();
}

const page = join(dist, 'index.html');
if (!existsSync(page) || !existsSync(join(dist, 'demo', 'index.html'))) {
  console.error('dist/ has no built site (with its app in dist/demo/). Run pnpm build first.');
  process.exit(1);
}
// A site built for / doesn't work at /flux/ (and the other way round): say which base it has.
const base = /src="([^"]*)assets\//.exec(readFileSync(page, 'utf8'))?.[1] ?? '?';
console.log(`Publishing dist/, built for ${base}`);

const remote = arg('--remote') ?? run('git', ['remote', 'get-url', 'origin'], { cwd: site });
const source = run('git', ['rev-parse', '--short', 'HEAD'], { cwd: site });
const message = arg('--message') ?? `Publish the website from ${source}`;

// GitHub Pages runs Jekyll on a branch unless told not to; the site is already built.
writeFileSync(join(dist, '.nojekyll'), '');
const tmp = mkdtempSync(join(tmpdir(), 'flux-pages-'));
try {
  const git = (...args) => run('git', ['--git-dir', join(tmp, '.git'), '--work-tree', dist, ...args]);
  run('git', ['init', '-q', '-b', 'gh-pages', tmp]);
  git('add', '-A');
  git('commit', '-q', '-m', message);
  git('push', '-q', '--force', remote, 'gh-pages:gh-pages');
  console.log(`Pushed gh-pages (${git('rev-parse', '--short', 'HEAD')}) from ${source}`);
} finally {
  rmSync(tmp, { recursive: true, force: true });
}
