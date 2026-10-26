#!/usr/bin/env node
// Builds the web client (../web) in mock mode as the site's live demo: the
// real app, with the in-browser mock backend and its demo library.
//
//   node scripts/build-demo.mjs [--out DIR]     (default: .demo, which `pnpm dev` serves)
//
// The demo's base is <SITE_BASE>demo/ so the app's router and its mock
// service worker work under the site. Needs ../web's dependencies (installed
// here when missing).
import { spawnSync } from 'node:child_process';
import { existsSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const site = join(here, '..');
const web = join(site, '..', 'web');
const i = process.argv.indexOf('--out');
const out = resolve(site, i > 0 ? process.argv[i + 1] : '.demo');
const base = `${process.env.SITE_BASE ?? '/'}demo/`;

// Run pnpm the way this script was started (pnpm sets npm_execpath), else from PATH.
function pnpm(args, cwd, env = process.env) {
  const viaNpmExec = process.env.npm_execpath && /pnpm/.test(process.env.npm_execpath);
  const [cmd, argv] = viaNpmExec ? [process.execPath, [process.env.npm_execpath, ...args]] : ['pnpm', args];
  const r = spawnSync(cmd, argv, { cwd, env, stdio: 'inherit' });
  if (r.error || r.status !== 0) {
    console.error(`pnpm ${args.join(' ')} failed in ${cwd}${r.error ? `: ${r.error.message}` : ''}`);
    process.exit(r.status || 1);
  }
}

if (!existsSync(join(web, 'node_modules'))) pnpm(['install', '--frozen-lockfile'], web);
console.log(`Building the demo (web/, mock backend) for ${base} into ${out}`);
pnpm(['exec', 'vite', 'build', '--base', base, '--outDir', out, '--emptyOutDir'], web, { ...process.env, VITE_FLUX_API: 'mock' });
