#!/usr/bin/env node
// Prints the licences of the npm packages bundled into the web client (the
// production dependencies of web/package.json, transitive ones included),
// with each package's licence file, for THIRD_PARTY_LICENSES.
//
//   node scripts/package/web-licenses.mjs [web-dir] > web-licenses.txt
//
// Needs `pnpm install` in web-dir. Exits non-zero if a package's licence is
// not on the list below (GPL, AGPL, LGPL, SSPL, unknown, …).
import { execFileSync } from 'node:child_process';
import { readdirSync, readFileSync } from 'node:fs';
import { join, resolve } from 'node:path';

const ALLOWED = new Set([
  'MIT',
  'ISC',
  'Apache-2.0',
  'BSD-2-Clause',
  'BSD-3-Clause',
  '0BSD',
  'CC0-1.0',
  'MPL-2.0',
  'Unlicense',
  'BlueOak-1.0.0',
  // Fonts (Archivo, Atkinson Hyperlegible): SIL Open Font License.
  'OFL-1.1',
]);
const LICENSE_FILE = /^(licen[cs]e|copying|notice)([-._].*)?$/i;

const webDir = resolve(process.argv[2] ?? 'web');
// `pnpm licenses` reads the package store; use the one node_modules was
// installed from (it may not be the default, e.g. `pnpm install --store-dir`).
const args = ['licenses', 'list', '--prod', '--json'];
try {
  const modules = readFileSync(join(webDir, 'node_modules', '.modules.yaml'), 'utf8');
  const storeDir = modules.match(/^\s*"?storeDir"?\s*:\s*"?([^"\n]+?)"?,?\s*$/m)?.[1];
  if (storeDir) args.push(`--config.store-dir=${storeDir}`);
} catch {
  // no node_modules/.modules.yaml: let pnpm report it
}
const json = execFileSync('pnpm', args, {
  cwd: webDir,
  encoding: 'utf8',
  maxBuffer: 64 << 20,
});
const byLicense = JSON.parse(json);

const packages = [];
const rejected = [];
for (const [license, list] of Object.entries(byLicense)) {
  for (const pkg of list) {
    const ok = license
      .replace(/[()]/g, ' ')
      .split(/\s+OR\s+/)
      .some((alt) => alt.split(/\s+AND\s+/).every((id) => ALLOWED.has(id.trim())));
    if (!ok) rejected.push(`${pkg.name}@${(pkg.versions ?? []).join(',')}: ${license}`);
    const paths = pkg.paths ?? (pkg.path ? [pkg.path] : []);
    (pkg.versions ?? ['?']).forEach((version, i) => {
      packages.push({ name: pkg.name, version, license, homepage: pkg.homepage, dir: paths[i] ?? paths[0] });
    });
  }
}
if (rejected.length) {
  console.error(`web-licenses: licences not allowed for the flux web bundle:\n  ${rejected.join('\n  ')}`);
  process.exit(1);
}

packages.sort((a, b) => a.name.localeCompare(b.name) || a.version.localeCompare(b.version));

const rule = '-'.repeat(80);
const out = [];
for (const p of packages) {
  out.push(rule, `${p.name} ${p.version} (${p.license})${p.homepage ? `\n${p.homepage}` : ''}`, rule, '');
  let files = [];
  try {
    files = readdirSync(p.dir).filter((f) => LICENSE_FILE.test(f)).sort();
  } catch {
    // no directory: fall through to the note below
  }
  if (files.length === 0) {
    out.push(`Licensed under ${p.license}. The package ships no licence file.`, '');
  }
  for (const f of files) {
    out.push(readFileSync(join(p.dir, f), 'utf8').replace(/\r\n/g, '\n').trimEnd(), '');
  }
}
process.stdout.write(out.join('\n') + '\n');
