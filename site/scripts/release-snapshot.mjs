#!/usr/bin/env node
// Saves the newest flux release (pre-releases included) to src/data/release.json.
// The page shows this snapshot at once and refreshes it from GitHub's API in the
// browser, so a stale snapshot only matters when that request fails.
//
//   node scripts/release-snapshot.mjs        (GITHUB_TOKEN is used when set)
import { writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const REPO = 'vibecook-dev/flux';
const out = join(dirname(fileURLToPath(import.meta.url)), '..', 'src', 'data', 'release.json');
const headers = { accept: 'application/vnd.github+json', 'user-agent': 'flux-site' };
if (process.env.GITHUB_TOKEN) headers.authorization = `Bearer ${process.env.GITHUB_TOKEN}`;

const res = await fetch(`https://api.github.com/repos/${REPO}/releases?per_page=10`, { headers });
if (!res.ok) {
  console.error(`GitHub answered ${res.status}; src/data/release.json is unchanged`);
  process.exit(1);
}
const release = (await res.json()).find((r) => !r.draft);
if (!release) {
  console.error('No published release; src/data/release.json is unchanged');
  process.exit(1);
}
const snapshot = {
  tag: release.tag_name,
  name: release.name,
  published_at: release.published_at,
  prerelease: release.prerelease,
  url: release.html_url,
  assets: release.assets.map((a) => ({ name: a.name, size: a.size, url: a.browser_download_url })),
};
writeFileSync(out, `${JSON.stringify(snapshot, null, 2)}\n`);
console.log(`Saved ${snapshot.tag} (${snapshot.assets.length} files) to ${out}`);
