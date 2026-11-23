//! Bohío's benchmark harness.
//!
//! `corpus` makes the files the scenarios read, `local` starts a `bohio serve` child and runs scenarios
//! against it, `run` runs them against a host that already exists (another machine, another network
//! namespace), `raw-serve` is the raw TCP control's server. A result is one JSON file; see
//! `benchmarks/README.md`.

mod compat;
mod corpus;
mod host;
mod raw;
mod report;
mod scenario;
mod wan;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use bohio::config::Layers;
use bohio::peer::Data;
use bohio_client::dial::Endpoint;
use bohio_client::link::Settings;
use clap::{Args, Parser, Subcommand};
use coqui_link::auth::{Proof, Trust};

use host::Sections;
use report::{Raw, Report};
use scenario::{Env, Runner};

#[derive(Debug, Parser)]
#[command(name = "bohio-bench", about = "Bohío's benchmark harness")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Make the deterministic corpus.
    Corpus(CorpusArgs),
    /// Start a `bohio serve` child, pair and approve this harness's key, and run scenarios.
    Local(LocalArgs),
    /// Run scenarios against a host that already trusts this harness's key.
    Run(RunArgs),
    /// Serve the raw TCP control.
    RawServe(RawServeArgs),
}

#[derive(Debug, Args)]
struct CorpusArgs {
    /// Where to make the files.
    #[arg(long)]
    dir: PathBuf,
    /// The sizes of the `random-<size>` and `text-<size>` files, comma separated.
    #[arg(
        long,
        value_delimiter = ',',
        default_value = "1KiB,256KiB,1MiB,5MiB,16MiB,64MiB,256MiB"
    )]
    sizes: Vec<String>,
    /// How many files of 1 to 8 KiB go under `small/`.
    #[arg(long, default_value_t = 1000)]
    small: usize,
}

/// What every way of running scenarios takes.
#[derive(Debug, Args)]
struct Common {
    /// A scenario to run (repeat); the names are in `benchmarks/README.md`.
    #[arg(long, required = true)]
    scenario: Vec<String>,
    /// Samples per metric.
    #[arg(long, default_value_t = 3)]
    samples: usize,
    /// The result file.
    #[arg(long)]
    save: PathBuf,
    /// The result's name; default the file's stem.
    #[arg(long)]
    name: Option<String>,
    /// A `[link]` or `[filesystem]` key for the host, and for this side's link bounds where there is one:
    /// `link.window=4MiB`. Both sides' windows and frames must agree, so set them here; `link.bulk_window` is the receiver's own and may differ.
    #[arg(long = "set", value_name = "KEY=VALUE")]
    set: Vec<String>,
    /// The path between the two sides, as the result names it.
    #[arg(long, default_value = "lan")]
    link: String,
    /// How the link was carried.
    #[arg(long, default_value = "tcp")]
    transport: String,
    /// A free label stored in the result.
    #[arg(long, default_value = "")]
    label: String,
    /// The commit measured; default `git rev-parse --short HEAD` in the current directory.
    #[arg(long)]
    commit: Option<String>,
    /// The `bohio` the `remote_*` scenarios run; default the one beside this program.
    #[arg(long)]
    bohio: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct LocalArgs {
    #[command(flatten)]
    common: Common,
    /// The corpus directory (made by `corpus`): the host's root.
    #[arg(long)]
    corpus: PathBuf,
    /// The helper program exec runs; default `examples/exec_helper` beside this program.
    #[arg(long)]
    exec_helper: Option<PathBuf>,
    /// A command that starts each host process, split on spaces: `sudo -n ip netns exec bh-host`.
    #[arg(long, default_value = "")]
    host_prefix: String,
    /// The address the host listens on.
    #[arg(long, default_value = "127.0.0.1")]
    listen_ip: String,
    /// Put an emulated path between this side and the host, in process: `DELAY_MS` or `DELAY_MS:MBIT` each way (the
    /// `wan` link of `netem.sh` is `133:20`). The delay and the rate are the proxy's own timing, with no loss.
    #[arg(long, value_name = "DELAY_MS[:MBIT]")]
    wan: Option<String>,
}

#[derive(Debug, Args)]
struct RunArgs {
    #[command(flatten)]
    common: Common,
    /// The host, `tcp://address:port`.
    #[arg(long)]
    endpoint: String,
    /// The client's configuration file (its `bohio join` and its `peer approve` on the host are done).
    #[arg(long)]
    client_config: PathBuf,
    /// The client's data directory.
    #[arg(long)]
    data_dir: PathBuf,
    /// The host's id to trust instead of the keys `join` stored.
    #[arg(long)]
    server: Option<String>,
    /// The corpus directory on the host, in the host's path text.
    #[arg(long)]
    root: String,
    /// The helper program on the host; without it the exec scenarios are skipped.
    #[arg(long)]
    program: Option<String>,
    /// The raw control server's address.
    #[arg(long)]
    raw: Option<SocketAddr>,
    /// The host process's id when it runs on this machine, for the host CPU metrics.
    #[arg(long)]
    host_pid: Option<u32>,
}

#[derive(Debug, Args)]
struct RawServeArgs {
    /// The address to listen on.
    #[arg(long)]
    listen: SocketAddr,
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(error) => {
            eprintln!("bohio-bench: {error}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: Cli) -> Result<bool, String> {
    match cli.command {
        Cmd::Corpus(args) => {
            corpus::make(&args.dir, &args.sizes, args.small)?;
            println!("corpus in {}", args.dir.display());
            Ok(true)
        }
        Cmd::RawServe(args) => {
            raw::serve(args.listen).map_err(|error| error.to_string())?;
            Ok(true)
        }
        Cmd::Local(args) => local(&args),
        Cmd::Run(args) => remote(&args),
    }
}

/// The `[link]` and `[filesystem]` keys of `--set`, as TOML for the host, this side's settings, and the one
/// `bench.compress=ALG[:LEVEL]` key, which is the client's request in the compression scenarios.
fn apply_sets(sets: &[String]) -> Result<(Sections, Settings, Option<String>), String> {
    let mut host: Sections = BTreeMap::new();
    let mut settings = Settings::default();
    let mut compress = None;
    let mut max_window_set = false;
    for set in sets {
        let (key, value) = set
            .split_once('=')
            .ok_or_else(|| format!("`{set}` is not KEY=VALUE"))?;
        let (section, name) = key
            .split_once('.')
            .ok_or_else(|| format!("`{key}` is not section.key"))?;
        if key == "bench.compress" {
            compress = Some(value.to_owned());
            continue;
        }
        if section != "link" && section != "filesystem" {
            return Err(format!(
                "`{key}`: only [link] and [filesystem] keys, and bench.compress, are set here"
            ));
        }
        let number = corpus::parse_size(value).ok();
        let toml = number.map_or_else(|| value.to_owned(), |count| count.to_string());
        host.entry(section.to_owned())
            .or_default()
            .insert(name.to_owned(), toml);
        if section == "link" {
            let count = number.ok_or_else(|| format!("`{key}` takes a number on this side"));
            let usize_of = |count: Result<u64, String>| -> Result<usize, String> {
                usize::try_from(count?).map_err(|_| format!("`{key}` is too large"))
            };
            match name {
                "window" => {
                    settings.window =
                        u32::try_from(count?).map_err(|_| "window too large".to_owned())?;
                }
                "max_window" => {
                    settings.max_window =
                        u32::try_from(count?).map_err(|_| "max_window too large".to_owned())?;
                    max_window_set = true;
                }
                "bulk_window" => {
                    settings.bulk_window =
                        u32::try_from(count?).map_err(|_| "bulk_window too large".to_owned())?;
                }
                "auto_window" => settings.auto_window = value == "true",
                "max_metadata" => settings.max_metadata = usize_of(count)?,
                "max_pending_opens" => settings.max_pending_opens = usize_of(count)?,
                "max_opens_per_minute" => {
                    settings.max_opens_per_minute = u32::try_from(count?)
                        .map_err(|_| "max_opens_per_minute too large".to_owned())?;
                }
                "tcp_send_buffer" => settings.tcp_send_buffer = Some(usize_of(count)?),
                "tcp_receive_buffer" => settings.tcp_receive_buffer = Some(usize_of(count)?),
                "max_frame" => settings.max_frame = usize_of(count)?,
                "max_message" => settings.max_message = usize_of(count)?,
                "queue_bytes" => settings.queue_bytes = usize_of(count)?,
                "read_ahead" => settings.read_ahead = usize_of(count)?,
                "queued_bytes" => settings.queued_bytes = usize_of(count)?,
                "max_streams" => settings.max_streams = usize_of(count)?,
                _ => {}
            }
        }
    }
    // As the host's own file does: a bulk window raises the most a window may grow to unless that was set.
    if !max_window_set {
        settings.max_window = settings.window.max(settings.bulk_window);
    }
    Ok((host, settings, compress))
}

fn sibling(name: &str) -> PathBuf {
    let exe = std::env::current_exe().unwrap_or_default();
    exe.parent()
        .map(|dir| dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX)))
        .unwrap_or_default()
}

fn command_text(program: &str, args: &[&str]) -> String {
    Command::new(program)
        .args(args)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .unwrap_or_default()
}

/// The proof of the client whose identity is in `data_dir` and whose configuration file `config` holds the
/// `[peer.<id>]` table `bohio join` wrote for the host, or that trusts `server` alone.
fn proof_of(data_dir: &Path, config: &Path, server: Option<&str>) -> Result<Proof, String> {
    let data = Data::new(data_dir);
    let identity = data.identity().map_err(|error| error.to_string())?;
    let trust = match server {
        Some(id) => Trust::pinned([id.parse().map_err(|_| "that is not a peer id".to_owned())?]),
        None => Trust::pinned(
            Layers::peers(&[config.to_owned()])
                .map_err(|error| error.to_string())?
                .ids()
                .filter_map(|name| name.parse().ok()),
        ),
    };
    Proof::builder(identity)
        .responders(trust)
        .build()
        .map_err(|error| error.to_string())
}

fn finish(
    common: &Common,
    env_settings: &Sections,
    compress: Option<&str>,
    runner: Runner,
    failed: bool,
) -> Result<bool, String> {
    let (processor, os) = report::machine();
    let mut settings = BTreeMap::new();
    if let Some(wanted) = compress {
        settings.insert("bench.compress".to_owned(), wanted.to_owned());
    }
    for (section, keys) in env_settings {
        for (key, value) in keys {
            settings.insert(format!("{section}.{key}"), value.clone());
        }
    }
    let name = common.name.clone().unwrap_or_else(|| {
        common
            .save
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default()
    });
    let report = Report {
        name,
        raw: Raw {
            commit: common
                .commit
                .clone()
                .unwrap_or_else(|| command_text("git", &["rev-parse", "--short", "HEAD"])),
            rustc: command_text("rustc", &["--version"]),
            os,
            arch: std::env::consts::ARCH.to_owned(),
            processor,
            transport: common.transport.clone(),
            link: common.link.clone(),
            settings,
            label: common.label.clone(),
        },
        metrics: runner.out.metrics,
        curves: runner.out.curves,
    };
    report
        .save(&common.save)
        .map_err(|error| error.to_string())?;
    println!(
        "saved {} ({} metrics)",
        common.save.display(),
        report.metrics.len()
    );
    Ok(!failed)
}

fn run_all(runner: &mut Runner, names: &[String]) -> bool {
    let mut ok = true;
    for name in names {
        eprintln!("scenario {name}");
        if let Err(error) = runner.run(name) {
            eprintln!("bohio-bench: {name} failed: {error}");
            ok = false;
        }
    }
    ok
}

fn local(args: &LocalArgs) -> Result<bool, String> {
    let common = &args.common;
    let (extra, settings, compress) = apply_sets(&common.set)?;
    let bohio = common.bohio.clone().unwrap_or_else(|| sibling("bohio"));
    let helper = args.exec_helper.clone().unwrap_or_else(|| {
        sibling("bohio")
            .parent()
            .map(|dir| {
                dir.join("examples")
                    .join(format!("exec_helper{}", std::env::consts::EXE_SUFFIX))
            })
            .unwrap_or_default()
    });
    let root = std::path::absolute(&args.corpus).map_err(|error| error.to_string())?;
    let helper = std::path::absolute(&helper).map_err(|error| error.to_string())?;
    let dir = std::env::temp_dir().join(format!("bohio-bench-{}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
    let prefix: Vec<String> = args
        .host_prefix
        .split_whitespace()
        .map(str::to_owned)
        .collect();

    let outcome = (|| {
        let local = host::start(&host::Setup {
            bohio: &bohio,
            dir: &dir,
            prefix: &prefix,
            listen_ip: &args.listen_ip,
            root: &root,
            program: &helper,
            extra: &extra,
        })?;
        let mut raw_argv = prefix.clone();
        raw_argv.push(
            std::env::current_exe()
                .map_err(|error| error.to_string())?
                .to_string_lossy()
                .into_owned(),
        );
        raw_argv.extend([
            "raw-serve".to_owned(),
            "--listen".to_owned(),
            format!("{}:0", args.listen_ip),
        ]);
        let mut raw_server = host::Proc::start(&raw_argv, &[])?;
        let raw_address: SocketAddr = raw_server
            .expect("raw listening ")?
            .parse()
            .map_err(|_| "the raw control's address".to_owned())?;
        let mut endpoint_text = local.endpoint.clone();
        if let Some(text) = &args.wan {
            let target: SocketAddr = endpoint_text
                .strip_prefix("tcp://")
                .and_then(|address| address.parse().ok())
                .ok_or("--wan needs a tcp endpoint")?;
            let proxy =
                wan::start(target, wan::Path::parse(text)?).map_err(|error| error.to_string())?;
            endpoint_text = format!("tcp://{proxy}");
        }
        let endpoint = Endpoint::parse(&endpoint_text)?;
        let proof = proof_of(&local.client_data, &local.client_config, None)?;
        let mut runner = Runner::new(Env {
            endpoint,
            endpoint_text: endpoint_text.clone(),
            proof,
            settings,
            root: root.to_string_lossy().into_owned(),
            program: Some(helper.to_string_lossy().into_owned()),
            bohio: bohio.clone(),
            client_config: local.client_config.clone(),
            server: None,
            raw: Some(raw_address),
            cpu: Some(host::Cpu::of(local.host.id())),
            samples: common.samples,
            suffix: format!("{}_{}", common.transport, common.link),
            compress: compress.clone(),
        });
        let ok = run_all(&mut runner, &common.scenario);
        drop(local);
        drop(raw_server);
        finish(common, &extra, compress.as_deref(), runner, !ok)
    })();
    let _ = std::fs::remove_dir_all(&dir);
    outcome
}

fn remote(args: &RunArgs) -> Result<bool, String> {
    let common = &args.common;
    let (extra, settings, compress) = apply_sets(&common.set)?;
    let bohio = common.bohio.clone().unwrap_or_else(|| sibling("bohio"));
    let proof = proof_of(&args.data_dir, &args.client_config, args.server.as_deref())?;
    let mut runner = Runner::new(Env {
        endpoint: Endpoint::parse(&args.endpoint)?,
        endpoint_text: args.endpoint.clone(),
        proof,
        settings,
        root: args.root.clone(),
        program: args.program.clone(),
        bohio,
        client_config: args.client_config.clone(),
        server: args.server.clone(),
        raw: args.raw,
        cpu: args.host_pid.map(host::Cpu::of),
        samples: common.samples,
        suffix: format!("{}_{}", common.transport, common.link),
        compress: compress.clone(),
    });
    let ok = run_all(&mut runner, &common.scenario);
    finish(common, &extra, compress.as_deref(), runner, !ok)
}
