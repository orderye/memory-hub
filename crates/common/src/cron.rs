//! 5 段 cron 表达式解析与到点判定（分 时 日 月 周）
//!
//! 支持：
//! - 字面量（`30`）
//! - 通配（`*`）
//! - 步长（`*/15`、`10-50/10`）
//! - 区间（`9-18`）
//! - 列表（`1,15,30`）
//!
//! 约束（借鉴标准 cron 语义）：
//! - 日（day-of-month）与周（day-of-week）同时被限定时拒绝（`* * 1 * 1` 非法）
//! - 每段取值越界即拒绝，保证启动期 fail-fast

use std::fmt;

/// 单个字段的取值集合（用 64 位位图表示，按位置索引）
#[derive(Debug, Clone, PartialEq, Eq)]
struct FieldSet(u64);

impl FieldSet {
    fn empty() -> Self {
        Self(0)
    }

    fn insert(&mut self, v: u32) {
        if v < 64 {
            self.0 |= 1 << v;
        }
    }

    fn contains(&self, v: u32) -> bool {
        v < 64 && (self.0 >> v) & 1 == 1
    }

    fn min(&self) -> Option<u32> {
        (0..64).find(|&v| self.contains(v))
    }
}

/// 解析后的 cron 表达式
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronSchedule {
    minutes: FieldSet,
    hours: FieldSet,
    days_of_month: FieldSet,
    months: FieldSet,
    days_of_week: FieldSet,
    /// 日与周均为 `*` 时标准 cron 用 OR 语义；两者不可同时限定
    dom_restricted: bool,
    dow_restricted: bool,
}

/// cron 解析错误
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CronError {
    FieldCount(usize),
    OutOfRange { field: &'static str, value: String },
    BadSyntax { field: &'static str, raw: String },
    DomDowConflict,
}

impl fmt::Display for CronError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CronError::FieldCount(n) => write!(f, "需要 5 段（分 时 日 月 周），当前 {} 段", n),
            CronError::OutOfRange { field, value } => {
                write!(f, "字段 {} 的值 {} 超出合法范围", field, value)
            }
            CronError::BadSyntax { field, raw } => write!(f, "字段 {} 的语法非法: {}", field, raw),
            CronError::DomDowConflict => {
                write!(
                    f,
                    "日（day-of-month）与周（day-of-week）不能同时限定，标准 cron 不允许"
                )
            }
        }
    }
}

impl std::error::Error for CronError {}

impl CronSchedule {
    /// 解析 5 段 cron 表达式
    pub fn parse(expr: &str) -> Result<Self, CronError> {
        let parts: Vec<&str> = expr.split_whitespace().collect();
        if parts.len() != 5 {
            return Err(CronError::FieldCount(parts.len()));
        }

        let minutes = parse_field(parts[0], 0, 59, "分钟")?;
        let hours = parse_field(parts[1], 0, 23, "小时")?;
        let days_of_month = parse_field(parts[2], 1, 31, "日")?;
        let months = parse_field(parts[3], 1, 12, "月")?;
        let days_of_week = parse_field(parts[4], 0, 6, "周")?;

        let dom_restricted = parts[2] != "*";
        let dow_restricted = parts[4] != "*";
        if dom_restricted && dow_restricted {
            return Err(CronError::DomDowConflict);
        }

        Ok(Self {
            minutes,
            hours,
            days_of_month,
            months,
            days_of_week,
            dom_restricted,
            dow_restricted,
        })
    }

    /// 该时刻是否命中表达式（分钟粒度，忽略秒）
    pub fn matches(&self, dt: chrono::DateTime<chrono::Utc>) -> bool {
        use chrono::{Datelike, Timelike};
        self.minutes.contains(dt.minute())
            && self.hours.contains(dt.hour())
            && self.months.contains(dt.month())
            && self.day_matches(dt)
    }

    fn day_matches(&self, dt: chrono::DateTime<chrono::Utc>) -> bool {
        use chrono::Datelike;
        if self.dom_restricted {
            return self.days_of_month.contains(dt.day());
        }
        if self.dow_restricted {
            // cron 的周日是 0，chrono 的周日也是 0
            return self
                .days_of_week
                .contains(dt.weekday().num_days_from_sunday());
        }
        true
    }

    /// 从 `from` 之后（含 `from` 所在分钟的下一秒起）找到下一次到点时间
    pub fn next_after(
        &self,
        from: chrono::DateTime<chrono::Utc>,
    ) -> Option<chrono::DateTime<chrono::Utc>> {
        use chrono::Duration;
        // 从下一分钟开始逐分钟探测（上限 366 天，覆盖所有合法表达式）
        let mut candidate = from + Duration::minutes(1);
        let horizon = from + Duration::days(366);
        while candidate <= horizon {
            if self.matches(candidate) {
                return Some(candidate);
            }
            candidate += Duration::minutes(1);
        }
        None
    }
}

/// 解析单个字段：`*` / 字面量 / 区间 / 步长 / 列表
fn parse_field(raw: &str, min: u32, max: u32, name: &'static str) -> Result<FieldSet, CronError> {
    let mut set = FieldSet::empty();
    for part in raw.split(',') {
        let part = part.trim();
        if part.is_empty() {
            return Err(CronError::BadSyntax {
                field: name,
                raw: raw.to_string(),
            });
        }
        parse_part(part, min, max, name, &mut set)?;
    }
    if set.min().is_none() {
        return Err(CronError::BadSyntax {
            field: name,
            raw: raw.to_string(),
        });
    }
    Ok(set)
}

fn parse_part(
    part: &str,
    min: u32,
    max: u32,
    name: &'static str,
    set: &mut FieldSet,
) -> Result<(), CronError> {
    let (range_part, step) = match part.split_once('/') {
        Some((r, s)) => {
            let step: u32 = s.trim().parse().map_err(|_| CronError::BadSyntax {
                field: name,
                raw: part.to_string(),
            })?;
            if step == 0 {
                return Err(CronError::BadSyntax {
                    field: name,
                    raw: part.to_string(),
                });
            }
            (r.trim(), Some(step))
        }
        None => (part, None),
    };

    let (lo, hi) = if range_part == "*" {
        (min, max)
    } else if let Some((a, b)) = range_part.split_once('-') {
        let a: u32 = a.trim().parse().map_err(|_| CronError::BadSyntax {
            field: name,
            raw: part.to_string(),
        })?;
        let b: u32 = b.trim().parse().map_err(|_| CronError::BadSyntax {
            field: name,
            raw: part.to_string(),
        })?;
        if a > b {
            return Err(CronError::BadSyntax {
                field: name,
                raw: part.to_string(),
            });
        }
        (a, b)
    } else {
        let v: u32 = range_part.parse().map_err(|_| CronError::BadSyntax {
            field: name,
            raw: part.to_string(),
        })?;
        match step {
            // `5/10` 表示从 5 起每 10 一次直到上界
            Some(_) => (v, max),
            None => (v, v),
        }
    };

    if lo < min || hi > max {
        return Err(CronError::OutOfRange {
            field: name,
            value: part.to_string(),
        });
    }

    let step = step.unwrap_or(1);
    let mut v = lo;
    while v <= hi {
        set.insert(v);
        v += step;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn test_parse_daily_expression() {
        let s = CronSchedule::parse("0 3 * * *").unwrap();
        let dt = chrono::Utc.with_ymd_and_hms(2026, 10, 11, 3, 0, 0).unwrap();
        assert!(s.matches(dt));
        let other = chrono::Utc.with_ymd_and_hms(2026, 10, 11, 4, 0, 0).unwrap();
        assert!(!s.matches(other));
    }

    #[test]
    fn test_parse_step_and_list() {
        let s = CronSchedule::parse("*/15 9-18 * * *").unwrap();
        assert!(s.minutes.contains(0));
        assert!(s.minutes.contains(15));
        assert!(!s.minutes.contains(7));
        assert!(s.hours.contains(9));
        assert!(s.hours.contains(18));
        assert!(!s.hours.contains(19));

        let s2 = CronSchedule::parse("1,15,30 0 * * *").unwrap();
        assert!(s2.minutes.contains(1) && s2.minutes.contains(30));
        assert!(!s2.minutes.contains(2));
    }

    #[test]
    fn test_rejects_out_of_range() {
        assert_eq!(
            CronSchedule::parse("99 * * * *"),
            Err(CronError::OutOfRange {
                field: "分钟",
                value: "99".to_string()
            })
        );
        assert!(CronSchedule::parse("* 25 * * *").is_err());
        assert!(CronSchedule::parse("* * 32 * *").is_err());
        assert!(CronSchedule::parse("* * * 13 *").is_err());
        assert!(CronSchedule::parse("* * * * 7").is_err());
    }

    #[test]
    fn test_rejects_bad_syntax() {
        assert!(CronSchedule::parse("* * * *").is_err());
        assert!(CronSchedule::parse("* * * * * *").is_err());
        assert!(CronSchedule::parse("*/0 * * * *").is_err());
        assert!(CronSchedule::parse("a * * * *").is_err());
        assert!(CronSchedule::parse("5-1 * * * *").is_err());
    }

    #[test]
    fn test_dom_dow_conflict_rejected() {
        assert_eq!(
            CronSchedule::parse("0 0 1 * 1"),
            Err(CronError::DomDowConflict)
        );
        // 只限定周几是允许的
        assert!(CronSchedule::parse("0 0 * * 1").is_ok());
        // 只限定日也是允许的
        assert!(CronSchedule::parse("0 0 1 * *").is_ok());
    }

    #[test]
    fn test_next_after() {
        let s = CronSchedule::parse("0 3 * * *").unwrap();
        let from = chrono::Utc
            .with_ymd_and_hms(2026, 10, 11, 5, 30, 0)
            .unwrap();
        let next = s.next_after(from).unwrap();
        assert_eq!(
            next,
            chrono::Utc.with_ymd_and_hms(2026, 10, 12, 3, 0, 0).unwrap()
        );

        // 每分钟表达式：下一秒的下一分钟即到点
        let every = CronSchedule::parse("* * * * *").unwrap();
        let next2 = every.next_after(from).unwrap();
        assert_eq!(
            next2,
            chrono::Utc
                .with_ymd_and_hms(2026, 10, 11, 5, 31, 0)
                .unwrap()
        );
    }

    #[test]
    fn test_dow_semantics() {
        // 2026-10-11 是周日
        let s = CronSchedule::parse("0 2 * * 0").unwrap();
        let sunday = chrono::Utc.with_ymd_and_hms(2026, 10, 11, 2, 0, 0).unwrap();
        let monday = chrono::Utc.with_ymd_and_hms(2026, 10, 12, 2, 0, 0).unwrap();
        assert!(s.matches(sunday));
        assert!(!s.matches(monday));
    }
}
