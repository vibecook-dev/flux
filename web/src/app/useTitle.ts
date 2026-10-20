import { useEffect } from 'react';

/** Set the document title for the current screen ("Arrival · flux"). */
export function useTitle(title: string | null | undefined): void {
  useEffect(() => {
    document.title = title ? `${title} · flux` : 'flux';
  }, [title]);
}
