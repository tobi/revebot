//! The `revebot` command.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use reve::house::House;
use reve::progress::Spinner;
use reve::project::{self, Project};
use reve::sandbox::{ExecOptions, Sandbox};

#[derive(Parser)]
#[command(
    name = "revebot",
    version,
    about = "A local house of durable bots: Rust core, Lua scripting, mandatory microVM"
)]
struct Cli {
    /// Bind address for the house server (default 127.0.0.1:7420).
    #[arg(long, global = true, default_value = "127.0.0.1:7420")]
    bind: String,
    /// Omitted: boot the house server.
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)]
enum Command {
    /// Scaffold a house directory.
    Init {
        /// Where to create it (default: here).
        dir: Option<PathBuf>,
    },
    /// Run a command inside this house's microVM.
    Exec {
        /// The command, as it would be typed in a shell.
        command: Vec<String>,
    },
    /// Run one of this house's Lua tools.
    Tool {
        /// The tool name, as declared by `tool("name", ...)`.
        name: Option<String>,
        /// Arguments, as a JSON object.
        #[arg(long, default_value = "{}")]
        args: String,
    },
    /// Show what this house is configured to do.
    Info,
    /// Boot the house HTTP server (also the default with no subcommand).
    Serve,
    /// Open the terminal UI for the first bot.
    Tui,
    /// Run the eval catalog in ./evals.
    Eval {
        /// Id, suite, or path prefix (e.g. `harness` or `house.init`).
        filters: Vec<String>,
        /// Catalog root (default: ./evals, or the crate evals/ when that is missing).
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Only these suites.
        #[arg(long)]
        suite: Vec<String>,
        /// Case id or prefix.
        #[arg(long)]
        case: Vec<String>,
        /// Keep evals carrying any of these tags.
        #[arg(long)]
        tag: Vec<String>,
        /// Drop evals carrying any of these tags.
        #[arg(long)]
        exclude_tag: Vec<String>,
        /// Include live (real model) cases.
        #[arg(long)]
        live: bool,
        /// Include microVM cases.
        #[arg(long)]
        microvm: bool,
        /// Soft misses fail the process.
        #[arg(long)]
        strict: bool,
        /// Drive a running house instead of the in-process harness.
        #[arg(long)]
        url: Option<String>,
        /// Bearer token for `--url`.
        #[arg(long)]
        token: Option<String>,
        /// Print cases instead of running.
        #[arg(long)]
        list: bool,
        /// JSON report path.
        #[arg(long)]
        report: Option<PathBuf>,
        /// Write JUnit XML.
        #[arg(long)]
        junit: Option<PathBuf>,
        /// Diff against this previous JSON report.
        #[arg(long)]
        baseline: Option<PathBuf>,
        /// Copy this run to evals/baselines/offline.json.
        #[arg(long)]
        save_baseline: bool,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("\x1b[31mrevebot:\x1b[0m {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> anyhow::Result<ExitCode> {
    let cli = Cli::parse();
    let Some(command) = cli.command else {
        return serve_house(cli.bind).await;
    };
    match command {
        Command::Init { dir } => {
            let root = dir.unwrap_or(std::env::current_dir()?);
            let report = project::init(&root)?;
            println!("\x1b[1minitialised {}\x1b[0m", report.root.display());
            for name in &report.created {
                println!("  \x1b[32m+\x1b[0m {name}");
            }
            for name in &report.unchanged {
                println!("  \x1b[2m· {name} (unchanged)\x1b[0m");
            }
            for name in &report.changed {
                println!("  \x1b[2m· {name} (edited; kept)\x1b[0m");
            }
            println!();
            println!("  run \x1b[1mrevebot\x1b[0m here to start the house");
            Ok(ExitCode::SUCCESS)
        }
        Command::Eval {
            filters,
            dir,
            suite,
            case,
            tag,
            exclude_tag,
            live,
            microvm,
            strict,
            url,
            token,
            list,
            report,
            junit,
            baseline,
            save_baseline,
        } => {
            run_eval(EvalArgs {
                filters,
                dir,
                suite,
                case,
                tag,
                exclude_tag,
                live,
                microvm,
                strict,
                url,
                token,
                list,
                report,
                junit,
                baseline,
                save_baseline,
            })
            .await
        }
        Command::Serve => serve_house(cli.bind).await,
        Command::Tui => {
            let project = Project::load(std::env::current_dir()?)?;
            let sandbox = start_sandbox(&project).await?;
            let result = reve::tui::session::run(project, sandbox.clone()).await;
            let stopped = sandbox.stop().await;
            result?;
            stopped?;
            Ok(ExitCode::SUCCESS)
        }

        Command::Info => {
            let project = Project::load(std::env::current_dir()?)?;
            let agent = &project.runtime.agent;
            println!("root      {}", project.root.display());
            println!("model     {}", agent.model.as_deref().unwrap_or("(unset)"));
            println!(
                "thinking  {}",
                agent.thinking.as_deref().unwrap_or("(default)")
            );
            println!(
                "sandbox   {} ({} cpu, {}MB)",
                project.runtime.policy.image,
                project.runtime.policy.cpus,
                project.runtime.policy.memory
            );
            println!("egress    {}", project.runtime.policy.egress_summary());
            println!("tools     {}", tool_names(&project).join(", "));
            Ok(ExitCode::SUCCESS)
        }

        Command::Exec { command } => {
            if command.is_empty() {
                anyhow::bail!("nothing to run");
            }
            let (project, sandbox) = boot().await?;
            let _ = &project;
            let output = sandbox
                .exec(&command.join(" "), ExecOptions::default(), None)
                .await?;
            print!("{}", output.stdout);
            eprint!("{}", output.stderr);
            sandbox.stop().await?;
            Ok(if output.success {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }

        Command::Tool { name, args } => {
            let project = Project::load(std::env::current_dir()?)?;
            let Some(name) = name else {
                for tool in &project.runtime.tools {
                    println!("{:<20} {}", tool.name, tool.description);
                }
                return Ok(ExitCode::SUCCESS);
            };
            let parsed: serde_json::Value = serde_json::from_str(&args)?;
            let object = parsed
                .as_object()
                .ok_or_else(|| anyhow::anyhow!("--args must be a JSON object"))?
                .clone();

            let sandbox = start_sandbox(&project).await?;
            let result = project
                .runtime
                .call_tool(&name, object, sandbox.clone())
                .await;
            sandbox.stop().await?;
            println!("{}", result?);
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn tool_names(project: &Project) -> Vec<String> {
    let mut names: Vec<String> = project
        .runtime
        .tools
        .iter()
        .map(|t| t.name.clone())
        .collect();
    if names.is_empty() {
        names.push("(none)".into());
    }
    names
}

async fn boot() -> anyhow::Result<(Project, Arc<Sandbox>)> {
    let project = Project::load(std::env::current_dir()?)?;
    let sandbox = start_sandbox(&project).await?;
    Ok((project, sandbox))
}

async fn serve_house(bind: String) -> anyhow::Result<ExitCode> {
    let project = Project::load(std::env::current_dir()?)?;
    let spinner = Spinner::new();
    let house = House::boot(project, bind, &spinner).await?;
    let url = format!("http://{}/", house.bind());
    println!("\x1b[1mrevebot\x1b[0m house on {url}");
    println!("  token  {}", house.token());
    println!("  open   {url}?token={}", house.token());
    let serving = house.clone();
    let result = tokio::select! {
        result = reve::house::serve::serve(serving) => result,
        _ = tokio::signal::ctrl_c() => Ok(()),
    };
    let _ = house.shutdown().await;
    result?;
    Ok(ExitCode::SUCCESS)
}

struct EvalArgs {
    filters: Vec<String>,
    dir: Option<PathBuf>,
    suite: Vec<String>,
    case: Vec<String>,
    tag: Vec<String>,
    exclude_tag: Vec<String>,
    live: bool,
    microvm: bool,
    strict: bool,
    url: Option<String>,
    token: Option<String>,
    list: bool,
    report: Option<PathBuf>,
    junit: Option<PathBuf>,
    baseline: Option<PathBuf>,
    save_baseline: bool,
}

async fn run_eval(args: EvalArgs) -> anyhow::Result<ExitCode> {
    let catalog = args.dir.unwrap_or_else(default_eval_catalog);
    let mut ids = args.case;
    ids.extend(args.filters);
    let opts = reve::eval::Options {
        catalog: catalog.clone(),
        suites: args.suite,
        ids,
        tags: args.tag,
        exclude_tags: args.exclude_tag,
        live: args.live,
        microvm: args.microvm,
        strict: args.strict,
        url: args.url,
        token: args.token,
        jobs: 4,
    };
    if args.list {
        for c in reve::eval::list(&opts)? {
            println!(
                "{:<40} {:>8}  {}",
                c.id,
                format!("{:?}", c.mode).to_ascii_lowercase(),
                c.description
            );
        }
        return Ok(ExitCode::SUCCESS);
    }
    let report_body = match reve::eval::run(opts).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("revebot: {e}");
            return Ok(ExitCode::from(2));
        }
    };
    report_body.print();
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
    let artifact_dir = catalog.join("reports").join(stamp.to_string());
    std::fs::create_dir_all(&artifact_dir)?;
    let json_path = args
        .report
        .unwrap_or_else(|| artifact_dir.join("summary.json"));
    report_body.write_json(&json_path)?;
    println!("  report  {}", json_path.display());
    if let Some(junit) = args.junit {
        report_body.write_junit(&junit)?;
        println!("  junit   {}", junit.display());
    }
    if args.save_baseline {
        let dest = catalog.join("baselines").join("offline.json");
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(&json_path, &dest)?;
        println!("  baseline {}", dest.display());
    }
    if let Some(path) = args.baseline {
        let prev: reve::eval::Report = serde_json::from_slice(&std::fs::read(path)?)?;
        for line in report_body.diff(&prev) {
            println!("  {line}");
        }
    }
    Ok(if report_body.ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn default_eval_catalog() -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    if cwd.join("evals/cases").is_dir() {
        cwd.join("evals")
    } else {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("evals")
    }
}

async fn start_sandbox(project: &Project) -> anyhow::Result<Arc<Sandbox>> {
    let sandbox = Sandbox::start(
        project.runtime.policy.clone(),
        project.workspace(),
        project.state_dir(),
        &Spinner::new(),
    )
    .await?;
    Ok(Arc::new(sandbox))
}
