//! JSON and markdown reports.

use std::path::Path;

use serde::{Deserialize, Serialize};

use super::grade::GradeResult;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Passed,
    Failed,
    Skipped,
    Error,
    /// Soft assertion missed; fatal only under `--strict`.
    Scored,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaseResult {
    pub id: String,
    pub suite: String,
    pub status: Status,
    pub duration_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub grades: Vec<GradeResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub started_at: String,
    pub catalog: String,
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    pub errored: usize,
    #[serde(default)]
    pub scored: usize,
    pub duration_ms: u64,
    pub cases: Vec<CaseResult>,
}

impl Report {
    pub fn from_cases(
        catalog: &Path,
        started: chrono::DateTime<chrono::Utc>,
        cases: Vec<CaseResult>,
    ) -> Self {
        let duration_ms = cases.iter().map(|c| c.duration_ms).sum();
        let passed = cases.iter().filter(|c| c.status == Status::Passed).count();
        let failed = cases.iter().filter(|c| c.status == Status::Failed).count();
        let skipped = cases.iter().filter(|c| c.status == Status::Skipped).count();
        let errored = cases.iter().filter(|c| c.status == Status::Error).count();
        let scored = cases.iter().filter(|c| c.status == Status::Scored).count();
        Self {
            started_at: started.to_rfc3339(),
            catalog: catalog.display().to_string(),
            passed,
            failed,
            skipped,
            errored,
            scored,
            duration_ms,
            cases,
        }
    }

    pub fn ok(&self) -> bool {
        self.failed == 0 && self.errored == 0
    }

    pub fn write_json(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }

    pub fn write_junit(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
        xml.push_str(&format!(
            "<testsuite name=\"revebot-eval\" tests=\"{}\" failures=\"{}\" errors=\"{}\" skipped=\"{}\">\n",
            self.cases.len(),
            self.failed,
            self.errored,
            self.skipped
        ));
        for case in &self.cases {
            xml.push_str(&format!(
                "  <testcase name=\"{}\" classname=\"{}\" time=\"{:.3}\">",
                xml_escape(&case.id),
                xml_escape(&case.suite),
                case.duration_ms as f64 / 1000.0
            ));
            match case.status {
                Status::Failed => {
                    let msg = case
                        .grades
                        .iter()
                        .filter(|g| !g.passed)
                        .map(|g| format!("{}: {}", g.grader, g.detail))
                        .collect::<Vec<_>>()
                        .join("; ");
                    xml.push_str(&format!("<failure message=\"{}\"/>", xml_escape(&msg)));
                }
                Status::Error => xml.push_str(&format!(
                    "<error message=\"{}\"/>",
                    xml_escape(case.error.as_deref().unwrap_or("error"))
                )),
                Status::Skipped => xml.push_str(&format!(
                    "<skipped message=\"{}\"/>",
                    xml_escape(case.skip_reason.as_deref().unwrap_or("skipped"))
                )),
                Status::Passed | Status::Scored => {}
            }
            xml.push_str("</testcase>\n");
        }
        xml.push_str("</testsuite>\n");
        std::fs::write(path, xml)?;
        Ok(())
    }

    pub fn markdown(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "# eval report\n\n{} passed, {} failed, {} skipped, {} errored  ({})\n\n",
            self.passed,
            self.failed,
            self.skipped,
            self.errored,
            format_ms(self.duration_ms),
        ));
        out.push_str("| status | id | suite | time |\n|---|---|---|---|\n");
        for case in &self.cases {
            let mark = match case.status {
                Status::Passed => "pass",
                Status::Failed => "FAIL",
                Status::Skipped => "skip",
                Status::Error => "ERR",
                Status::Scored => "score",
            };
            out.push_str(&format!(
                "| {mark} | `{}` | {} | {} |\n",
                case.id,
                case.suite,
                format_ms(case.duration_ms)
            ));
            if let Some(reason) = &case.skip_reason {
                out.push_str(&format!("|  | _{reason}_ |  |  |\n"));
            }
            if let Some(err) = &case.error {
                out.push_str(&format!("|  | `{err}` |  |  |\n"));
            }
            for grade in case.grades.iter().filter(|g| !g.passed) {
                out.push_str(&format!(
                    "|  | grader `{}`: {} |  |  |\n",
                    grade.grader, grade.detail
                ));
            }
        }
        out
    }

    pub fn print(&self) {
        println!(
            "evals  {} passed  {} failed  {} scored  {} skipped  {} errored  {}",
            self.passed,
            self.failed,
            self.scored,
            self.skipped,
            self.errored,
            format_ms(self.duration_ms),
        );
        for case in &self.cases {
            let glyph = match case.status {
                Status::Passed => "✓",
                Status::Failed => "✗",
                Status::Skipped => "·",
                Status::Error => "!",
                Status::Scored => "~",
            };
            println!(
                "  {glyph} {:<42} {:>8}",
                case.id,
                format_ms(case.duration_ms)
            );
            if let Some(reason) = &case.skip_reason {
                println!("      skip: {reason}");
            }
            if let Some(err) = &case.error {
                println!("      error: {err}");
            }
            for grade in case.grades.iter().filter(|g| !g.passed) {
                println!("      {}: {}", grade.grader, grade.detail);
            }
        }
    }

    pub fn diff(&self, baseline: &Report) -> Vec<String> {
        let mut lines = Vec::new();
        for case in &self.cases {
            let prev = baseline.cases.iter().find(|c| c.id == case.id);
            match (prev.map(|p| p.status), case.status) {
                (Some(Status::Passed), Status::Failed | Status::Error) => {
                    lines.push(format!("REGRESSION {}", case.id));
                }
                (Some(Status::Failed | Status::Error), Status::Passed) => {
                    lines.push(format!("fixed      {}", case.id));
                }
                (None, Status::Passed) => lines.push(format!("new pass   {}", case.id)),
                (None, Status::Failed | Status::Error) => {
                    lines.push(format!("new fail   {}", case.id));
                }
                _ => {}
            }
        }
        lines
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn format_ms(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else {
        format!("{:.1}s", ms as f64 / 1000.0)
    }
}
