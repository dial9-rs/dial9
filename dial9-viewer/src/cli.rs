use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;
use std::time::Duration;

mod skills {
    include!(concat!(env!("OUT_DIR"), "/skills.rs"));

    pub fn get(name: &str) -> Option<&'static str> {
        SKILL_DIRS.iter().find(|s| s.name == name).map(|s| s.body)
    }
}

#[derive(Parser, Debug)]
#[command(
    name = "dial9",
    about = "Trace browser and viewer for dial9-tokio-telemetry"
)]
pub struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Agent skill documentation and analysis toolkit
    Agents {
        #[command(subcommand)]
        action: Option<AgentsAction>,
    },
    /// Extract or generate trace shapes (sanitized structural fingerprints)
    TraceShape {
        #[command(subcommand)]
        action: TraceShapeAction,
    },
    /// Start the web server
    Serve {
        /// Port to listen on
        #[arg(long, default_value = "3000")]
        port: u16,

        /// S3 bucket name
        #[arg(long)]
        bucket: Option<String>,

        /// S3 key prefix
        #[arg(long)]
        prefix: Option<String>,

        /// Run without S3 using generated traces. With no value, uses
        /// `synthetic`; `demo` replays the bundled demo trace in every segment.
        #[arg(
            long,
            value_enum,
            num_args = 0..=1,
            default_missing_value = "synthetic",
            conflicts_with_all = [
                "bucket",
                "local_dir",
                "agg",
                "agg_source_dir",
                "agg_output_dir",
                "agg_output_bucket",
                "agg_output_prefix",
                "agg_segment_secs",
                "enable_upload"
            ]
        )]
        simulator: Option<SimulatorModeArg>,

        /// Number of simulated hosts.
        #[arg(long, default_value_t = 3, requires = "simulator")]
        simulator_hosts: usize,

        /// Duration and key spacing of each simulated segment, in seconds.
        #[arg(long, default_value_t = 60, requires = "simulator")]
        simulator_segment_secs: u64,

        /// Template repetitions inside each synthetic segment.
        #[arg(long, default_value_t = 1, requires = "simulator")]
        simulator_repetitions: u32,

        /// Synthetic feature groups to include, comma-separated. Omit for all;
        /// use `none` for clock/metadata events only.
        #[arg(
            long,
            value_enum,
            value_delimiter = ',',
            num_args = 1..,
            requires = "simulator"
        )]
        simulator_features: Option<Vec<SimulatorFeatureArg>>,

        /// Synthetic stack-symbol naming. Omit for anonymous placeholders.
        #[arg(long, value_enum, requires = "simulator")]
        simulator_symbols: Option<SimulatorSymbolModeArg>,

        /// Serve traces from a local directory instead of S3
        #[arg(long, conflicts_with = "bucket")]
        local_dir: Option<PathBuf>,

        /// Dev mode: serve UI files from disk for faster iteration
        #[arg(long)]
        dev: bool,

        /// Local mode: optimize output for running on a workstation rather than
        /// a deployed host. Logs are rendered human-readable (instead of JSON)
        /// and per-request metrics use metrique's local format (instead of
        /// CloudWatch EMF). The default (deployed) emits JSON logs and EMF
        /// metrics to stdout.
        #[arg(long)]
        local: bool,

        /// Enable demand-driven aggregation against the S3 `--bucket`/`--prefix`
        /// source: the flamegraph button folds raw trace segments on demand and
        /// progressively refines. (For a local source, use `--agg-source-dir`.)
        #[arg(long)]
        agg: bool,

        /// Enable demand-driven aggregation reading raw segments from this local
        /// directory (the local equivalent of `--agg` over S3).
        #[arg(long, conflicts_with = "bucket")]
        agg_source_dir: Option<PathBuf>,

        /// Where the on-demand aggregator writes (and re-reads) its Parquet
        /// part-files (local). Defaults to `<agg_source_dir>/flamegraph-data`.
        #[arg(long)]
        agg_output_dir: Option<PathBuf>,

        /// Optional S3 bucket for persistent aggregator Parquet part-files. For
        /// S3/BYOC aggregation, leaving this unset uses a process-local temporary
        /// directory and never writes to the source bucket.
        #[arg(long)]
        agg_output_bucket: Option<String>,

        /// Output S3 key prefix for the aggregator's Parquet part-files.
        #[arg(long, default_value = "flamegraph-data")]
        agg_output_prefix: String,

        /// Raw-trace segment duration (seconds), used to pad the scope time
        /// filter so boundary files are not dropped.
        #[arg(long, default_value_t = crate::ingest::aggregate::DEFAULT_SEGMENT_DURATION_SECS)]
        agg_segment_secs: i64,

        /// Enable the temporary trace-upload feature: lets another site POST a
        /// trace (`POST /api/upload`) and have the viewer serve it back once.
        /// Off by default; there is no auth, so only enable on a trusted network.
        #[arg(long)]
        enable_upload: bool,
    },
    /// Tools for working with agent-generated HTML reports
    Report {
        #[command(subcommand)]
        action: ReportAction,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum SimulatorModeArg {
    Synthetic,
    Demo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum SimulatorSymbolModeArg {
    Anonymous,
    Realistic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum SimulatorFeatureArg {
    Cpu,
    Scheduling,
    Tasks,
    Spans,
    Memory,
    Resources,
    CustomEvents,
    None,
}

#[derive(Subcommand, Debug)]
enum ReportAction {
    /// Serve a report folder over HTTP so embedded iframes can fetch
    /// trace files (browsers block `fetch()` over `file://`).
    Serve {
        /// Path to the report folder (containing `report.html` and assets)
        path: PathBuf,

        /// Port to listen on
        #[arg(long, default_value = "8000")]
        port: u16,
    },
}

#[derive(Subcommand, Debug)]
enum TraceShapeAction {
    /// Extract a sanitized shape from a trace file
    Extract {
        /// Input trace file (binary or gzip)
        trace: PathBuf,
        /// Output shape JSON file
        shape_json: PathBuf,
    },
    /// Generate a synthetic trace from a shape file
    Generate {
        /// Input shape JSON file
        shape_json: PathBuf,
        /// Output trace file (binary)
        trace: PathBuf,
        /// Number of repetitions of the template (default 1, must be >= 1)
        #[arg(long, default_value = "1")]
        repeat: u32,
    },
    /// Sanitize a source trace directly into a synthetic trace without JSON
    Synthesize {
        /// Input source trace file (binary or gzip)
        source_trace: PathBuf,
        /// Output synthetic trace file (binary)
        synthetic_trace: PathBuf,
        /// Number of repetitions of the template (default 1, must be >= 1)
        #[arg(long, default_value = "1")]
        repeat: u32,
    },
}

#[derive(Subcommand, Debug)]
enum AgentsAction {
    /// Copy the analysis toolkit (JS modules) to a directory
    Toolkit {
        /// Directory to write toolkit files into (created if missing)
        path: PathBuf,
    },
    /// Print a specific skill's instructions
    Skill {
        /// Skill name (e.g. dial9-trace-loading, dial9-red-flags)
        name: String,
    },
    /// Unpack all skills as an Agent Skills spec directory
    Skills {
        /// Directory to write skills into (created if missing)
        path: PathBuf,
    },
}

/// Build a Tokio runtime and run the CLI. For binaries that don't set up their
/// own runtime (e.g. the `dial9` binary).
pub fn run_blocking() -> anyhow::Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run())
}

/// Run the CLI. Call this from your binary's `main()`.
pub async fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Agents { action } => match action {
            None => print!("{}", skills::HEADER),
            Some(AgentsAction::Toolkit { path }) => {
                std::fs::create_dir_all(&path)?;
                for (name, content) in skills::TOOLKIT_FILES {
                    std::fs::write(path.join(name), content)?;
                }
                let abs = std::fs::canonicalize(&path)?;
                eprintln!("Toolkit written to {}", abs.display());
                eprintln!(
                    "Run: node {}/analyze.js <trace.bin or directory>",
                    abs.display()
                );
            }
            Some(AgentsAction::Skill { name }) => match skills::get(&name) {
                Some(content) => print!("{}", content),
                None => {
                    eprintln!("Unknown skill: {name}");
                    eprintln!("Available skills:");
                    for skill in skills::SKILL_DIRS {
                        eprintln!("  {:24} {}", skill.name, skill.description);
                    }
                    std::process::exit(1);
                }
            },
            Some(AgentsAction::Skills { path }) => {
                for skill in skills::SKILL_DIRS {
                    let skill_dir = path.join(skill.name);
                    for (rel_path, content) in skill.files {
                        let file_path = skill_dir.join(rel_path);
                        if let Some(parent) = file_path.parent() {
                            std::fs::create_dir_all(parent)?;
                        }
                        std::fs::write(&file_path, content)?;
                    }
                }
                let abs = std::fs::canonicalize(&path)?;
                eprintln!("Skills unpacked to {}", abs.display());
                eprintln!("Add to .kiro/skills/ or point your agent at this directory.");
            }
        },
        Commands::TraceShape { action } => match action {
            TraceShapeAction::Extract { trace, shape_json } => {
                crate::trace_shape::extract(&trace, &shape_json)?;
                eprintln!("Shape extracted to {}", shape_json.display());
            }
            TraceShapeAction::Generate {
                shape_json,
                trace,
                repeat,
            } => {
                crate::trace_shape::generate(&shape_json, &trace, repeat)?;
                eprintln!("Trace generated at {}", trace.display());
            }
            TraceShapeAction::Synthesize {
                source_trace,
                synthetic_trace,
                repeat,
            } => {
                crate::trace_shape::synthesize(&source_trace, &synthetic_trace, repeat)?;
                eprintln!("Synthetic trace generated at {}", synthetic_trace.display());
            }
        },
        Commands::Serve {
            port,
            bucket,
            prefix,
            simulator,
            simulator_hosts,
            simulator_segment_secs,
            simulator_repetitions,
            simulator_features,
            simulator_symbols,
            local_dir,
            dev,
            local,
            agg,
            agg_source_dir,
            agg_output_dir,
            agg_output_bucket,
            agg_output_prefix,
            agg_segment_secs,
            enable_upload,
        } => {
            // Logging and per-request metrics are process-global concerns owned
            // by the binary, not by app assembly (see `crate::build_app`).
            // `--local` selects human-readable logs + metrique's local metrics
            // format; the default (deployed) is JSON logs + EMF. Hold the
            // metrics handle for the life of the server.
            crate::init_tracing(local);
            let _metrics = crate::attach_request_metrics(local);

            let app = if let Some(mode) = simulator {
                if mode == SimulatorModeArg::Demo && simulator_features.is_some() {
                    anyhow::bail!("--simulator-features applies only to synthetic simulator mode");
                }
                if mode == SimulatorModeArg::Demo && simulator_symbols.is_some() {
                    anyhow::bail!("--simulator-symbols applies only to synthetic simulator mode");
                }
                let features = build_simulator_features(simulator_features.as_deref())?;
                let symbol_mode = match simulator_symbols {
                    Some(SimulatorSymbolModeArg::Realistic) => {
                        crate::simulator::SimulatorSymbolMode::Realistic
                    }
                    Some(SimulatorSymbolModeArg::Anonymous) | None => {
                        crate::simulator::SimulatorSymbolMode::Anonymous
                    }
                };
                let trace_mode = match mode {
                    SimulatorModeArg::Synthetic => crate::simulator::SimulatorTraceMode::Synthetic,
                    SimulatorModeArg::Demo => crate::simulator::SimulatorTraceMode::DemoReplay,
                };
                let config = crate::simulator::SimulatorConfig::builder()
                    .trace_mode(trace_mode)
                    .hosts(simulator_hosts)
                    .segment_duration(Duration::from_secs(simulator_segment_secs))
                    .repetitions_per_segment(simulator_repetitions)
                    .features(features)
                    .symbol_mode(symbol_mode)
                    .prefix(prefix.unwrap_or_else(|| "traces".to_string()))
                    .dev(dev)
                    .build();
                crate::simulator::build_simulator_app(config).await?
            } else {
                crate::build_app(crate::ViewerConfig {
                    bucket,
                    prefix,
                    local_dir,
                    dev,
                    agg,
                    agg_source_dir,
                    agg_output_dir,
                    agg_output_bucket,
                    agg_output_prefix,
                    agg_segment_secs,
                    enable_upload,
                })
                .await?
            };

            let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
            tracing::info!(port, "dial9-viewer listening");
            println!("\n  → http://localhost:{port}\n");
            axum::serve(listener, app)
                .with_graceful_shutdown(crate::shutdown_signal())
                .await?;
            return Ok(());
        }
        Commands::Report { action } => match action {
            ReportAction::Serve { path, port } => {
                let canon = std::fs::canonicalize(&path).map_err(|e| {
                    anyhow::anyhow!("report path '{}' not found: {e}", path.display())
                })?;
                if !canon.is_dir() {
                    anyhow::bail!("report path '{}' is not a directory", canon.display());
                }
                let app = crate::report_serve_router(&canon);
                let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
                eprintln!("Serving report from {}", canon.display());
                let entry = if canon.join("report.html").exists() {
                    "report.html"
                } else {
                    ""
                };
                println!("\n  → http://localhost:{port}/{entry}\n");
                axum::serve(listener, app).await?;
            }
        },
    }
    Ok(())
}

fn build_simulator_features(
    selected: Option<&[SimulatorFeatureArg]>,
) -> anyhow::Result<crate::simulator::SimulatorFeatures> {
    let Some(selected) = selected else {
        return Ok(crate::simulator::SimulatorFeatures::default());
    };
    let none = selected.contains(&SimulatorFeatureArg::None);
    if none && selected.len() != 1 {
        anyhow::bail!("simulator feature `none` cannot be combined with other feature groups");
    }
    let enabled = |feature| !none && selected.contains(&feature);
    Ok(crate::simulator::SimulatorFeatures::builder()
        .cpu(enabled(SimulatorFeatureArg::Cpu))
        .scheduling(enabled(SimulatorFeatureArg::Scheduling))
        .tasks(enabled(SimulatorFeatureArg::Tasks))
        .spans(enabled(SimulatorFeatureArg::Spans))
        .memory(enabled(SimulatorFeatureArg::Memory))
        .resources(enabled(SimulatorFeatureArg::Resources))
        .custom_events(enabled(SimulatorFeatureArg::CustomEvents))
        .build())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serve_without_simulator_still_parses() {
        let cli = Cli::try_parse_from(["dial9", "serve"]).unwrap();
        let Commands::Serve { simulator, .. } = cli.command else {
            panic!("expected serve command");
        };
        assert_eq!(simulator, None);
    }

    #[test]
    fn simulator_flag_defaults_to_synthetic_without_a_value() {
        let cli = Cli::try_parse_from(["dial9", "serve", "--simulator"]).unwrap();
        let Commands::Serve {
            simulator,
            simulator_hosts,
            ..
        } = cli.command
        else {
            panic!("expected serve command");
        };
        assert_eq!(simulator, Some(SimulatorModeArg::Synthetic));
        assert_eq!(simulator_hosts, 3);
    }

    #[test]
    fn simulator_parses_host_and_feature_configuration() {
        let cli = Cli::try_parse_from([
            "dial9",
            "serve",
            "--simulator",
            "synthetic",
            "--simulator-hosts",
            "8",
            "--simulator-features",
            "cpu,spans",
        ])
        .unwrap();
        let Commands::Serve {
            simulator,
            simulator_hosts,
            simulator_features,
            ..
        } = cli.command
        else {
            panic!("expected serve command");
        };
        assert_eq!(simulator, Some(SimulatorModeArg::Synthetic));
        assert_eq!(simulator_hosts, 8);
        assert_eq!(
            simulator_features,
            Some(vec![SimulatorFeatureArg::Cpu, SimulatorFeatureArg::Spans])
        );
    }

    #[test]
    fn simulator_parses_realistic_symbol_mode() {
        let cli = Cli::try_parse_from([
            "dial9",
            "serve",
            "--simulator",
            "synthetic",
            "--simulator-symbols",
            "realistic",
        ])
        .unwrap();
        let Commands::Serve {
            simulator_symbols, ..
        } = cli.command
        else {
            panic!("expected serve command");
        };
        assert_eq!(simulator_symbols, Some(SimulatorSymbolModeArg::Realistic));
    }

    #[test]
    fn simulator_rejects_real_storage_inputs() {
        let error = Cli::try_parse_from([
            "dial9",
            "serve",
            "--simulator",
            "demo",
            "--bucket",
            "real-bucket",
        ])
        .unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
    }
}
