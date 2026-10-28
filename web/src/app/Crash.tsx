/**
 * When a screen fails to show: an error boundary that says what happened and
 * offers a way on, instead of an empty (or black) window.
 *
 * A screen whose code can't be fetched almost always means flux was updated
 * under this page and its old files are gone: the boundary asks the server and
 * reloads onto the new version by itself (update-checks.ts), or says why not.
 */
import { Component, type ErrorInfo, type ReactNode } from 'react';
import styles from './Crash.module.css';
import { type Recovery, recoverFromStaleCode } from './update-checks';
import { isStaleCode, reloadNow } from './update';

/** Where the boundary sits: the whole app, a page over the library, or the player. */
export type CrashPlace = 'app' | 'overlay' | 'player';

interface Props {
  place: CrashPlace;
  children: ReactNode;
  /** Close the page (overlays). */
  onClose?: () => void;
  /** Leave the player. */
  onBack?: () => void;
}

interface State {
  error: Error | null;
  /** For code that didn't load: what the recovery came to (`checking` while it asks the server). */
  recovery: Recovery | 'checking' | null;
}

export class CrashBoundary extends Component<Props, State> {
  state: State = { error: null, recovery: null };

  static getDerivedStateFromError(error: unknown): State {
    const err = error instanceof Error ? error : new Error(String(error));
    return { error: err, recovery: isStaleCode(err) ? 'checking' : null };
  }

  componentDidCatch(error: unknown, _info: ErrorInfo) {
    if (!isStaleCode(error)) return;
    void recoverFromStaleCode().then((recovery) => this.setState({ recovery }));
  }

  render() {
    const { error, recovery } = this.state;
    if (!error) return this.props.children;
    return <Crash place={this.props.place} error={error} recovery={recovery} onClose={this.props.onClose} onBack={this.props.onBack} />;
  }
}

export function Crash({ place, error, recovery, onClose, onBack }: { place: CrashPlace; error: Error; recovery: State['recovery']; onClose?: () => void; onBack?: () => void }) {
  let title: string;
  let body: string;
  let action: string | null = 'Reload';
  if (recovery === 'checking' || recovery === 'reloading') {
    title = 'flux was updated';
    body = 'Opening the new version…';
    action = null;
  } else if (recovery === 'offline') {
    title = 'Can’t reach flux';
    body = 'This screen’s code didn’t load, and flux didn’t answer. Check that it is running, then try again.';
    action = 'Try again';
  } else if (recovery === 'stuck') {
    title = 'This screen didn’t load';
    body = 'Its code didn’t arrive. Reloading usually brings it.';
  } else {
    title = 'Something went wrong';
    body = 'flux hit an error showing this screen. Reloading usually fixes it.';
  }
  return (
    <div className={styles.crash} data-place={place} role="alert">
      <div className={place === 'player' ? styles.card : `${styles.card} glass`}>
        <h1 className="display-m">{title}</h1>
        <p>{body}</p>
        {recovery === null && (
          <details className={styles.details}>
            <summary>Details</summary>
            <code className="mono">{error.message || error.name}</code>
          </details>
        )}
        <div className={styles.actions}>
          {action && (
            <button type="button" className="btn btn-primary" onClick={() => reloadNow()}>
              {action}
            </button>
          )}
          {place === 'overlay' && onClose && (
            <button type="button" className="btn" onClick={onClose}>
              Close
            </button>
          )}
          {place === 'player' && onBack && (
            <button type="button" className="btn" onClick={onBack}>
              Back
            </button>
          )}
        </div>
      </div>
    </div>
  );
}
