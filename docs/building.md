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
where no clang is found, the container build's two base images by digest, and the system packages a target needs.
`python tools/build.py sdk check` holds the repository to it (the Containerfile's digests included, and with `--online` each
download's hash against what its publisher says); `sdk install --groups core,release` installs the groups under one
prefix without touching the shell profile.

## Container builds

`packaging/container/Containerfile` builds from the deps branch with no network after the toolchain is installed. It has four
stages: `sdk` (the pinned `rust` base image, a `rustup-init` checked against the lock's hash, and the toolchain the lock
names), `build` and `test` (the deps branch copied in, `offline plan` writing the cargo home, then `cargo build` and
`cargo test` with `RUN --network=none`) and `runtime` (the image, a non-root account and `bohio serve --stdio`). Build it
through the tool, which supplies the build contexts a plain `podman build` lacks:

```text
python tools/build.py container build --tag bohio    # the image
python tools/build.py container test                 # the workspace tests, offline, in the same container
python tools/build.py container build --print        # the settings and the engine command, nothing fetched or run
```

The deps commit is the one the lock's `deps crates` row names, fetched from that row's URL into `target/container/` and held
to `Cargo.lock` before the engine starts. `docker` takes the same command line (`CONTAINER_ENGINE=docker`).

Every override is a source or a location and goes in `.env` (shared by a checkout) or `.env.local` (one machine), both ignored by
git; `.env.example` lists every key with its default commented out. A later layer wins: `.env`, `.env.local`, the process
environment, then `--env KEY=VALUE`. A key that is not in the list (`python tools/build.py help env`) is an error naming the
file and line.

| Keys | What they change |
| --- | --- |
| `RUSTUP_DIST_SERVER`, `RUSTUP_UPDATE_ROOT`, `RUSTUP_INIT_BASE_URL`, `APT_MIRROR` | where the `sdk` stage downloads from; the lock's hash still has to match `rustup-init` |
| `BASE_IMAGE`, `RUNTIME_IMAGE`, `ALLOW_UNPINNED_IMAGE` | the two base images; an override needs an `@sha256:` digest unless `ALLOW_UNPINNED_IMAGE=1`, which is reported |
| `RUST_VERSION`, `RUSTUP_VERSION` with `RUSTUP_INIT_SHA256_X86_64` and `_AARCH64` | a toolchain or rustup other than the lock's; a rustup version and both its hashes go together |
| `DEPS_DIR`, or `DEPS_GIT_URL` and `DEPS_REF` | a local deps checkout, or another place or commit to fetch it from; either is read file by file against `Cargo.lock` |
| `HTTP_PROXY`, `HTTPS_PROXY`, `NO_PROXY`, `EXTRA_CA_CERT` | the `sdk` stage's network; the certificate is trusted there and never reaches the image |
| `CONTAINER_ENGINE`, `CONTAINER_PLATFORM` | `podman` (the default) or `docker`, and the engine's `--platform` |
| `VSINSTALLDIR`, `VCTOOLSINSTALLDIR`, `WINDOWSSDKDIR`, `WINDOWSSDKVERSION`, `LIBCLANG_PATH`, `LLVM_DIR`, `SDK_HOME` | native builds only (below); the container build reports them and ignores them |
| `MSVC_SPLAT_DIR` | a splat of the MSVC CRT and the Windows SDK for the release overlay's optional cross stage, which this Containerfile does not have |

A build prints each key that differs from the lock first and records their names, never their values, as the image label
`build.py.non-default`, beside `org.opencontainers.image.revision` and `build.py.deps-commit`. A value with credentials (a proxy
with a password) reaches the engine through its environment, is on no command line and is printed with the user and password
replaced. `python tools/build.py sdk check` holds the Containerfile's base images, toolchain, `rustup` version and hashes to
the lock.

### Native builds

`sdk install` and `release build` read the native keys from the same `.env`, `.env.local`, environment and `--env KEY=VALUE`
layers (and nothing else of those files), for a machine whose Visual Studio, Windows SDK or LLVM is not where the tools look:

| Key | Names | Effect |
| --- | --- | --- |
| `VSINSTALLDIR`, `VCTOOLSINSTALLDIR` | the Visual Studio or Build Tools root and the MSVC toolset inside it | set for cargo; `sdk install` takes the root instead of asking vswhere |
| `WINDOWSSDKDIR`, `WINDOWSSDKVERSION` | the Windows SDK root and the version of it to build against | set for cargo; the version must be installed under the root |
| `LLVM_DIR`, `LIBCLANG_PATH` | an LLVM installation, and the directory of libclang | `LLVM_DIR`'s `bin` goes first on `PATH` and `sdk install` fetches no LLVM; `LIBCLANG_PATH` defaults to that `bin` when it holds libclang |
| `SDK_HOME` | where `sdk install` puts the SDKs | after `--prefix`, before `$BUILD_SDK_HOME` |

Each path is checked when it is read: a directory that is missing, or lacks what such a directory holds (`VC/`, `Include/` and
`Lib/`, `bin/clang`, `libclang`), fails the command naming the key and the file it came from, before anything is installed or
cargo starts. The keys only point at files on the machine: nothing of Microsoft's is copied into the repository, an image or
the SDK prefix. `sdk install` writes the set keys into `env.sh` and `env.ps1`; `release build` prints each as
`native: KEY=VALUE (where)`.

## Release builds

The archives hold the node alone. A host for libraries in a process of their own (`[providers] isolate`) is
`guatiao-remote-server`, built with `cargo build -p guatiao-source-remote --bin guatiao-remote-server`; it is not in the
archives (`docs/providers.md` says why a host must match the library, not the node).

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
--arch amd64|arm64` builds `packaging/container/Containerfile` as `container build` does and saves it as an OCI archive.
Nothing is pushed to a registry.

The `test` workflow builds all of this on every run, and the `release` workflow, on a `v*` tag, runs that workflow, builds the
docs strictly and attaches the archives, the images and one `SHA256SUMS` to a GitHub release.
