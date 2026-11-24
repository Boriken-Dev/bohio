# bohio-provider-serial-plugin

The `serial` terminal provider of [Bohío](https://github.com/Boriken-Dev/bohio) built as a library (`cdylib`) that a node maps at
start: name the file, or a directory holding it, in `[providers] paths`. The node already has `serial` built in when it was built
with feature `serial`; this library is for a build without it, and is also the smallest example of a provider library: one line.
See `docs/providers.md`, "Loading a provider library".
