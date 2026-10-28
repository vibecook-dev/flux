/**
 * flux's live events, shown where you are instead of on an admin page: a scan in
 * progress becomes a ring in the island, new titles arrive with a short note
 * (and slide into the lens), a drive that stops answering says so, and so does
 * remote access when it changes. Lists refresh themselves.
 */
import { useEffect } from 'react';
import { navigate } from '../app/router';
import { subscribeEvents } from '../api/events';
import { formatCount } from '../api/format';
import { refreshFolders } from '../api/folder-list';
import { refreshCollections, refreshLibrary } from '../api/library-list';
import { keys, recallSummary } from '../api/queries';
import { invalidateQueries, peekQuery, setQueryData } from '../api/query';
import type { Id, Library, TailnetInfo } from '../api/types';
import { applyScanProgress } from '../routes/admin-state';
import { currentRing, postActivity, setRing } from './live';
import { spacePath } from './space';

const plural = (n: number, one: string, many: string) => `${formatCount(n)} ${n === 1 ? one : many}`;

function nounFor(lib: Library | undefined): [string, string] {
  if (lib?.kind === 'shows') return ['show', 'shows'];
  if (lib?.kind === 'videos') return ['video', 'videos'];
  if (lib?.kind === 'mixed') return ['title', 'titles'];
  return ['film', 'films'];
}

export function useEventsBridge(): void {
  useEffect(() => {
    const added = new Map<Id, Id[]>();
    const timers = new Map<Id, ReturnType<typeof setTimeout>>();
    let lastTailnet: TailnetInfo['state'] | null = peekQuery<TailnetInfo>(keys.tailnet)?.state ?? null;

    const libsNow = () => peekQuery<Library[]>(keys.libraries);

    const flushAdded = (libraryId: Id) => {
      timers.delete(libraryId);
      const ids = added.get(libraryId) ?? [];
      added.delete(libraryId);
      refreshLibrary(libraryId);
      refreshFolders(libraryId);
      invalidateQueries(keys.home);
      if (ids.length === 0) return;
      const lib = libsNow()?.find((l) => l.id === libraryId);
      const [one, many] = nounFor(lib);
      // Posters arrive once the refreshed list has the summaries; try now and post either way.
      setTimeout(() => {
        const thumbs = ids
          .map((id) => recallSummary(id)?.poster ?? null)
          .filter(Boolean)
          .slice(0, 3);
        const titles = ids
          .map((id) => recallSummary(id)?.title)
          .filter(Boolean)
          .slice(0, 2)
          .join(', ');
        postActivity({
          id: `added:${libraryId}`,
          icon: 'sparkle',
          title: `${plural(ids.length, `new ${one}`, `new ${many}`)}${lib ? ` in ${lib.name}` : ''}`,
          detail: titles || 'Added by the last scan',
          thumbs,
          action: { label: 'Show', run: () => navigate(`${spacePath(libraryId)}?sort=-added`) },
          ms: 6000,
        });
      }, 900);
    };

    const unsub = subscribeEvents((e) => {
      switch (e.type) {
        case 'scan_progress': {
          const { libs, name } = applyScanProgress(libsNow(), e.library_id, e.status);
          if (libsNow()) setQueryData(keys.libraries, libs);
          const s = e.status;
          if (s.state === 'scanning') {
            const total = s.files_total ?? s.files_seen;
            setRing({
              kind: 'scanning',
              progress: s.files_total ? Math.min(1, s.files_probed / Math.max(1, s.files_total)) : null,
              label: total ? `${formatCount(s.files_probed)} / ${formatCount(total)}` : 'Scanning',
              aria: `Scanning ${name ?? 'a library'}: open Libraries & scanning`,
              run: () => navigate('/admin'),
            });
          } else if (currentRing()?.kind === 'scanning') {
            setRing(null);
            if (s.state === 'error' && s.last_error) {
              postActivity({ id: `scan-error:${e.library_id}`, icon: 'warning', title: `${name ?? 'A library'} didn’t finish scanning`, detail: s.last_error, action: { label: 'Open', run: () => navigate('/admin') } });
            }
          }
          break;
        }
        case 'items_changed': {
          // An agent's edit (or a match) renamed or filled in these: open pages refetch.
          for (const id of e.updated) {
            invalidateQueries(keys.item(id));
            invalidateQueries(keys.facts(id));
          }
          if (e.added.length) added.set(e.library_id, [...(added.get(e.library_id) ?? []), ...e.added]);
          if (!timers.has(e.library_id)) timers.set(e.library_id, setTimeout(() => flushAdded(e.library_id), 1500));
          break;
        }
        case 'source_availability': {
          invalidateQueries(keys.libraries);
          const lib = libsNow()?.find((l) => l.id === e.library_id);
          const where = e.root.split(/[\\/]/).filter(Boolean).pop() ?? e.root;
          postActivity(
            e.available
              ? { id: `root:${e.root}`, icon: 'check', title: `${where} is back`, detail: `${lib?.name ?? 'Its titles'} can play again.` }
              : { id: `root:${e.root}`, icon: 'archive', title: `${where} isn’t answering`, detail: `${lib?.name ?? 'Its titles'} stay here and play again when it’s back. Nothing was removed.`, ms: 7000 },
          );
          refreshLibrary(e.library_id);
          break;
        }
        case 'changes': {
          invalidateQueries(keys.changes);
          const cs = e.changeset;
          const n = plural(cs.change_count, 'change', 'changes');
          const review = { label: 'Review', run: () => navigate(`/changes?open=${cs.id}`) };
          // An agent's own work, said where you are; a person's decisions stay quiet.
          if (cs.state === 'applied' && cs.agent && cs.decided_by === cs.by) {
            postActivity({ id: `changes:${cs.id}`, icon: 'agent', title: `${cs.agent} made ${n}`, detail: cs.summary, action: review, ms: 7000 });
          } else if (cs.state === 'proposed') {
            postActivity({ id: `changes:${cs.id}`, icon: 'agent', title: `${cs.agent ?? cs.by} suggests ${n}`, detail: cs.summary, action: review, ms: 8000 });
          }
          break;
        }
        case 'lenses': {
          invalidateQueries(keys.serverLenses);
          break;
        }
        case 'collections': {
          invalidateQueries(keys.collections);
          refreshCollections();
          break;
        }
        case 'tailnet': {
          setQueryData(keys.tailnet, e.info);
          const state = e.info.state;
          if (state !== lastTailnet) {
            if (state === 'running') postActivity({ id: 'tailnet', icon: 'tailscale', title: 'Remote access is on', detail: e.info.url ?? e.info.dns_name ?? 'Your tailnet can reach flux.' });
            else if (state === 'needs_login') postActivity({ id: 'tailnet', icon: 'tailscale', title: 'Remote access needs a sign-in', detail: 'Finish signing in to Tailscale.', action: { label: 'Open', run: () => navigate('/connect') } });
            else if (state === 'error' && e.info.last_error) postActivity({ id: 'tailnet', icon: 'warning', title: 'Remote access stopped', detail: e.info.last_error, action: { label: 'Open', run: () => navigate('/connect') } });
          }
          lastTailnet = state;
          break;
        }
        case 'update': {
          // The Libraries screen's update panel and the owner's note follow this.
          setQueryData(keys.update, e.info);
          break;
        }
      }
    });
    return () => {
      unsub();
      for (const t of timers.values()) clearTimeout(t);
    };
  }, []);
}
