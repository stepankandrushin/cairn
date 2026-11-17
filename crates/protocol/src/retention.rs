//! A folder's retention policy as a person writes it (docs/protocol.md, "Retention"): tiers of how much
//! history is kept, one after another back from now, and how long a deleted file is kept once deleted.
//!
//! `all 2d, hourly 1d, daily 30d, weekly forever; trash 30d` keeps every revision of the last two days,
//! then the state at each hour's start for a day before that, at each day's start for 30 days before
//! that, and at each Monday's start for good; a deleted file's last revision stays 30 days after the
//! delete. The tiers go from finer to coarser, each at most once, so that a revision a policy gives up
//! today it gives up on every later day as well.

use std::fmt;

pub const HOUR: u64 = 3600;
pub const DAY: u64 = 24 * HOUR;
pub const WEEK: u64 = 7 * DAY;
pub const YEAR: u64 = 365 * DAY;

/// How long a deleted file's last revision is kept when a policy does not say.
pub const DEFAULT_TRASH: u64 = 30 * DAY;

/// What a tier keeps of the time it covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Every {
    /// Every revision.
    All,
    /// The state at the start of each hour, UTC.
    Hourly,
    /// The state at the start of each day, UTC.
    Daily,
    /// The state at the start of each Monday, UTC.
    Weekly,
}

impl Every {
    pub const ALL: [Every; 4] = [Every::All, Every::Hourly, Every::Daily, Every::Weekly];

    pub fn as_str(self) -> &'static str {
        match self {
            Every::All => "all",
            Every::Hourly => "hourly",
            Every::Daily => "daily",
            Every::Weekly => "weekly",
        }
    }

    /// The points in time this tier keeps the state at: every second a multiple of the first after
    /// the second, `None` for `All`, which keeps everything.
    pub fn grid(self) -> Option<(u64, u64)> {
        match self {
            Every::All => None,
            Every::Hourly => Some((HOUR, 0)),
            Every::Daily => Some((DAY, 0)),
            // 1970-01-05, the first Monday of the epoch.
            Every::Weekly => Some((WEEK, 4 * DAY)),
        }
    }
}

/// One tier: what it keeps, for how long after the tier before it ends; `None` for good.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Tier {
    pub every: Every,
    pub span: Option<u64>,
}

/// A folder's policy: its tiers, from now back, and how long a deleted file is kept, in seconds.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Policy {
    pub tiers: Vec<Tier>,
    pub trash: u64,
}

impl Policy {
    /// The tiers as text, without the trash: what the database keeps as `keep`.
    pub fn keep_text(&self) -> String {
        self.tiers
            .iter()
            .map(|t| {
                format!(
                    "{} {}",
                    t.every.as_str(),
                    t.span.map_or_else(|| "forever".to_string(), duration_text)
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// A policy from what the database keeps: its tiers' text and its trash in seconds.
    pub fn from_parts(keep: &str, trash: u64) -> Result<Policy, String> {
        let mut p: Policy = if keep.trim().is_empty() {
            Policy {
                tiers: Vec::new(),
                trash,
            }
        } else {
            keep.parse()?
        };
        p.trash = trash;
        Ok(p)
    }

    /// Each tier with the ages it covers, `[from, to)` in seconds back from now, `to` `None` for good.
    pub fn ages(&self) -> Vec<(Every, u64, Option<u64>)> {
        let mut out = Vec::new();
        let mut from = 0u64;
        for t in &self.tiers {
            let to = t.span.map(|s| from.saturating_add(s));
            out.push((t.every, from, to));
            match to {
                Some(to) => from = to,
                None => break,
            }
        }
        out
    }
}

impl fmt::Display for Policy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let keep = self.keep_text();
        if keep.is_empty() {
            write!(f, "trash {}", duration_text(self.trash))
        } else {
            write!(f, "{keep}; trash {}", duration_text(self.trash))
        }
    }
}

/// A duration as a policy writes it: a whole number of hours, days, weeks or years, as `36h` `30d`
/// `4w` `1y` (365 days).
pub fn parse_duration(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let bad =
        || format!("{s:?} is no duration: a whole number and h, d, w or y, as 12h, 30d, 4w, 1y");
    let (n, unit) = s.split_at(s.len().saturating_sub(1));
    let unit = match unit {
        "h" => HOUR,
        "d" => DAY,
        "w" => WEEK,
        "y" => YEAR,
        _ => return Err(bad()),
    };
    if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad());
    }
    let n: u64 = n.parse().map_err(|_| bad())?;
    n.checked_mul(unit)
        .filter(|&d| d > 0 && d <= 1000 * YEAR)
        .ok_or_else(|| format!("{s:?} is no duration: from 1h to 1000y"))
}

/// A duration in the largest unit that divides it: `parse_duration`'s inverse for what it gives.
pub fn duration_text(secs: u64) -> String {
    for (unit, name) in [(YEAR, "y"), (WEEK, "w"), (DAY, "d"), (HOUR, "h")] {
        if secs >= unit && secs % unit == 0 {
            return format!("{}{name}", secs / unit);
        }
    }
    format!("{}h", secs.div_ceil(HOUR).max(1))
}

impl std::str::FromStr for Policy {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let (keep, trash) = match text.split_once(';') {
            Some((k, t)) => (k, Some(t)),
            None => (text, None),
        };
        // `trash 7d` alone keeps no history but each file as it is now.
        let (keep, trash) = match (keep.trim().strip_prefix("trash"), trash) {
            (Some(rest), None) if rest.starts_with(char::is_whitespace) => ("", Some(keep)),
            _ => (keep, trash),
        };
        let mut tiers: Vec<Tier> = Vec::new();
        for part in keep.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            let mut words = part.split_whitespace();
            let (Some(every), Some(span), None) = (words.next(), words.next(), words.next()) else {
                return Err(format!(
                    "{part:?} is no tier: one of all, hourly, daily and weekly, and how long, as `daily 30d` or `weekly forever`"
                ));
            };
            let every = Every::ALL
                .into_iter()
                .find(|e| e.as_str() == every)
                .ok_or_else(|| {
                    format!("{every:?} is no tier: one of all, hourly, daily and weekly")
                })?;
            if let Some(last) = tiers.last() {
                if last.span.is_none() {
                    return Err(format!(
                        "{} forever is the last tier: nothing comes after it",
                        last.every.as_str()
                    ));
                }
                if last.every >= every {
                    return Err(format!(
                        "{} comes after {}: the tiers go from finer to coarser, all, hourly, daily, weekly, each once",
                        every.as_str(),
                        last.every.as_str()
                    ));
                }
            }
            let span = if span == "forever" {
                None
            } else {
                Some(parse_duration(span)?)
            };
            tiers.push(Tier { every, span });
        }
        let trash_text = trash;
        let trash = match trash {
            None => DEFAULT_TRASH,
            Some(t) => {
                let t = t.trim();
                let Some(d) = t
                    .strip_prefix("trash")
                    .filter(|d| d.starts_with(char::is_whitespace))
                else {
                    return Err(format!(
                        "{t:?} is not how long a deleted file is kept: `trash 30d`"
                    ));
                };
                parse_duration(d)?
            }
        };
        if tiers.is_empty() && trash_text.is_none() {
            return Err(
                "a policy says what it keeps, as `daily 30d, weekly forever; trash 30d`".into(),
            );
        }
        Ok(Policy { tiers, trash })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_policy_reads_as_it_is_written_and_back() {
        let p: Policy = "all 2d, hourly 1d, daily 30d, weekly forever; trash 30d"
            .parse()
            .unwrap();
        assert_eq!(p.tiers.len(), 4);
        assert_eq!(p.tiers[0].span, Some(2 * DAY));
        assert_eq!(p.tiers[3].span, None);
        assert_eq!(p.trash, 30 * DAY);
        assert_eq!(
            p.to_string(),
            "all 2d, hourly 1d, daily 30d, weekly forever; trash 30d"
        );
        assert_eq!(p.to_string().parse::<Policy>().unwrap(), p);
        // The ages each tier covers follow on from the one before.
        assert_eq!(
            p.ages(),
            [
                (Every::All, 0, Some(2 * DAY)),
                (Every::Hourly, 2 * DAY, Some(3 * DAY)),
                (Every::Daily, 3 * DAY, Some(33 * DAY)),
                (Every::Weekly, 33 * DAY, None),
            ]
        );
        // The trash when not said; a trash alone; units.
        assert_eq!("daily 4w".parse::<Policy>().unwrap().trash, DEFAULT_TRASH);
        let bare: Policy = "trash 7d".parse().unwrap();
        assert!(bare.tiers.is_empty());
        assert_eq!(bare.trash, 7 * DAY);
        assert_eq!(bare.to_string(), "trash 1w");
        assert_eq!(
            "hourly 36h; trash 1y"
                .parse::<Policy>()
                .unwrap()
                .to_string(),
            "hourly 36h; trash 1y"
        );
        assert_eq!(
            Policy::from_parts("", 3 * DAY).unwrap().to_string(),
            "trash 3d"
        );
        assert_eq!(Policy::from_parts(&p.keep_text(), p.trash).unwrap(), p);
    }

    #[test]
    fn a_policy_that_could_give_back_what_it_gave_up_is_refused() {
        for bad in [
            "",
            "daily 30d, hourly 1d",
            "all 1d, all 2d",
            "weekly forever, daily 30d",
            "daily forever, weekly forever",
            "monthly 1y",
            "daily",
            "daily 30",
            "daily 0d",
            "daily -1d",
            "daily 30m",
            "all 2d; 30d",
            "all 2d; trash",
            "all 2d; trash 0h",
            "hourly 1d daily 30d",
        ] {
            assert!(bad.parse::<Policy>().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn durations_read_and_write_back() {
        for (text, secs) in [
            ("12h", 12 * HOUR),
            ("30d", 30 * DAY),
            ("4w", 4 * WEEK),
            ("1y", YEAR),
        ] {
            assert_eq!(parse_duration(text).unwrap(), secs);
            assert_eq!(duration_text(secs), text);
        }
        assert_eq!(duration_text(14 * DAY), "2w");
        assert_eq!(duration_text(25 * HOUR), "25h");
        assert!(parse_duration("1.5d").is_err());
        assert!(parse_duration("d").is_err());
    }

    #[test]
    fn a_week_starts_on_monday_utc() {
        let (unit, offset) = Every::Weekly.grid().unwrap();
        // 2026-11-16 00:00 UTC was a Monday.
        let monday: u64 = 1_794_787_200;
        assert_eq!((monday - offset) % unit, 0);
        assert_ne!((monday + DAY - offset) % unit, 0);
    }
}
