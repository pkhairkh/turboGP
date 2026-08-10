//! Date / Time / Timestamp / Interval types — instruction-shaped.
//!
//! All types fit in u64 cells (or pairs of u64) so existing kernels
//! work unchanged. Date = i32 days since epoch in low 32 bits of u64.
//!
//! Research: Zeller's congruence for branchless day-of-week computation.

use crate::Error;
use time::format_description::well_known::Iso8601;
use time::{Date as TimeDate, Month, PrimitiveDateTime, Time as TimeTime};

/// Convert days-since-epoch (1970-01-01) to Gregorian year using
/// Howard Hinnant's `civil_from_days` algorithm (O(1) integer math).
/// <https://howardhinnant.github.io/date_algorithms.html#civil_from_days>
///
/// For TPC-H date ranges (1992-2003, plus the 5-year forward window to 2069)
/// this takes ~8 integer ops vs `time::Date::from_julian_day`'s ~30 ops +
/// branches. Q7/Q8/Q9 extract year from ~6M lineitem rows.
///
/// Hinnant's algorithm models the year as starting on March 1 (so that leap
/// days fall at year-end, simplifying the math). For January and February
/// dates (doy >= 306 in Hinnant's calendar), the actual Gregorian year is
/// `y + 1`.
///
/// Correct for d ∈ [-2557, 36525] (1963-01-01 through 2069-12-31) and well
/// beyond — verified bit-exact with `time::Date::from_julian_day(d + 2440588).year()`
/// across 1900-2100 in unit tests.
#[inline(always)]
pub fn days_since_epoch_to_year(d: i64) -> i32 {
    let z = d + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
                                                       // January or February (doy >= 306) → Gregorian year is y + 1
    if doy >= 306 {
        (y + 1) as i32
    } else {
        y as i32
    }
}

/// A calendar date stored as days-since-epoch (i32 in low 32 bits of u64).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Date(pub i32);

impl Date {
    pub const EPOCH: Date = Date(0);

    pub fn from_ymd(year: i32, month: u32, day: u32) -> Result<Self, Error> {
        let m = Month::try_from(month as u8)
            .map_err(|e| Error::InvalidArg(format!("invalid month {month}: {e}")))?;
        let d = TimeDate::from_calendar_date(year, m, day as u8)
            .map_err(|e| Error::InvalidArg(format!("invalid YMD {year}-{month}-{day}: {e}")))?;
        Ok(Self(d.to_julian_day() - 2_440_588))
    }

    pub fn to_ymd(&self) -> (i32, u32, u32) {
        let d = TimeDate::from_julian_day(self.0 + 2_440_588).expect("julian day in range");
        (d.year(), d.month() as u32, d.day() as u32)
    }

    /// Fast path for `extract(year FROM ...)` — returns the Gregorian year
    /// without going through `to_ymd()` (which calls
    /// `time::Date::from_julian_day`, ~30 ops + branches per row).
    ///
    /// Delegates to [`days_since_epoch_to_year`] (Howard Hinnant's
    /// `civil_from_days` algorithm — 6-8 integer ops).
    #[inline(always)]
    pub fn year(&self) -> i32 {
        days_since_epoch_to_year(self.0 as i64)
    }

    pub fn to_u64(&self) -> u64 {
        self.0 as u32 as u64
    }
    pub fn from_u64(v: u64) -> Self {
        Date(v as u32 as i32)
    }

    pub fn from_str(s: &str) -> Result<Self, Error> {
        let fmt = time::format_description::parse_borrowed::<2>("[year]-[month]-[day]")
            .map_err(|e| Error::Parse(format!("date fmt: {e}")))?;
        let d = TimeDate::parse(s, &fmt).map_err(|e| Error::Parse(format!("date '{s}': {e}")))?;
        Ok(Self(d.to_julian_day() - 2_440_588))
    }

    pub fn to_iso(&self) -> String {
        let (y, m, d) = self.to_ymd();
        format!("{y:04}-{m:02}-{d:02}")
    }

    pub fn add_days(&self, days: i32) -> Date {
        Date(self.0.saturating_add(days))
    }

    pub fn add_interval(&self, iv: &Interval) -> Timestamp {
        let (y, m, d) = self.to_ymd();
        let total_months = y * 12 + (m as i32 - 1) + iv.months;
        let ny = total_months.div_euclid(12);
        let nm = total_months.rem_euclid(12) as u32 + 1;
        let nd = (d as u8).min(days_in_month(ny, nm));
        let base = Date::from_ymd(ny, nm, nd as u32).unwrap_or(*self);
        Timestamp(base.0 as i64 * 86_400_000_000 + iv.micros)
    }

    pub fn days_since(&self, other: &Date) -> i32 {
        self.0 - other.0
    }

    /// Day of week using Zeller's congruence (branchless, SIMD-friendly).
    /// Returns 0=Sunday, 1=Monday, ..., 6=Saturday (PostgreSQL convention).
    pub fn dow(&self) -> i32 {
        ((self.0 + 4).rem_euclid(7))
    }

    /// ISO day of week: 1=Monday, 7=Sunday.
    pub fn isodow(&self) -> i32 {
        ((self.0 + 3).rem_euclid(7) + 1)
    }

    pub fn doy(&self) -> u32 {
        let (y, m, d) = self.to_ymd();
        day_of_year(y, m, d)
    }

    pub fn quarter(&self) -> u32 {
        let (_, m, _) = self.to_ymd();
        (m - 1) / 3 + 1
    }

    pub fn epoch_seconds(&self) -> i64 {
        self.0 as i64 * 86_400
    }
}

/// Time of day stored as microseconds since midnight (u64).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Time(pub u64);

impl Time {
    pub const MIDNIGHT: Time = Time(0);

    pub fn from_hms_nano(hour: u32, minute: u32, second: u32, nano: u32) -> Result<Self, Error> {
        let t = TimeTime::from_hms_nano(hour as u8, minute as u8, second as u8, nano)
            .map_err(|e| Error::InvalidArg(format!("invalid HMSns: {e}")))?;
        let total_us = (t.hour() as u64) * 3_600_000_000
            + (t.minute() as u64) * 60_000_000
            + (t.second() as u64) * 1_000_000
            + (t.nanosecond() as u64) / 1_000;
        Ok(Time(total_us))
    }

    pub fn to_hms_micro(&self) -> (u32, u32, u32, u32) {
        let total = self.0;
        let h = (total / 3_600_000_000) as u32;
        let m = ((total / 60_000_000) % 60) as u32;
        let s = ((total / 1_000_000) % 60) as u32;
        let us = (total % 1_000_000) as u32;
        (h, m, s, us)
    }

    pub fn to_u64(&self) -> u64 {
        self.0
    }
    pub fn from_u64(v: u64) -> Self {
        Time(v)
    }

    pub fn from_str(s: &str) -> Result<Self, Error> {
        let fmt = time::format_description::parse_borrowed::<2>("[hour]:[minute]:[second]")
            .map_err(|e| Error::Parse(format!("time fmt: {e}")))?;
        let t = TimeTime::parse(s, &fmt).map_err(|e| Error::Parse(format!("time '{s}': {e}")))?;
        Self::from_hms_nano(
            t.hour() as u32,
            t.minute() as u32,
            t.second() as u32,
            t.nanosecond() as u32,
        )
    }

    pub fn to_iso(&self) -> String {
        let (h, m, s, _) = self.to_hms_micro();
        format!("{h:02}:{m:02}:{s:02}")
    }
}

/// Timestamp stored as microseconds since epoch (i64 in u64).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(pub i64);

impl Timestamp {
    pub const EPOCH: Timestamp = Timestamp(0);

    pub fn to_u64(&self) -> u64 {
        self.0 as u64
    }
    pub fn from_u64(v: u64) -> Self {
        Timestamp(v as i64)
    }

    pub fn from_date(d: Date) -> Self {
        Timestamp(d.0 as i64 * 86_400_000_000)
    }

    pub fn from_date_time(d: Date, t: Time) -> Self {
        Timestamp(d.0 as i64 * 86_400_000_000 + t.0 as i64)
    }

    pub fn to_date_time(&self) -> (Date, Time) {
        let days = self.0.div_euclid(86_400_000_000);
        let micros_in_day = self.0.rem_euclid(86_400_000_000);
        (Date(days as i32), Time(micros_in_day as u64))
    }

    pub fn from_str(s: &str) -> Result<Self, Error> {
        let s_norm = s.replacen(' ', "T", 1);
        let pdt = PrimitiveDateTime::parse(&s_norm, &Iso8601::DEFAULT)
            .map_err(|e| Error::Parse(format!("timestamp '{s}': {e}")))?;
        let dt = pdt.assume_utc();
        let micros = dt.unix_timestamp_nanos() / 1_000;
        Ok(Timestamp(micros as i64))
    }

    pub fn to_iso(&self) -> String {
        let (d, t) = self.to_date_time();
        let (h, m, s, _) = t.to_hms_micro();
        let (y, mo, da) = d.to_ymd();
        format!("{y:04}-{mo:02}-{da:02}T{h:02}:{m:02}:{s:02}")
    }

    pub fn add_interval(&self, iv: &Interval) -> Timestamp {
        let (d, t) = self.to_date_time();
        let new_ts = d.add_interval(iv);
        Timestamp(new_ts.0 + t.0 as i64)
    }
}

/// Interval: months + microseconds dual representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Interval {
    pub months: i32,
    pub micros: i64,
}

impl Interval {
    pub const ZERO: Interval = Interval { months: 0, micros: 0 };

    pub fn from_months(m: i32) -> Self {
        Interval { months: m, micros: 0 }
    }
    pub fn from_days(d: i32) -> Self {
        Interval { months: 0, micros: (d as i64) * 86_400_000_000 }
    }
    pub fn from_micros(m: i64) -> Self {
        Interval { months: 0, micros: m }
    }

    pub fn to_u64_pair(&self) -> [u64; 2] {
        [(self.months as u32) as u64, self.micros as u64]
    }

    pub fn from_u64_pair(v: [u64; 2]) -> Self {
        Interval { months: v[0] as u32 as i32, micros: v[1] as i64 }
    }

    pub fn from_sql_str(s: &str) -> Result<Self, Error> {
        let s = s.trim().trim_matches('\'');
        let lower = s.to_lowercase();
        let (n_str, unit) = lower
            .split_once(char::is_whitespace)
            .ok_or_else(|| Error::Parse(format!("interval '{s}': missing unit")))?;
        let n: i64 =
            n_str.parse().map_err(|e| Error::Parse(format!("interval number '{n_str}': {e}")))?;
        let iv = match unit {
            "year" | "years" => Interval::from_months((n * 12) as i32),
            "month" | "months" => Interval::from_months(n as i32),
            "day" | "days" => Interval::from_days(n as i32),
            "hour" | "hours" => Interval::from_micros(n * 3_600_000_000),
            "minute" | "minutes" => Interval::from_micros(n * 60_000_000),
            "second" | "seconds" => Interval::from_micros(n * 1_000_000),
            "millisecond" | "milliseconds" => Interval::from_micros(n * 1_000),
            "microsecond" | "microseconds" => Interval::from_micros(n),
            _ => return Err(Error::Parse(format!("interval unit '{unit}' not supported"))),
        };
        Ok(iv)
    }

    pub fn add(&self, other: &Interval) -> Interval {
        Interval {
            months: self.months.saturating_add(other.months),
            micros: self.micros.saturating_add(other.micros),
        }
    }
}

fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0)
}

fn days_in_month(year: i32, month: u32) -> u8 {
    if let Ok(m) = Month::try_from(month as u8) {
        if let Ok(first) = TimeDate::from_calendar_date(year, m, 1) {
            let next_first = if month == 12 {
                TimeDate::from_calendar_date(year + 1, Month::January, 1).unwrap_or(first)
            } else {
                let next_m = Month::try_from((month + 1) as u8).unwrap_or(Month::December);
                TimeDate::from_calendar_date(year, next_m, 1).unwrap_or(first)
            };
            return next_first.previous_day().unwrap_or(first).day();
        }
    }
    28
}

fn day_of_year(year: i32, month: u32, day: u32) -> u32 {
    let leap = is_leap_year(year);
    let days_in_months: [u32; 12] =
        [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut doy = day;
    for m in 1..month {
        doy += days_in_months[(m - 1) as usize];
    }
    doy
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------- W1-C: days_since_epoch_to_year tests ----------

    /// Cross-check days_since_epoch_to_year against the slow path
    /// (time::Date::from_julian_day + .year()) for a single Y-M-D.
    fn check_year_against_time(year: i32, month: u32, day: u32) {
        let date = Date::from_ymd(year, month, day).unwrap();
        let slow = date.to_ymd().0;
        let fast = days_since_epoch_to_year(date.0 as i64);
        assert_eq!(
            fast, slow,
            "mismatch for {year:04}-{month:02}-{day:02}: fast={fast}, slow={slow}, d={}",
            date.0
        );
    }

    #[test]
    fn w1c_year_epoch_1970_01_01() {
        // d = 0 → year 1970
        assert_eq!(days_since_epoch_to_year(0), 1970);
        check_year_against_time(1970, 1, 1);
    }

    #[test]
    fn w1c_year_2000_02_29_leap() {
        // Leap day — January-February branch in Hinnant's calendar.
        check_year_against_time(2000, 2, 29);
    }

    #[test]
    fn w1c_year_2000_03_01_leap_boundary() {
        // Day after leap day — March 1 starts Hinnant's civil year.
        check_year_against_time(2000, 3, 1);
    }

    #[test]
    fn w1c_year_2024_12_31() {
        check_year_against_time(2024, 12, 31);
    }

    #[test]
    fn w1c_year_1900_03_01_negative_d() {
        // Pre-epoch date with negative d (1900-03-01).
        check_year_against_time(1900, 3, 1);
    }

    #[test]
    fn w1c_year_1900_01_01_negative_d() {
        // Pre-epoch January (negative d, Jan/Feb branch).
        check_year_against_time(1900, 1, 1);
    }

    #[test]
    fn w1c_year_2099_12_31() {
        check_year_against_time(2099, 12, 31);
    }

    #[test]
    fn w1c_year_tpc_h_range_1992_1998() {
        // Sweep every year boundary in the TPC-H date range plus a few
        // leap days — bit-exact check against time::Date.
        for year in 1992..=1998 {
            for (m, d) in [(1, 1), (2, 28), (3, 1), (12, 31)] {
                check_year_against_time(year, m, d);
            }
        }
    }

    #[test]
    fn w1c_year_tpc_h_forward_window_1998_2003() {
        // 5-year forward window per TPC-H spec.
        for year in 1998..=2003 {
            for (m, d) in [(1, 1), (2, 28), (3, 1), (12, 31)] {
                check_year_against_time(year, m, d);
            }
        }
    }

    #[test]
    fn w1c_year_constraint_bounds_1963_2069() {
        // W1-C task constraint: d ∈ [-2557, 36525] (1963-01-01 through 2069-12-31).
        // Sweep every year boundary + Jan 1 + Feb 28/29 + Mar 1 + Dec 31.
        for year in 1963..=2069 {
            check_year_against_time(year, 1, 1);
            check_year_against_time(year, 2, 28);
            // Feb 29 only on leap years
            let leap = (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0);
            if leap {
                check_year_against_time(year, 2, 29);
            }
            check_year_against_time(year, 3, 1);
            check_year_against_time(year, 12, 31);
        }
    }

    #[test]
    fn w1c_year_random_100_days_1970_2030() {
        // Pseudo-random sample of 100 days in 1970-2030.
        // Deterministic LCG (no rand dep).
        let mut state: u64 = 0x1234_5678_9abc_def0;
        for _ in 0..100 {
            // LCG step (Numerical Recipes constants).
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let year = 1970 + (state % 61) as i32; // 1970..=2030
            let month = 1 + ((state >> 32) % 12) as u32;
            let day = 1 + ((state >> 48) % 28) as u32; // safe for any month
            check_year_against_time(year, month, day);
        }
    }

    // ---------- end W1-C tests ----------

    #[test]
    fn date_from_ymd_round_trip() {
        let d = Date::from_ymd(2024, 7, 30).unwrap();
        assert_eq!(d.to_ymd(), (2024, 7, 30));
    }

    #[test]
    fn date_epoch() {
        assert_eq!(Date::from_ymd(1970, 1, 1).unwrap(), Date::EPOCH);
    }

    #[test]
    fn date_leap_year_2024() {
        let d = Date::from_ymd(2024, 2, 28).unwrap();
        assert_eq!(d.add_days(1).to_ymd(), (2024, 2, 29));
    }

    #[test]
    fn date_non_leap_year_2023() {
        let d = Date::from_ymd(2023, 2, 28).unwrap();
        assert_eq!(d.add_days(1).to_ymd(), (2023, 3, 1));
    }

    #[test]
    fn date_year_rollover() {
        let d = Date::from_ymd(2023, 12, 31).unwrap();
        assert_eq!(d.add_days(1).to_ymd(), (2024, 1, 1));
    }

    #[test]
    fn date_str_round_trip() {
        let d = Date::from_str("2024-07-30").unwrap();
        assert_eq!(d.to_ymd(), (2024, 7, 30));
        assert_eq!(d.to_iso(), "2024-07-30");
    }

    #[test]
    fn date_u64_round_trip() {
        let d = Date::from_ymd(2024, 7, 30).unwrap();
        assert_eq!(Date::from_u64(d.to_u64()), d);
    }

    #[test]
    fn date_days_since() {
        let a = Date::from_ymd(2024, 1, 1).unwrap();
        let b = Date::from_ymd(2024, 12, 31).unwrap();
        assert_eq!(b.days_since(&a), 365);
    }

    #[test]
    fn date_dow_thursday() {
        let d = Date::from_ymd(1970, 1, 1).unwrap();
        assert_eq!(d.dow(), 4);
    }

    #[test]
    fn date_dow_sunday() {
        let d = Date::from_ymd(2024, 7, 28).unwrap();
        assert_eq!(d.dow(), 0);
    }

    #[test]
    fn date_isodow_monday() {
        let d = Date::from_ymd(2024, 7, 29).unwrap();
        assert_eq!(d.isodow(), 1);
    }

    #[test]
    fn date_doy_jan1() {
        let d = Date::from_ymd(2024, 1, 1).unwrap();
        assert_eq!(d.doy(), 1);
    }

    #[test]
    fn date_doy_dec31_leap() {
        let d = Date::from_ymd(2024, 12, 31).unwrap();
        assert_eq!(d.doy(), 366);
    }

    #[test]
    fn date_quarter() {
        assert_eq!(Date::from_ymd(2024, 1, 15).unwrap().quarter(), 1);
        assert_eq!(Date::from_ymd(2024, 7, 15).unwrap().quarter(), 3);
    }

    #[test]
    fn date_epoch_seconds() {
        assert_eq!(Date::from_ymd(1970, 1, 1).unwrap().epoch_seconds(), 0);
        assert_eq!(Date::from_ymd(1970, 1, 2).unwrap().epoch_seconds(), 86_400);
    }

    #[test]
    fn time_from_hms() {
        let t = Time::from_hms_nano(15, 45, 30, 0).unwrap();
        let (h, m, s, _) = t.to_hms_micro();
        assert_eq!((h, m, s), (15, 45, 30));
    }

    #[test]
    fn time_u64_round_trip() {
        let t = Time::from_hms_nano(23, 59, 59, 999_999_999).unwrap();
        assert_eq!(Time::from_u64(t.to_u64()), t);
    }

    #[test]
    fn timestamp_from_date() {
        let d = Date::from_ymd(2024, 1, 1).unwrap();
        let ts = Timestamp::from_date(d);
        let (d2, t) = ts.to_date_time();
        assert_eq!(d2, d);
        assert_eq!(t, Time::MIDNIGHT);
    }

    #[test]
    fn timestamp_from_str() {
        let ts = Timestamp::from_str("2024-07-30T15:45:00").unwrap();
        let (d, t) = ts.to_date_time();
        assert_eq!(d.to_ymd(), (2024, 7, 30));
        let (h, m, s, _) = t.to_hms_micro();
        assert_eq!((h, m, s), (15, 45, 0));
    }

    #[test]
    fn timestamp_from_str_space() {
        let ts = Timestamp::from_str("2024-07-30 15:45:00").unwrap();
        let (d, _) = ts.to_date_time();
        assert_eq!(d.to_ymd(), (2024, 7, 30));
    }

    #[test]
    fn interval_months() {
        let iv = Interval::from_months(3);
        assert_eq!(iv.months, 3);
        assert_eq!(iv.micros, 0);
    }

    #[test]
    fn interval_pack_unpack() {
        let iv = Interval { months: 5, micros: -123_456 };
        assert_eq!(Interval::from_u64_pair(iv.to_u64_pair()), iv);
    }

    #[test]
    fn interval_sql_str_months() {
        assert_eq!(Interval::from_sql_str("'3 months'").unwrap().months, 3);
    }

    #[test]
    fn interval_sql_str_days() {
        let iv = Interval::from_sql_str("'7 days'").unwrap();
        assert_eq!(iv.micros, 7 * 86_400_000_000);
    }

    #[test]
    fn interval_sql_str_years() {
        assert_eq!(Interval::from_sql_str("'2 years'").unwrap().months, 24);
    }

    #[test]
    fn date_add_interval_month_rollover() {
        let d = Date::from_ymd(2024, 1, 31).unwrap();
        let result = d.add_interval(&Interval::from_months(1));
        let (y, m, dd) = result.to_date_time().0.to_ymd();
        assert_eq!((y, m, dd), (2024, 2, 29));
    }

    #[test]
    fn timestamp_add_interval() {
        let ts = Timestamp::from_str("2024-07-30T15:45:00").unwrap();
        let ts2 = ts.add_interval(&Interval::from_micros(60_000_000));
        let (_, t) = ts2.to_date_time();
        let (h, m, s, _) = t.to_hms_micro();
        assert_eq!((h, m, s), (15, 46, 0));
    }

    #[test]
    fn date_invalid_month() {
        assert!(Date::from_ymd(2024, 13, 1).is_err());
    }

    #[test]
    fn date_invalid_day() {
        assert!(Date::from_ymd(2024, 2, 30).is_err());
    }

    #[test]
    fn date_invalid_str() {
        assert!(Date::from_str("not a date").is_err());
    }
}
