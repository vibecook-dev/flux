// Where the dev scripts find the test corpus and put their output. Nothing
// here defaults to a particular machine's paths (see CONTRIBUTING.md):
//
//   FLUX_WORK        scratch directory: bench/results/, screenshots, temp files
//   FLUX_TEST_MEDIA  the test corpus (features/, library/, …); default $FLUX_WORK/media
//
// Each helper exits with a clear message when the variable it needs is unset,
// and only when a script actually needs it.
import { join } from 'node:path';

function fail(message) {
  console.error(`${message} (see CONTRIBUTING.md)`);
  process.exit(2);
}

/** $FLUX_WORK, or exit with a message saying what it is for. */
export function fluxWork(purpose = 'results and scratch files') {
  const v = process.env.FLUX_WORK;
  if (!v) fail(`FLUX_WORK is not set: point it at a scratch directory for ${purpose}`);
  return v;
}

/** $FLUX_TEST_MEDIA, else $FLUX_WORK/media, or exit. */
export function testMedia() {
  const v = process.env.FLUX_TEST_MEDIA;
  if (v) return v;
  if (process.env.FLUX_WORK) return join(process.env.FLUX_WORK, 'media');
  return fail('FLUX_TEST_MEDIA is not set: point it at the test corpus directory');
}

/** $FLUX_WORK/bench/results/<name>. */
export function resultFile(name) {
  return join(fluxWork('bench results'), 'bench', 'results', name);
}
