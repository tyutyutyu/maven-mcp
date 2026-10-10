//! Small UTC timestamp codec: RFC3339, IDE wall-clock timestamps, and epoch ms.
//! No locale or machine timezone is consulted. Offset-free logs are treated as UTC.
use regex::Regex;
use std::sync::LazyLock;

static DATE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
    r"^([0-9]{4})-([0-9]{2})-([0-9]{2})(?:[T ]([0-9]{2}):([0-9]{2}):([0-9]{2})(?:[.,]([0-9]{1,9}))?(Z|[+-][0-9]{2}:[0-9]{2})?)?$"
).expect("constant timestamp expression")
});

pub fn normalize_timestamp(value: &str) -> Option<String> {
    if value.bytes().all(|c| c.is_ascii_digit()) && matches!(value.len(), 10 | 13) {
        let epoch = value.parse::<i64>().ok()?;
        return format_epoch(if value.len() == 10 {
            epoch.checked_mul(1000)?
        } else {
            epoch
        });
    }
    let c = DATE.captures(value)?;
    let number = |i| c.get(i).map_or(Some(0), |m| m.as_str().parse::<i64>().ok());
    let (year, month, day) = (number(1)?, number(2)?, number(3)?);
    let (hour, minute, second) = (number(4)?, number(5)?, number(6)?);
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days_in_month = match month {
        2 => {
            if leap {
                29
            } else {
                28
            }
        }
        4 | 6 | 9 | 11 => 30,
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        _ => return None,
    };
    if !(1..=days_in_month).contains(&day) || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let adjusted_year = year - i64::from(month <= 2);
    let era = adjusted_year.div_euclid(400);
    let yoe = adjusted_year - era * 400;
    let shifted_month = month + if month > 2 { -3 } else { 9 };
    let days = era * 146097 + yoe * 365 + yoe / 4 - yoe / 100 + (153 * shifted_month + 2) / 5 + day
        - 1
        - 719468;
    let offset = match c.get(8).map(|m| m.as_str()) {
        Some("Z") | None => 0,
        Some(zone) => {
            let h = zone.get(1..3)?.parse::<i64>().ok()?;
            let m = zone.get(4..6)?.parse::<i64>().ok()?;
            if h > 23 || m > 59 {
                return None;
            }
            (h * 60 + m) * 60 * if zone.starts_with('-') { -1 } else { 1 }
        }
    };
    let fraction = c.get(7).map_or("", |m| m.as_str());
    let millis = format!("{fraction:0<3}").get(..3)?.parse::<i64>().ok()?;
    format_epoch((days * 86400 + hour * 3600 + minute * 60 + second - offset) * 1000 + millis)
}

fn format_epoch(millis: i64) -> Option<String> {
    let seconds = millis.div_euclid(1000);
    let days = seconds.div_euclid(86400) + 719468;
    let era = days.div_euclid(146097);
    let doe = days - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    let year = year + i64::from(month <= 2);
    if !(0..=9999).contains(&year) {
        return None;
    }
    let within_day = seconds.rem_euclid(86400);
    Some(format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        within_day / 3600,
        within_day / 60 % 60,
        within_day % 60,
        millis.rem_euclid(1000)
    ))
}

#[cfg(test)]
#[path = "../../tests/unit/log_analysis_time.rs"]
mod tests;
