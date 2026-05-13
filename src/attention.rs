//! Snooze / future-attention scheduling.
//!
//! Provides the fuzzy "later" presets the cell / bullet context menus
//! offer (Later today, Tomorrow, Next week, Next month, Next quarter,
//! Someday). Each preset resolves to an absolute epoch-ms timestamp
//! that the rest of the app reads as `Cell.resurface_after` /
//! `Bullet.resurface_after`.
//!
//! Time math is DST-safe: presets that mean "tomorrow morning" use
//! `chrono::NaiveDate::succ_opt()` plus a fixed local time-of-day,
//! never `+ Duration::hours(24)`, so the November fall-back and the
//! March spring-forward don't shift the wake hour by ±1.

use chrono::{
    Datelike, Duration, Local, NaiveDate, NaiveTime, TimeZone, Timelike, Weekday,
};

/// User-facing preset durations. Pure UX vocabulary — no fields here
/// pin a calendar date. Resolution happens in [`resurface_at`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnoozePreset {
    LaterToday,
    Tomorrow,
    NextWeek,
    NextMonth,
    NextQuarter,
    Someday,
}

/// Default wake hour for the "tomorrow morning" cluster of presets.
/// Picking 09:00 local feels neutral — not aggressively early, not
/// past lunch.
const WAKE_HOUR: u32 = 9;
/// Threshold for "later today" semantics: when the local clock is
/// before this hour, "Later today" maps to 18:00 of the same day;
/// otherwise it falls back to `now + 3h`. 17:00 keeps the
/// late-afternoon return from feeling like "tomorrow."
const LATER_TODAY_CUTOFF_HOUR: u32 = 17;
const LATER_TODAY_EVENING_HOUR: u32 = 18;
/// "Later today" fallback offset when the user is already past the
/// cutoff hour. Keeps the resurface within the same waking window
/// but pushes it out enough to feel "later."
const LATER_TODAY_FALLBACK_HOURS: i64 = 3;

/// Resolve a preset against a concrete local time, returning the
/// absolute epoch-ms timestamp the cell / bullet should resurface
/// at. Pure function so tests can pin a specific `now_local` across
/// DST boundaries.
pub fn resurface_at(now_local: chrono::DateTime<Local>, preset: SnoozePreset) -> i64 {
    let today = now_local.date_naive();
    let at = |date: NaiveDate, hour: u32| -> i64 {
        let t = NaiveTime::from_hms_opt(hour, 0, 0).expect("static hms");
        // Local::from_local_datetime can return Ambiguous (fall back)
        // or None (spring forward inside the gap). Both cases are
        // benign for wake-times: pick the earliest valid mapping so
        // the resurface lands no later than the requested local time.
        let dt = date.and_time(t);
        match Local.from_local_datetime(&dt) {
            chrono::LocalResult::Single(dt) => dt.timestamp_millis(),
            chrono::LocalResult::Ambiguous(earlier, _later) => earlier.timestamp_millis(),
            chrono::LocalResult::None => {
                // Spring-forward gap. Bump to the next valid hour.
                let dt = date.and_hms_opt(hour + 1, 0, 0).expect("static");
                Local
                    .from_local_datetime(&dt)
                    .single()
                    .unwrap_or_else(|| now_local + Duration::hours(1))
                    .timestamp_millis()
            }
        }
    };
    match preset {
        SnoozePreset::LaterToday => {
            if now_local.hour() < LATER_TODAY_CUTOFF_HOUR {
                at(today, LATER_TODAY_EVENING_HOUR)
            } else {
                (now_local + Duration::hours(LATER_TODAY_FALLBACK_HOURS))
                    .timestamp_millis()
            }
        }
        SnoozePreset::Tomorrow => {
            let d = today.succ_opt().unwrap_or(today);
            at(d, WAKE_HOUR)
        }
        SnoozePreset::NextWeek => {
            let d = next_monday(today);
            at(d, WAKE_HOUR)
        }
        SnoozePreset::NextMonth => {
            let d = first_of_next_month(today);
            at(d, WAKE_HOUR)
        }
        SnoozePreset::NextQuarter => {
            let d = first_of_next_quarter(today);
            at(d, WAKE_HOUR)
        }
        SnoozePreset::Someday => {
            // +6 months at 09:00 local. Far enough to drop out of
            // attention competition but still finite — every
            // someday item eventually returns. Computed by stepping
            // 6 months forward, clamping the day where calendar
            // months differ in length (e.g. snoozing on Aug 31 →
            // Feb 28/29).
            let d = add_months(today, 6);
            at(d, WAKE_HOUR)
        }
    }
}

/// The next Monday strictly after `today`. If `today` already is a
/// Monday, returns the following Monday.
fn next_monday(today: NaiveDate) -> NaiveDate {
    let days_ahead = match today.weekday() {
        Weekday::Mon => 7,
        Weekday::Tue => 6,
        Weekday::Wed => 5,
        Weekday::Thu => 4,
        Weekday::Fri => 3,
        Weekday::Sat => 2,
        Weekday::Sun => 1,
    };
    today + Duration::days(days_ahead)
}

fn first_of_next_month(today: NaiveDate) -> NaiveDate {
    let (y, m) = (today.year(), today.month());
    let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
    NaiveDate::from_ymd_opt(ny, nm, 1).expect("first-of-month always valid")
}

fn first_of_next_quarter(today: NaiveDate) -> NaiveDate {
    let m = today.month();
    let (y, qm) = match m {
        1..=3 => (today.year(), 4),
        4..=6 => (today.year(), 7),
        7..=9 => (today.year(), 10),
        _ => (today.year() + 1, 1),
    };
    NaiveDate::from_ymd_opt(y, qm, 1).expect("first-of-quarter always valid")
}

/// Add `months` calendar months to `date`, clamping the day to the
/// target month's length (so Aug 31 + 1 month = Sep 30, not Oct 1).
fn add_months(date: NaiveDate, months: u32) -> NaiveDate {
    let total = date.month() as i32 + months as i32;
    let year_shift = (total - 1) / 12;
    let new_month = ((total - 1) % 12 + 1) as u32;
    let new_year = date.year() + year_shift;
    // Try the same day; fall back through 30, 29, 28 (handles
    // Feb-end-of-month).
    for d in (1..=date.day()).rev() {
        if let Some(out) = NaiveDate::from_ymd_opt(new_year, new_month, d) {
            return out;
        }
    }
    NaiveDate::from_ymd_opt(new_year, new_month, 1).expect("first valid")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn local(y: i32, m: u32, d: u32, h: u32, mi: u32) -> chrono::DateTime<Local> {
        Local
            .with_ymd_and_hms(y, m, d, h, mi, 0)
            .single()
            .expect("unambiguous local time")
    }

    fn local_ms(y: i32, m: u32, d: u32, h: u32, mi: u32) -> i64 {
        local(y, m, d, h, mi).timestamp_millis()
    }

    #[test]
    fn later_today_before_cutoff_jumps_to_evening_of_same_day() {
        let now = local(2026, 5, 14, 9, 0);
        let when = resurface_at(now, SnoozePreset::LaterToday);
        assert_eq!(when, local_ms(2026, 5, 14, 18, 0));
    }

    #[test]
    fn later_today_after_cutoff_uses_plus_three_hours() {
        let now = local(2026, 5, 14, 20, 0);
        let when = resurface_at(now, SnoozePreset::LaterToday);
        assert_eq!(when, local_ms(2026, 5, 14, 23, 0));
    }

    #[test]
    fn tomorrow_lands_at_nine_am_next_day() {
        let now = local(2026, 5, 14, 14, 30);
        let when = resurface_at(now, SnoozePreset::Tomorrow);
        assert_eq!(when, local_ms(2026, 5, 15, 9, 0));
    }

    #[test]
    fn next_week_is_following_monday_nine_am() {
        // Thursday 2026-05-14 → next Monday is 2026-05-18.
        let now = local(2026, 5, 14, 10, 0);
        let when = resurface_at(now, SnoozePreset::NextWeek);
        assert_eq!(when, local_ms(2026, 5, 18, 9, 0));
    }

    #[test]
    fn next_week_from_a_monday_lands_a_week_later() {
        // 2026-05-04 is a Monday.
        let now = local(2026, 5, 4, 10, 0);
        let when = resurface_at(now, SnoozePreset::NextWeek);
        assert_eq!(when, local_ms(2026, 5, 11, 9, 0));
    }

    #[test]
    fn next_month_is_first_of_following_month_nine_am() {
        let now = local(2026, 5, 14, 10, 0);
        let when = resurface_at(now, SnoozePreset::NextMonth);
        assert_eq!(when, local_ms(2026, 6, 1, 9, 0));
    }

    #[test]
    fn next_month_from_december_rolls_year() {
        let now = local(2026, 12, 14, 10, 0);
        let when = resurface_at(now, SnoozePreset::NextMonth);
        assert_eq!(when, local_ms(2027, 1, 1, 9, 0));
    }

    #[test]
    fn next_quarter_returns_first_of_quarter_start_month() {
        // May (Q2) → next quarter starts July 1.
        let now = local(2026, 5, 14, 10, 0);
        assert_eq!(
            resurface_at(now, SnoozePreset::NextQuarter),
            local_ms(2026, 7, 1, 9, 0)
        );
        // Q4 → next quarter is Q1 of following year.
        let now = local(2026, 11, 30, 10, 0);
        assert_eq!(
            resurface_at(now, SnoozePreset::NextQuarter),
            local_ms(2027, 1, 1, 9, 0)
        );
    }

    #[test]
    fn someday_lands_six_months_out_at_nine_am() {
        let now = local(2026, 5, 14, 10, 0);
        let when = resurface_at(now, SnoozePreset::Someday);
        assert_eq!(when, local_ms(2026, 11, 14, 9, 0));
    }

    #[test]
    fn someday_clamps_day_when_target_month_is_shorter() {
        // August has 31 days; +6 months lands in February. Day 31
        // clamps to Feb 28/29.
        let now = local(2026, 8, 31, 10, 0);
        let when = resurface_at(now, SnoozePreset::Someday);
        // 2027 is not a leap year — clamp to Feb 28.
        assert_eq!(when, local_ms(2027, 2, 28, 9, 0));
    }

    #[test]
    fn tomorrow_across_spring_forward_dst_does_not_skew_wake_hour() {
        // US DST 2026 starts on Sun March 8 at 02:00 local. The
        // wake hour is 09:00 — well past the gap — but the date
        // arithmetic must use date arithmetic, not naive +24h, or
        // we'd land at 08:00 or 10:00. Anchor on the Saturday before
        // and confirm tomorrow is 09:00 local on Sunday, which is
        // post-spring-forward.
        let now = local(2026, 3, 7, 14, 0);
        let when = resurface_at(now, SnoozePreset::Tomorrow);
        assert_eq!(when, local_ms(2026, 3, 8, 9, 0));
    }

    #[test]
    fn tomorrow_across_fall_back_dst_does_not_skew_wake_hour() {
        // US DST 2026 ends on Sun Nov 1 at 02:00 local. The wake
        // hour is 09:00 — past the rollback — but date arithmetic
        // must hold. Saturday Oct 31 → Sunday Nov 1 at 09:00 local.
        let now = local(2026, 10, 31, 14, 0);
        let when = resurface_at(now, SnoozePreset::Tomorrow);
        assert_eq!(when, local_ms(2026, 11, 1, 9, 0));
    }
}
