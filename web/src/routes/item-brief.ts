/** Helpers of the item page's About panel (ItemAbout.tsx). */
import type { AgentItemDetail } from '../api/agent';
import type { ItemDetail } from '../api/types';

/** `2001-07-20` → "20 July 2001"; `2001-07` → "July 2001"; `2001` stays. */
export function formatRelease(d: string): string {
  const [y, m, day] = d.split('-').map(Number);
  if (!m) return String(y);
  const date = new Date(Date.UTC(y, m - 1, day || 1));
  return new Intl.DateTimeFormat('en-GB', day ? { day: 'numeric', month: 'long', year: 'numeric', timeZone: 'UTC' } : { month: 'long', year: 'numeric', timeZone: 'UTC' }).format(date);
}


export function agentBrief(item: ItemDetail, facts: AgentItemDetail | undefined, origin: string): string {
  const what = `“${item.title}”${item.year ? ` (${item.year})` : ''}`;
  const file = facts?.files[0]?.path ?? item.assets[0]?.path ?? null;
  return `Help me fix ${what} in my flux library (item ${item.id}).

Look it up with flux: \`flux get ${item.id}\` (or the MCP tool item_get with item ${item.id}) shows every field, where each came from, and what its files say: path, duration, languages, the title stored inside the file.

Then research the open web, check what you find against the file (a runtime within a few minutes, the audio language), and fill in what's missing or wrong: what it is, its year, the original title, a short description in your own words, directors, cast, languages, countries, links. Cite every page you use (--source), write a one-line --summary, and if you aren't sure, --propose instead of applying. Never use TMDB's text. The flux-metadata skill has the details: \`flux skills show flux-metadata\`.

Server: ${origin}${file ? `\nFile: ${file}` : ''}`;
}

