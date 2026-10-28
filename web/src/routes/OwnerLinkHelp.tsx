/**
 * The pieces the setup screen shares with the empty library and the owner
 * link: a command to copy, and how a browser becomes flux's owner.
 */
import { useState } from 'react';
import { Icon } from '../components/Icon';
import styles from './Setup.module.css';
import { OWNER_LINK_DOCS } from './setup-state';

/** A command, with a button that copies it. */
export function Command({ text }: { text: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <div className={styles.command}>
      <code className="mono">{text}</code>
      <button
        type="button"
        className="btn btn-sm"
        onClick={() => {
          void navigator.clipboard?.writeText(text).then(() => {
            setCopied(true);
            setTimeout(() => setCopied(false), 1600);
          });
        }}
      >
        <Icon name={copied ? 'check' : 'copy'} />
        {copied ? 'Copied' : 'Copy'}
      </button>
    </div>
  );
}

/** For someone who isn't the owner: how the owner makes a browser one. */
export function OwnerLinkHelp() {
  return (
    <div className={styles.panel}>
      <div className={styles.body}>
        <h2 className="display-m">An owner link</h2>
        <p className={styles.hint}>On the server, ask flux for a link that makes this browser an owner. It works once, within a day.</p>
        <Command text="flux-server owner-link" />
        <p className={styles.hint}>
          In Docker: <code className="mono">docker exec flux flux-server owner-link</code>. A fresh flux also prints one in its log.
        </p>
        <a className={styles.link} href={OWNER_LINK_DOCS} target="_blank" rel="noreferrer noopener">
          How owner links work
        </a>
      </div>
    </div>
  );
}
