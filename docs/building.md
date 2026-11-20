# Building

Bohío builds with plain `cargo`. `tools/build.py` adds what a cargo build does not: a vendored copy of every dependency so a
build needs no network, a lock of the tools a build is made with, and the release archives and container image. It is a
single Python 3.11 file with no packages to install, configured by `tools/build.cfg`; `python tools/build.py --help` and
`python tools/build.py help config` list every command and setting, so this page says what each part is for and not its flags.

The file is a copy of the release overlay's `tools/build.py` in
[Boriken-Dev/agent-overlays](https://github.com/Boriken-Dev/agent-overlays). Line 3 of it names the commit it was copied
from, `python tools/build.py self check` fails when it differs from that commit, and a change to it is made in the overlay and
copied here with `self update`, never edited in this repository.

## The deps branch

The `deps` branch is an orphan branch that holds `crates/`, a cargo vendor directory with one `NAME-VERSION/` for every
package `Cargo.lock` names, from crates.io or from git. It leaves out tests, examples, benchmarks and fixtures, writes a binary
a build reads as text, and holds nothing an archive scanner would refuse. `manifest.json` on it records the commit and the
`Cargo.lock` it was made from.

`tools/sdk.lock` records the deps commit a given main was built with, on its `deps crates` row. A contributor changes
`Cargo.lock`, then refreshes the branch on a clean checkout of main with `python tools/build.py deps refresh`, and pushes the
`deps` branch before main. A refresh runs locally and commits as the person who runs it; CI only checks the pinned commit
(`deps check --ref`, `scan`).

To build from it without a network, check the pinned commit out beside the repository and let the tool write the cargo
configuration that replaces every source by it:

```text
git fetch --depth 1 origin <the commit on the deps crates row of tools/sdk.lock>
git worktree add --detach ../bohio-deps <that commit>
BUILD_DEPS=../bohio-deps python tools/build.py deps use --out /tmp/bohio-offline --json
cargo --config /tmp/bohio-offline/cargo-home/config.toml build --release --locked -p bohio --features cli
```

CI runs this build inside a network namespace that has no interface, next to a control that fails the same way when it tries
the registry, so an offline build that quietly reached for the network cannot pass.

## The SDK lock

`tools/sdk.lock` is the only file to trust for what a release is built with: the Rust toolchains (the newest tested stable and
the minimum, which `Cargo.toml`'s `rust-version` must agree with), `rustup`, the LLVM that compiles `ring` on Windows ARM64
where no clang is found, the container image's two base images by digest, and the system packages a target needs.
`python tools/build.py sdk check` holds the repository to it (the Containerfile's digests included, and with `--online` each
download's hash against what its publisher says); `sdk install --groups core,release` installs the groups under one
prefix without touching the shell profile.

## Release builds

A release is one archive per target, built natively on that target's runner, plus a container image per Linux architecture.
`python tools/build.py release matrix` prints the targets and runners, which is what the workflows read.

| Target | Archive | Notes |
| --- | --- | --- |
| `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl` | `.tar.gz` | static; `musl-tools` compiles `ring`'s C |
| `x86_64-pc-windows-msvc`, `aarch64-pc-windows-msvc` | `.zip` | static CRT |
| `aarch64-apple-darwin`, `x86_64-apple-darwin`, `universal-apple-darwin` | `.tar.gz` | the universal binary joins both with `lipo` |

`python tools/build.py release build --target TRIPLE --offline` builds one from the deps branch and packs
`dist/bohio-VERSION-TRIPLE.ARCHIVE` with the licence and readme files, recording its hash in `dist/SHA256SUMS`; the same
inputs give the same archive bytes. `release verify` checks that each Linux binary is statically linked, and `release image
--arch amd64|arm64` builds `packaging/container/Containerfile` and saves it as an OCI archive. The image build runs cargo
inside the container and uses the network. Nothing is pushed to a registry.

The `test` workflow builds all of this on every run, and the `release` workflow, on a `v*` tag, runs that workflow, builds the
docs strictly and attaches the archives, the images and one `SHA256SUMS` to a GitHub release.
