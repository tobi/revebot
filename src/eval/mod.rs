//! Eval catalog runner.
//!
//! Cases live in `evals/cases/**/*.yaml`. `make eval` / `revebot eval` (and
//! `cargo test --test eval`) run the offline suite without a microVM or a model
//! key. `--live` uses `OPENROUTER_API_KEY` unless `REVEBOT_EVAL_MODEL` is set.

pub mod case;
pub mod files;
pub mod grade;
pub mod harness;
pub mod http;
pub mod report;
pub mod run;
pub mod unit;

pub use case::{Case, Mode, RunnerKind};
pub use report::Report;
pub use run::{Options, default_report_path, list, run};
