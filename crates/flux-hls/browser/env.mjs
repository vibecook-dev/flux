// Locations shared by the browser and AVPlayer harnesses, from the
// environment (nothing machine-specific is written in the scripts):
//
//   FLUX_TEST_MEDIA        the media corpus (features/, playback-research/, …);
//                          a harness that needs it skips when it is unset
//   FLUX_BENCH_RESULTS     where results JSON goes (else printed, not written)
//   CARGO_TARGET_DIR       where hls-serve was built (else <repo>/target)
//   FLUX_WEB_NODE_MODULES  node_modules with hls.js and playwright-core
//                          (else <repo>/web/node_modules)
//   FLUX_TEST_CLIPS        clips made by flux-test-media (else
//                          <tmp>/flux-test-clips-v1, its default)
//   FLUX_AVCHECK           the compiled AVPlayer checker (macOS); the
//                          AVPlayer checks skip when it is unset

import { writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
export const REPO = path.resolve(here, "../../..");
export const MEDIA = process.env.FLUX_TEST_MEDIA || null;
export const RESULTS = process.env.FLUX_BENCH_RESULTS || null;
export const TARGET = process.env.CARGO_TARGET_DIR || path.join(REPO, "target");
export const NODE_MODULES = process.env.FLUX_WEB_NODE_MODULES || path.join(REPO, "web/node_modules");
export const CLIPS = process.env.FLUX_TEST_CLIPS || path.join(os.tmpdir(), "flux-test-clips-v1");
export const AVCHECK = process.env.FLUX_AVCHECK || null;
export const TMP = os.tmpdir();

/** The corpus directory, or exits (successfully) saying why the run skips. */
export function requireMedia() {
  if (!MEDIA) {
    console.error("skipping: FLUX_TEST_MEDIA is not set");
    process.exit(0);
  }
  return MEDIA;
}

/** `explicit`, else `$FLUX_BENCH_RESULTS/<name>`, else null. */
export function resultsPath(name, explicit) {
  if (explicit) return explicit;
  return RESULTS ? path.join(RESULTS, name) : null;
}

/** Writes `data` as JSON to `file`, or prints it when there is no file. */
export function writeResults(file, data) {
  const text = JSON.stringify(data, null, 2);
  if (!file) {
    console.log(text);
    console.error("results not written (pass --out FILE or set FLUX_BENCH_RESULTS)");
    return;
  }
  writeFileSync(file, text);
}
