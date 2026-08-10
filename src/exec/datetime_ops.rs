//! Date/Time operations — EXTRACT, date_part, date_trunc.
//!
//! Research: Zeller's congruence for branchless day-of-week.
//! Lookup table for year extraction (4KB for ±1000 years).

use crate::types::{Date, Interval, Time, Timestamp};
use crate::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DateTimeField {
    Year,
    Month,
    Day,
    Hour,
    Minute,
    Second,
    Millisecond,
    Microsecond,
    Dow,
    Isodow,
    Doy,
    Week,
    Quarter,
    Epoch,
    Decade,
    Century,
    Millennium,
}

pub fn extract_from_date(d: Date, field: DateTimeField) -> i64 {
    let (y, m, dd) = d.to_ymd();
    match field {
        DateTimeField::Year => y as i64,
        DateTimeField::Month => m as i64,
        DateTimeField::Day => dd as i64,
        DateTimeField::Dow => d.dow() as i64,
        DateTimeField::Isodow => d.isodow() as i64,
        DateTimeField::Doy => d.doy() as i64,
        DateTimeField::Quarter => d.quarter() as i64,
        DateTimeField::Epoch => d.epoch_seconds(),
        DateTimeField::Decade => (y / 10) as i64,
        DateTimeField::Century => {
            if y > 0 {
                ((y - 1) / 100 + 1) as i64
            } else {
                (y / 100) as i64
            }
        }
        DateTimeField::Millennium => {
            if y > 0 {
                ((y - 1) / 1000 + 1) as i64
            } else {
                (y / 1000) as i64
            }
        }
        _ => 0,
    }
}

pub fn extract_from_timestamp(ts: Timestamp, field: DateTimeField) -> i64 {
    let (d, t) = ts.to_date_time();
    match field {
        DateTimeField::Year
        | DateTimeField::Month
        | DateTimeField::Day
        | DateTimeField::Dow
        | DateTimeField::Isodow
        | DateTimeField::Doy
        | DateTimeField::Week
        | DateTimeField::Quarter
        | DateTimeField::Decade
        | DateTimeField::Century
        | DateTimeField::Millennium => extract_from_date(d, field),
        DateTimeField::Hour => (t.0 / 3_600_000_000) as i64,
        DateTimeField::Minute => ((t.0 / 60_000_000) % 60) as i64,
        DateTimeField::Second => ((t.0 / 1_000_000) % 60) as i64,
        DateTimeField::Millisecond => ((t.0 / 1_000) % 1000) as i64,
        DateTimeField::Microsecond => (t.0 % 1_000_000) as i64,
        DateTimeField::Epoch => ts.0 / 1_000_000,
    }
}

pub fn date_part(d: Date, field: &str) -> Result<i64, Error> {
    extract_from_date(d, parse_field(field)?);
    Ok(extract_from_date(d, parse_field(field)?))
}

pub fn date_trunc(ts: Timestamp, field: &str) -> Result<Timestamp, Error> {
    let f = parse_field(field)?;
    let (d, t) = ts.to_date_time();
    let (y, m, dd) = d.to_ymd();
    let (h, mi, s, us) = t.to_hms_micro();
    let (ny, nm, nd, nh, nmi, ns, nus) = match f {
        DateTimeField::Year => (y, 1, 1, 0, 0, 0, 0),
        DateTimeField::Quarter => (y, ((m - 1) / 3) * 3 + 1, 1, 0, 0, 0, 0),
        DateTimeField::Month => (y, m, 1, 0, 0, 0, 0),
        DateTimeField::Day => (y, m, dd, 0, 0, 0, 0),
        DateTimeField::Hour => (y, m, dd, h, 0, 0, 0),
        DateTimeField::Minute => (y, m, dd, h, mi, 0, 0),
        DateTimeField::Second => (y, m, dd, h, mi, s, 0),
        _ => return Err(Error::InvalidArg(format!("date_trunc: unsupported field '{field}'"))),
    };
    let new_d = Date::from_ymd(ny, nm, nd)?;
    let new_t = Time::from_hms_nano(nh, nmi, ns, nus)?;
    Ok(Timestamp::from_date_time(new_d, new_t))
}

fn parse_field(s: &str) -> Result<DateTimeField, Error> {
    match s.to_lowercase().as_str() {
        "year" | "y" => Ok(DateTimeField::Year),
        "month" | "mon" => Ok(DateTimeField::Month),
        "day" | "d" => Ok(DateTimeField::Day),
        "hour" | "h" => Ok(DateTimeField::Hour),
        "minute" | "min" => Ok(DateTimeField::Minute),
        "second" | "sec" => Ok(DateTimeField::Second),
        "dow" => Ok(DateTimeField::Dow),
        "isodow" => Ok(DateTimeField::Isodow),
        "doy" => Ok(DateTimeField::Doy),
        "quarter" => Ok(DateTimeField::Quarter),
        "epoch" => Ok(DateTimeField::Epoch),
        _ => Err(Error::InvalidArg(format!("unknown datetime field: '{s}'"))),
    }
}

pub fn now() -> Timestamp {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0);
    Timestamp(secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_year() {
        let d = Date::from_ymd(2024, 7, 30).unwrap();
        assert_eq!(extract_from_date(d, DateTimeField::Year), 2024);
    }

    #[test]
    fn extract_month() {
        let d = Date::from_ymd(2024, 7, 30).unwrap();
        assert_eq!(extract_from_date(d, DateTimeField::Month), 7);
    }

    #[test]
    fn extract_dow() {
        let d = Date::from_ymd(1970, 1, 1).unwrap();
        assert_eq!(extract_from_date(d, DateTimeField::Dow), 4);
    }

    #[test]
    fn extract_quarter() {
        assert_eq!(
            extract_from_date(Date::from_ymd(2024, 7, 15).unwrap(), DateTimeField::Quarter),
            3
        );
    }

    #[test]
    fn extract_epoch() {
        assert_eq!(extract_from_date(Date::from_ymd(1970, 1, 1).unwrap(), DateTimeField::Epoch), 0);
    }

    #[test]
    fn date_trunc_year() {
        let ts = Timestamp::from_str("2024-07-30T15:45:30").unwrap();
        let t = date_trunc(ts, "year").unwrap();
        let (d, time) = t.to_date_time();
        assert_eq!(d.to_ymd(), (2024, 1, 1));
    }

    #[test]
    fn date_trunc_month() {
        let ts = Timestamp::from_str("2024-07-30T15:45:30").unwrap();
        let t = date_trunc(ts, "month").unwrap();
        let (d, _) = t.to_date_time();
        assert_eq!(d.to_ymd(), (2024, 7, 1));
    }

    #[test]
    fn date_trunc_hour() {
        let ts = Timestamp::from_str("2024-07-30T15:45:30").unwrap();
        let t = date_trunc(ts, "hour").unwrap();
        let (_, time) = t.to_date_time();
        let (h, m, s, _) = time.to_hms_micro();
        assert_eq!((h, m, s), (15, 0, 0));
    }

    #[test]
    fn now_returns_recent() {
        let n = now();
        assert!(n.0 > 19723 * 86_400_000_000);
    }
}
