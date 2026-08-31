//! Five-field cron (`minute hour dom month dow`). Used by house routines.

use chrono::{Datelike, NaiveDateTime, Timelike};
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum CronError {
    #[error("cron needs five fields (minute hour dom month dow), got {0:?}")]
    Fields(String),
    #[error("cron field {field}: {detail}")]
    Field { field: &'static str, detail: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cron {
    minute: Field,
    hour: Field,
    dom: Field,
    month: Field,
    dow: Field,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Field {
    /// True when the field was `*` (or equivalent all-values).
    any: bool,
    values: Vec<u32>,
}

impl Cron {
    pub fn parse(src: &str) -> Result<Self, CronError> {
        let parts: Vec<&str> = src.split_whitespace().collect();
        if parts.len() != 5 {
            return Err(CronError::Fields(src.to_string()));
        }
        Ok(Self {
            minute: parse_field("minute", parts[0], 0, 59)?,
            hour: parse_field("hour", parts[1], 0, 23)?,
            dom: parse_field("dom", parts[2], 1, 31)?,
            month: parse_field("month", parts[3], 1, 12)?,
            dow: parse_dow(parts[4])?,
        })
    }

    pub fn matches(&self, at: NaiveDateTime) -> bool {
        if !self.minute.contains(at.minute()) || !self.hour.contains(at.hour()) {
            return false;
        }
        if !self.month.contains(at.month()) {
            return false;
        }
        let dom = self.dom.contains(at.day());
        // chrono: 0 = Monday .. 6 = Sunday. Cron: 0/7 = Sunday, 1 = Monday.
        let cron_dow = at.weekday().num_days_from_sunday();
        let dow = self.dow.contains(cron_dow);
        if self.dom.any || self.dow.any {
            dom && dow
        } else {
            // Classic cron: both restricted → match if either hits.
            dom || dow
        }
    }

    /// Short English, e.g. `Weekdays at 9:28 AM`.
    pub fn describe(&self, src: &str) -> String {
        describe(src)
    }
}

impl Field {
    fn contains(&self, v: u32) -> bool {
        self.any || self.values.binary_search(&v).is_ok()
    }
}

fn parse_dow(src: &str) -> Result<Field, CronError> {
    let mut field = parse_field("dow", src, 0, 7)?;
    // 7 is Sunday, same as 0.
    if field.values.contains(&7) {
        if !field.values.contains(&0) {
            field.values.push(0);
        }
        field.values.retain(|v| *v != 7);
        field.values.sort();
        field.values.dedup();
        field.any = field.values.len() == 7;
    }
    Ok(field)
}

fn parse_field(name: &'static str, src: &str, min: u32, max: u32) -> Result<Field, CronError> {
    let mut values = Vec::new();
    for part in src.split(',') {
        let part = part.trim();
        if part.is_empty() {
            return Err(CronError::Field {
                field: name,
                detail: "empty".into(),
            });
        }
        let (range, step) = match part.split_once('/') {
            Some((range, step)) => {
                let step: u32 = step.parse().map_err(|_| CronError::Field {
                    field: name,
                    detail: format!("bad step {step}"),
                })?;
                if step == 0 {
                    return Err(CronError::Field {
                        field: name,
                        detail: "step cannot be 0".into(),
                    });
                }
                (range, step)
            }
            None => (part, 1),
        };
        let (lo, hi) = if range == "*" {
            (min, max)
        } else if let Some((a, b)) = range.split_once('-') {
            let lo = parse_num(name, a, min, max)?;
            let hi = parse_num(name, b, min, max)?;
            if lo > hi {
                return Err(CronError::Field {
                    field: name,
                    detail: format!("{lo}-{hi} is inverted"),
                });
            }
            (lo, hi)
        } else {
            let n = parse_num(name, range, min, max)?;
            (n, n)
        };
        let mut v = lo;
        while v <= hi {
            values.push(v);
            v = v.saturating_add(step);
            if step == 0 {
                break;
            }
        }
    }
    values.sort();
    values.dedup();
    let any = values.len() as u32 == (max - min + 1);
    Ok(Field { any, values })
}

fn parse_num(name: &'static str, src: &str, min: u32, max: u32) -> Result<u32, CronError> {
    let n: u32 = src.parse().map_err(|_| CronError::Field {
        field: name,
        detail: format!("not a number: {src}"),
    })?;
    if n < min || n > max {
        return Err(CronError::Field {
            field: name,
            detail: format!("{n} out of {min}–{max}"),
        });
    }
    Ok(n)
}

fn describe(src: &str) -> String {
    let parts: Vec<&str> = src.split_whitespace().collect();
    if parts.len() != 5 {
        return src.to_string();
    }
    let minute = parts[0];
    let hour = parts[1];
    let dom = parts[2];
    let month = parts[3];
    let dow = parts[4];

    if minute == "*" && hour == "*" && dom == "*" && month == "*" && dow == "*" {
        return "Every minute".into();
    }
    if minute != "*" && hour == "*" && dom == "*" && month == "*" && dow == "*" {
        return format!("Every hour at :{minute:0>2}");
    }

    match (parse_clock(minute, hour), weekday_phrase(dow, dom, month)) {
        (Some(clock), Some(days)) => format!("{days} at {clock}"),
        (Some(clock), None) if dom == "*" && month == "*" && dow == "*" => {
            format!("Every day at {clock}")
        }
        (Some(clock), None) => format!("At {clock} ({src})"),
        _ => src.to_string(),
    }
}

fn parse_clock(minute: &str, hour: &str) -> Option<String> {
    let m: u32 = minute.parse().ok()?;
    let h: u32 = hour.parse().ok()?;
    let (h12, am) = match h {
        0 => (12, true),
        1..=11 => (h, true),
        12 => (12, false),
        _ => (h - 12, false),
    };
    Some(format!("{h12}:{m:02} {}", if am { "AM" } else { "PM" }))
}

fn weekday_phrase(dow: &str, dom: &str, month: &str) -> Option<&'static str> {
    if dom != "*" || month != "*" {
        return None;
    }
    match dow {
        "1-5" => Some("Weekdays"),
        "0,6" | "6,0" | "6,7" | "0,7" | "7,0" | "7,6" => Some("Weekends"),
        "0" | "7" => Some("Sundays"),
        "1" => Some("Mondays"),
        "2" => Some("Tuesdays"),
        "3" => Some("Wednesdays"),
        "4" => Some("Thursdays"),
        "5" => Some("Fridays"),
        "6" => Some("Saturdays"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, mo, d)
            .unwrap()
            .and_hms_opt(h, mi, 0)
            .unwrap()
    }

    #[test]
    fn weekdays_at_928() {
        let c = Cron::parse("28 9 * * 1-5").unwrap();
        assert!(c.matches(at(2026, 8, 31, 9, 28))); // Monday
        assert!(!c.matches(at(2026, 8, 30, 9, 28))); // Sunday
        assert!(!c.matches(at(2026, 8, 31, 9, 29)));
        assert_eq!(c.describe("28 9 * * 1-5"), "Weekdays at 9:28 AM");
    }

    #[test]
    fn every_minute() {
        let c = Cron::parse("* * * * *").unwrap();
        assert!(c.matches(at(2026, 1, 1, 0, 0)));
        assert_eq!(c.describe("* * * * *"), "Every minute");
    }

    #[test]
    fn lists_and_steps() {
        let c = Cron::parse("0,30 */2 * * *").unwrap();
        assert!(c.matches(at(2026, 1, 1, 0, 0)));
        assert!(c.matches(at(2026, 1, 1, 2, 30)));
        assert!(!c.matches(at(2026, 1, 1, 1, 0)));
    }

    #[test]
    fn sunday_seven() {
        let c = Cron::parse("0 0 * * 7").unwrap();
        assert!(c.matches(at(2026, 8, 30, 0, 0))); // Sunday
        assert!(!c.matches(at(2026, 8, 31, 0, 0)));
    }

    #[test]
    fn rejects_wrong_arity() {
        assert!(Cron::parse("0 9 * *").is_err());
    }
}
