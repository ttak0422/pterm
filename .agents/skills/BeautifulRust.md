# BeautifulRust provenance and scope

Rust skills and references are copied without modification from
https://github.com/morganlinton/BeautifulRust at
`b9718436d77ee7bcf17cdcb6aa27628feb95d97a`, under the included MIT license.
No installer, hook, MCP server or evaluation program is installed or executed.

pterm is an application with a shared protocol crate. Preserve wire bytes, terminal
behavior, bounded queues, process ownership and compatibility with existing clients.
Do not rewrite the vendored vt100 fork or force generic API rules onto PTY/FFI
contracts. Review findings are evidence to assess, not automatic rewrite commands.

The workspace initially enables only unsafe-operation, dbg! and todo! warnings.
Broad pedantic/unwrap reviews remain opt-in; existing CI is not made stricter.

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked --no-deps
cargo test --workspace --locked
# Optional broader review, not a CI gate:
cargo clippy --workspace --all-targets --locked --no-deps -- -W clippy::pedantic -W clippy::unwrap_used -W clippy::undocumented_unsafe_blocks
```
