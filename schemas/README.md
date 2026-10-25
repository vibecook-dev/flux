# Schemas

JSON Schemas (draft 2020-12) of flux's agent contract, `flux.agent/1`
([docs/AGENT-NATIVE.md](../docs/AGENT-NATIVE.md)): the input and output of
every operation (`<op>.input.json`, `<op>.output.json`), and `lens`,
`fields`, `item`, `changeset`, `error` and `manifest`.

They are generated from the Rust types in `crates/flux-core/src/agent.rs`
and checked in so a contract change shows up in review. A running server
serves the same files at `/api/v1/schemas/{name}` (and the whole manifest at
`/api/v1/agent`); `flux schema <name>` prints one.

To regenerate them after changing the types:

```sh
UPDATE_SCHEMAS=1 cargo test -p flux-server --test schemas
```
