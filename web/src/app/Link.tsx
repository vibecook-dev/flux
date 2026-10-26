/** An <a> that navigates client-side and preloads its route's code on hover/focus. */
import type { AnchorHTMLAttributes, MouseEvent, Ref } from 'react';
import { appHref } from './base';
import { navigateWhenLoaded, preloadRoute } from './router';

export interface LinkProps extends AnchorHTMLAttributes<HTMLAnchorElement> {
  to: string;
  /** Extra intent work (e.g. data prefetch) on hover/focus. */
  onIntent?: () => void;
  replace?: boolean;
  ref?: Ref<HTMLAnchorElement>;
}

export function Link({ to, onIntent, replace, onClick, onPointerEnter, onFocus, ...rest }: LinkProps) {
  const intent = () => {
    void preloadRoute(to);
    onIntent?.();
  };
  return (
    <a
      href={appHref(to)}
      {...rest}
      onPointerEnter={(e) => {
        intent();
        onPointerEnter?.(e);
      }}
      onFocus={(e) => {
        intent();
        onFocus?.(e);
      }}
      onClick={(e: MouseEvent<HTMLAnchorElement>) => {
        onClick?.(e);
        if (e.defaultPrevented || e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return;
        if (rest.target && rest.target !== '_self') return;
        e.preventDefault();
        navigateWhenLoaded(to, { replace });
      }}
    />
  );
}
