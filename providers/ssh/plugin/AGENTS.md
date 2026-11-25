# `bohio-provider-ssh-plugin` — API header

A `cdylib` and nothing else: no Rust API. It exports guatiao's entry symbol and declares kinds `bohio_terminal` and `bohio_exec`; a
node with `[providers] paths = [..]` naming the file maps it and offers provider `ssh` (settings: `providers/ssh/AGENTS.md`).

- Library id `bohio-provider-ssh-plugin`, version the workspace's. Provider id `ssh`.
- A node built with feature `ssh` already registers `ssh`: this library then loads, offers nothing new and is reported `loaded`
  with `its provider `ssh` is already registered`. Built-in providers are never replaced by a library.
- A library has its own copy of `tracing`, with no subscriber: the `non-default:` lines of its sources are logged only by the built-in
  provider.
- Not the workspace's lints: the entry symbols are `no_mangle`. Nothing depends on this crate except `bohio`'s tests, as a
  dev-dependency that is never named in code.
