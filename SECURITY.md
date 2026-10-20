# Security policy

## Supported versions

flux is pre-1.0. Security fixes go into the latest release only, and into `main`.

| Version | Supported |
|---|---|
| Latest release | Yes |
| Older releases | No |
| `main` | Fixed on `main` first, then released |

Until the first release, only `main` is supported.

## Reporting a vulnerability

**Please don't report security problems in public issues, discussions or pull requests.**

Report them privately through GitHub's private vulnerability reporting: go to the repository's [Security tab](https://github.com/vibecook-dev/flux/security) and choose **Report a vulnerability** (or open the [advisory form](https://github.com/vibecook-dev/flux/security/advisories/new) directly). Only the maintainers can see the report.

Please include:

- what an attacker can do, and from where (localhost, the LAN, the tailnet, a crafted media file, …);
- the flux version or commit, the OS, and the remote-access mode (`--tailnet off|embedded|system`, `--lan`);
- steps or a proof of concept that reproduce it;
- any fix or mitigation you suggest.

We aim to acknowledge a report within a few days, keep you updated while we fix it, and credit you in the advisory unless you'd rather stay anonymous. Please give us a reasonable time to release a fix before you disclose the issue publicly.

## Scope

In scope: the `flux-server` binary and its crates, the web client in `web/`, and the release binaries and Docker image published from this repository. Examples:

- getting a role or data you shouldn't have, e.g. bypassing the Host allowlist, the CSRF check or tailnet role checks;
- reading files outside a library through the media or artwork routes;
- crashing or exhausting the server from the network, or with a crafted media file.

Out of scope: vulnerabilities in FFmpeg, Tailscale, Mille or Truffle themselves (report those to their projects; tell us too if flux makes them reachable), and attacks that need an already compromised machine or the server's owner account.
