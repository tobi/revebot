//! Discover, filter, run, report.

use std::path::{Path, PathBuf};
use std::time::Instant;

use super::case::{self, Case, Grader, Mode, RunnerKind};
use super::files;
use super::grade::{self, GradeResult};
use super::harness;
use super::http;
use super::report::{CaseResult, Report, Status};
use super::unit;

#[derive(Debug, Clone)]
pub struct Options {
    pub catalog: PathBuf,
    pub suites: Vec<String>,
    pub ids: Vec<String>,
    pub tags: Vec<String>,
    pub exclude_tags: Vec<String>,
    pub live: bool,
    pub microvm: bool,
    pub strict: bool,
    pub url: Option<String>,
    pub token: Option<String>,
    pub jobs: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            catalog: PathBuf::from("evals"),
            suites: Vec::new(),
            ids: Vec::new(),
            tags: Vec::new(),
            exclude_tags: Vec::new(),
            live: false,
            microvm: false,
            strict: false,
            url: None,
            token: None,
            jobs: 4,
        }
    }
}

pub fn list(opts: &Options) -> anyhow::Result<Vec<Case>> {
    let mut cases = case::discover(&opts.catalog)?;
    cases.retain(|c| selected(c, opts));
    Ok(cases)
}

fn selected(case: &Case, opts: &Options) -> bool {
    if !opts.suites.is_empty() && !opts.suites.iter().any(|s| s == &case.suite) {
        return false;
    }
    if !opts.ids.is_empty()
        && !opts.ids.iter().any(|id| {
            case.id == *id
                || case.id.starts_with(id)
                || case.suite == *id
                || case.source.to_string_lossy().contains(id)
        })
    {
        return false;
    }
    if !opts.tags.is_empty() && !opts.tags.iter().any(|t| case.tags.iter().any(|c| c == t)) {
        return false;
    }
    if opts
        .exclude_tags
        .iter()
        .any(|t| case.tags.iter().any(|c| c == t))
    {
        return false;
    }
    true
}

pub async fn run(opts: Options) -> anyhow::Result<Report> {
    let started = chrono::Utc::now();
    let cases = list(&opts)?;
    if !opts.tags.is_empty() && cases.is_empty() {
        anyhow::bail!("no evals match --tag {:?}", opts.tags);
    }
    let mut results = Vec::new();
    for case in cases {
        results.push(run_one(&case, &opts).await);
    }
    Ok(Report::from_cases(&opts.catalog, started, results))
}

async fn run_one(case: &Case, opts: &Options) -> CaseResult {
    let t0 = Instant::now();
    if let Some(reason) = &case.skip {
        return skip(case, t0, reason);
    }
    match case.mode {
        Mode::Live if !opts.live && opts.url.is_none() => {
            return skip(case, t0, "pass --live (real model)");
        }
        Mode::Microvm if !opts.microvm => {
            return skip(case, t0, "pass --microvm (real guest)");
        }
        _ => {}
    }
    let result = if let Some(url) = &opts.url {
        let token = opts.token.as_deref().unwrap_or("");
        http::run(case, url, token).await.map(|trace| (trace, None))
    } else {
        match case.runner {
            RunnerKind::Harness => harness::run(case, opts.live)
                .await
                .map(|trace| (trace, None)),
            RunnerKind::Files => files::run_keep(case).map(|(trace, dir)| (trace, Some(dir))),
            RunnerKind::Unit => unit::run(case).map(|trace| (trace, None)),
            RunnerKind::LiveHouse => {
                return skip(
                    case,
                    t0,
                    "live-house runner needs --url against a running house",
                );
            }
        }
    };
    match result {
        Ok((trace, _keep)) => {
            let mut grades = grade::apply(&case.graders, &trace);
            for (spec, grade) in case.graders.iter().zip(grades.iter_mut()) {
                if let Grader::ClosedQa { criteria, at_least } = &spec.kind {
                    *grade = judge_closed_qa(&trace.final_text, criteria, *at_least).await;
                    grade.soft = spec.soft;
                }
            }
            let hard_fail = grades.iter().any(|g| !g.passed && !g.soft);
            let soft_fail = grades.iter().any(|g| !g.passed && g.soft);
            let status = if hard_fail || (soft_fail && opts.strict) {
                Status::Failed
            } else if soft_fail {
                Status::Scored
            } else {
                Status::Passed
            };
            CaseResult {
                id: case.id.clone(),
                suite: case.suite.clone(),
                status,
                duration_ms: u64::try_from(t0.elapsed().as_millis()).unwrap_or(u64::MAX),
                skip_reason: None,
                error: None,
                grades,
            }
        }
        Err(err) => CaseResult {
            id: case.id.clone(),
            suite: case.suite.clone(),
            status: Status::Error,
            duration_ms: u64::try_from(t0.elapsed().as_millis()).unwrap_or(u64::MAX),
            skip_reason: None,
            error: Some(err.to_string()),
            grades: Vec::new(),
        },
    }
}

async fn judge_closed_qa(reply: &str, criteria: &str, at_least: Option<f64>) -> GradeResult {
    let Some(model) = crate::eval::harness::try_live_model() else {
        return GradeResult {
            grader: "closed_qa".into(),
            passed: true,
            detail: "judge skipped (no model key)".into(),
            soft: true,
            score: None,
        };
    };
    let prompt = format!(
        "You grade an assistant reply. Criteria: {criteria}\n\nReply:\n{reply}\n\nAnswer YES or NO only."
    );
    let result = model
        .respond(
            crate::model::Request {
                context: &[],
                system: &prompt,
                tools: &[],
            },
            &|_| {},
        )
        .await;
    let answer = result.map(|a| a.text).unwrap_or_default();
    let yes = answer.to_ascii_uppercase().contains("YES");
    let score = if yes { 1.0 } else { 0.0 };
    let bar = at_least.unwrap_or(1.0);
    GradeResult {
        grader: "closed_qa".into(),
        passed: score + f64::EPSILON >= bar,
        detail: format!("{score:.0}% vs {bar:.0}%: {}", answer.trim()),
        soft: true,
        score: Some(score),
    }
}

fn skip(case: &Case, t0: Instant, reason: &str) -> CaseResult {
    CaseResult {
        id: case.id.clone(),
        suite: case.suite.clone(),
        status: Status::Skipped,
        duration_ms: u64::try_from(t0.elapsed().as_millis()).unwrap_or(u64::MAX),
        skip_reason: Some(reason.into()),
        error: None,
        grades: Vec::new(),
    }
}

pub fn default_report_path(catalog: &Path) -> PathBuf {
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
    catalog.join("reports").join(format!("{stamp}.json"))
}
