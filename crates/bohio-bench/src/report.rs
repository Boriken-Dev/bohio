//! Samples, their statistics and the result file.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use serde::Serialize;

/// One metric: min, median and max over its samples, in milliseconds.
#[derive(Debug, Clone, Serialize)]
pub struct Metric {
    /// The fastest sample.
    pub min_ms: f64,
    /// The middle sample (the mean of the two middle ones for an even count).
    pub median_ms: f64,
    /// The slowest sample.
    pub max_ms: f64,
    /// How many samples.
    pub samples: usize,
    /// Bytes one sample moved, when it moves bytes: throughput is `bytes / median_ms`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
}

/// Milliseconds of a duration, with microsecond resolution.
pub fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

impl Metric {
    /// The statistics of `values` (milliseconds); `None` when there are none.
    pub fn of(values: &[f64], bytes: Option<u64>) -> Option<Self> {
        if values.is_empty() {
            return None;
        }
        let mut sorted = values.to_vec();
        sorted.sort_by(f64::total_cmp);
        let middle = sorted.len() / 2;
        let median = if sorted.len() % 2 == 1 {
            sorted[middle]
        } else {
            f64::midpoint(sorted[middle - 1], sorted[middle])
        };
        Some(Self {
            min_ms: sorted[0],
            median_ms: median,
            max_ms: sorted[sorted.len() - 1],
            samples: sorted.len(),
            bytes,
        })
    }
}

/// What a result was measured on.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Raw {
    /// The commit built.
    pub commit: String,
    /// The compiler.
    pub rustc: String,
    /// The operating system.
    pub os: String,
    /// The architecture.
    pub arch: String,
    /// The processor's brand, which `dotagents compare-bench` compares.
    pub processor: String,
    /// How the link was carried (`tcp`).
    pub transport: String,
    /// The path between the two sides: an emulated link's name, or `site-vpn`.
    pub link: String,
    /// The `--set` values applied to both sides.
    pub settings: BTreeMap<String, String>,
    /// A free label.
    pub label: String,
}

/// A result file.
#[derive(Debug, Serialize)]
pub struct Report {
    /// The result's name.
    pub name: String,
    /// What it was measured on.
    pub raw: Raw,
    /// Every metric by name.
    pub metrics: BTreeMap<String, Metric>,
    /// For a timeline scenario, each sample's bytes received by time: `[[ms, bytes], ...]`.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub curves: BTreeMap<String, Vec<Vec<[u64; 2]>>>,
}

impl Report {
    /// Writes the file, creating its directory.
    ///
    /// # Errors
    ///
    /// The error of creating the directory or writing the file.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)?;
        }
        let mut text = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        text.push('\n');
        std::fs::write(path, text)
    }
}

/// The processor brand and the operating system, as the machine reports them.
pub fn machine() -> (String, String) {
    let mut system = sysinfo::System::new();
    system.refresh_cpu_list(sysinfo::CpuRefreshKind::nothing());
    let brand = system
        .cpus()
        .first()
        .map(|cpu| cpu.brand().trim().to_owned())
        .unwrap_or_default();
    (brand, std::env::consts::OS.to_owned())
}
