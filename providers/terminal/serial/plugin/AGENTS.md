# `bohio-provider-serial-plugin` — API header

A `cdylib` and nothing else: no Rust API. It exports guatiao's entry symbol and declares kind `bohio_terminal`; a node with
`[providers] paths = [..]` naming the file maps it and offers provider `serial` (settings: `providers/terminal/serial/AGENTS.md`).

- Library id `bohio-provider-serial-plugin`, version the workspace's. Provider id `serial`.
- A node built with feature `serial` already registers `serial`: this library then loads, offers nothing new and is reported
  `loaded` with `its provider `serial` is already registered`. Built-in providers are never replaced by a library.
- Not the workspace's lints: the entry symbols are `no_mangle`. Nothing depends on this crate except `bohio`'s tests, as a
  dev-dependency that is never named in code.
