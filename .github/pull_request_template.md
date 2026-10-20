## Summary

<!-- What does this change, and why? Link issues with "Closes #123". -->

## How it was tested

<!-- Commands you ran, media you tried, platforms. For performance changes, say how you measured. -->

## Checklist

- [ ] The pull request title is a [Conventional Commit](https://www.conventionalcommits.org/) (`feat: …`, `fix(scan): …`); release-please builds the changelog from it.
- [ ] `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` pass.
- [ ] If `web/` changed: `pnpm typecheck && pnpm lint && pnpm test` pass in `web/`.
- [ ] Tests are added or updated for the change.
- [ ] Docs are updated where behaviour changed (README, docs/ARCHITECTURE.md).
- [ ] No secrets, personal paths or private media in the diff.
