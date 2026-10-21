#!/usr/bin/env node
// Prints the licences of the npm packages whose code is in the web client
// build, with each package's licence file, for THIRD_PARTY_LICENSES: the
// production dependencies of web/package.json (transitive ones included) and
// the build tools listed below.
//
//   node scripts/package/web-licenses.mjs [web-dir] > web-licenses.txt
//
// Needs `pnpm install` in web-dir. Exits non-zero if a package's licence is
// not on the list below (GPL, AGPL, LGPL, SSPL, unknown, …), or if a package
// ships neither a licence file nor a copyright notice and has no entry in
// REPOSITORY_LICENSES.
import { execFileSync } from 'node:child_process';
import { existsSync, readdirSync, readFileSync, realpathSync } from 'node:fs';
import { dirname, join, relative, resolve } from 'node:path';

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
const LICENSE_FILE = /^(third[-_]party[-_])?(licen[cs]es?|copying|notice)([-._].*)?$/i;

// Packages that are not production dependencies but put code into the build,
// and what they put there. `from`: the package that depends on it. `when`: a
// file in web-dir without which the package adds nothing.
const BUILD_TOOLS = [
  { name: 'vite', what: 'its module preload polyfill and preload helper, in the entry chunk' },
  { name: 'rolldown', from: 'vite', what: 'its runtime helper that wraps CommonJS modules (React)' },
  {
    name: 'msw',
    what: 'public/mockServiceWorker.js, which the build copies',
    when: 'public/mockServiceWorker.js',
  },
];
// Where a licence file goes on to cover code that never reaches the build:
// vite's LICENSE.md lists the dependencies bundled into vite's own tooling.
const LICENSE_ENDS = { vite: '\n# Licenses of bundled dependencies' };

// Packages that ship no licence file and no copyright notice in their code:
// the licence file of their repository, at the commit the version was
// published from. Keyed by version, so that an upgrade is checked again.
const REPOSITORY_LICENSES = {
  'blurhash@2.0.5': {
    url: 'https://github.com/woltapp/blurhash/blob/cb151cab5b7d9cd3eef624e12e30381c6d292f0d/License.md',
    text: `MIT License

Copyright (c) 2018 Wolt Enterprises

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.`,
  },
};

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

const allowed = (license) =>
  license
    .replace(/[()]/g, ' ')
    .split(/\s+OR\s+/)
    .some((alt) => alt.split(/\s+AND\s+/).every((id) => ALLOWED.has(id.trim())));

const packages = [];
const rejected = [];
for (const [license, list] of Object.entries(byLicense)) {
  for (const pkg of list) {
    if (!allowed(license)) rejected.push(`${pkg.name}@${(pkg.versions ?? []).join(',')}: ${license}`);
    const paths = pkg.paths ?? (pkg.path ? [pkg.path] : []);
    (pkg.versions ?? ['?']).forEach((version, i) => {
      packages.push({ name: pkg.name, version, license, homepage: pkg.homepage, dir: paths[i] ?? paths[0] });
    });
  }
}

// The directory of package `name` as Node resolves it from `dir`.
function findPackage(name, dir) {
  for (let d = dir; ; d = dirname(d)) {
    const candidate = join(d, 'node_modules', name);
    if (existsSync(join(candidate, 'package.json'))) return realpathSync(candidate);
    if (dirname(d) === d) return null;
  }
}
const found = new Map();
for (const tool of BUILD_TOOLS) {
  if (tool.when && !existsSync(join(webDir, tool.when))) continue;
  const dir = findPackage(tool.name, tool.from ? found.get(tool.from) : webDir);
  if (!dir) {
    console.error(`web-licenses: ${tool.name} is not installed in ${webDir}`);
    process.exit(1);
  }
  found.set(tool.name, dir);
  const pkg = JSON.parse(readFileSync(join(dir, 'package.json'), 'utf8'));
  const license = typeof pkg.license === 'string' ? pkg.license : 'unknown';
  if (!allowed(license)) rejected.push(`${pkg.name}@${pkg.version}: ${license}`);
  packages.push({ name: pkg.name, version: pkg.version, license, homepage: pkg.homepage, dir, what: tool.what });
}
if (rejected.length) {
  console.error(`web-licenses: licences not allowed for the flux web bundle:\n  ${rejected.join('\n  ')}`);
  process.exit(1);
}

// The file a bundler takes for `import 'name'`.
function entryFile(dir) {
  const pkg = JSON.parse(readFileSync(join(dir, 'package.json'), 'utf8'));
  const pick = (e) => {
    if (typeof e === 'string') return e;
    if (!e || typeof e !== 'object') return undefined;
    for (const key of ['.', 'browser', 'import', 'module', 'default']) {
      const file = pick(e[key]);
      if (file) return file;
    }
    return undefined;
  };
  return join(dir, pick(pkg.exports) ?? pkg.module ?? pkg.main ?? 'index.js');
}

// The comment at the top of a file (one /* */ block, or consecutive // lines)
// if it holds a copyright notice.
function copyrightComment(file) {
  let text;
  try {
    text = readFileSync(file, 'utf8').replace(/\r\n/g, '\n').trimStart();
  } catch {
    return null;
  }
  const comment = text.match(/^\/\*[\s\S]*?\*\//)?.[0] ?? text.match(/^(?:[ \t]*\/\/[^\n]*\n)+/)?.[0];
  return comment && /copyright|\(c\)|©/i.test(comment) ? comment.trimEnd() : null;
}

packages.sort((a, b) => a.name.localeCompare(b.name) || a.version.localeCompare(b.version));

const rule = '-'.repeat(80);
const out = [];
const unlicensed = [];
for (const p of packages) {
  const head = `${p.name} ${p.version} (${p.license})${p.homepage ? `\n${p.homepage}` : ''}`;
  out.push(rule, head, ...(p.what ? [`Not a production dependency, but in the build: ${p.what}.`] : []), rule, '');
  let files = [];
  try {
    files = readdirSync(p.dir, { withFileTypes: true })
      .filter((f) => f.isFile() && LICENSE_FILE.test(f.name))
      .map((f) => f.name)
      .sort();
  } catch {
    // no directory: fall through to the notices below
  }
  for (const f of files) {
    let text = readFileSync(join(p.dir, f), 'utf8').replace(/\r\n/g, '\n');
    const end = LICENSE_ENDS[p.name] && text.indexOf(LICENSE_ENDS[p.name]);
    if (end > 0) text = text.slice(0, end);
    out.push(text.trimEnd(), '');
  }
  if (files.length > 0) continue;
  const entry = entryFile(p.dir);
  const comment = copyrightComment(entry);
  const repository = REPOSITORY_LICENSES[`${p.name}@${p.version}`];
  if (comment) {
    out.push(`The package ships no licence file. The notice at the top of ${relative(p.dir, entry)}:`, '', comment, '');
  } else if (repository) {
    out.push(
      'The package ships no licence file or copyright notice. The licence file of its',
      `repository, at the commit this version was published from (${repository.url}):`,
      '',
      repository.text.trimEnd(),
      '',
    );
  } else {
    unlicensed.push(`${p.name}@${p.version} (${p.license})`);
  }
}
if (unlicensed.length) {
  console.error(
    'web-licenses: these packages ship no licence file or copyright notice; find the licence\n' +
      `in their repository and add it to REPOSITORY_LICENSES:\n  ${unlicensed.join('\n  ')}`,
  );
  process.exit(1);
}
process.stdout.write(out.join('\n') + '\n');
