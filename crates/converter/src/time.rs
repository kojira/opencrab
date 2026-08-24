pub(crate) fn parse_utc_nanos(input: &str) -> Option<i64> {
    if input.len() < 19 {
        return None;
    }
    let bytes = input.as_bytes();
    if bytes.get(4) != Some(&b'-')
        || bytes.get(7) != Some(&b'-')
        || !matches!(bytes.get(10), Some(b'T' | b' '))
        || bytes.get(13) != Some(&b':')
        || bytes.get(16) != Some(&b':')
    {
        return None;
    }
    let year = digits(bytes, 0, 4)? as i64;
    let month = digits(bytes, 5, 2)?;
    let day = digits(bytes, 8, 2)?;
    let hour = digits(bytes, 11, 2)?;
    let minute = digits(bytes, 14, 2)?;
    let second = digits(bytes, 17, 2)?;
    if year == 0
        || !(1..=12).contains(&month)
        || day == 0
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }

    let mut index = 19;
    let mut nanos = 0_i64;
    if bytes.get(index) == Some(&b'.') {
        index += 1;
        let start = index;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) && index - start < 9 {
            nanos = nanos
                .checked_mul(10)?
                .checked_add((bytes[index] - b'0') as i64)?;
            index += 1;
        }
        let digits = index - start;
        if digits == 0 || bytes.get(index).is_some_and(u8::is_ascii_digit) {
            return None;
        }
        for _ in digits..9 {
            nanos = nanos.checked_mul(10)?;
        }
    }

    let offset_seconds = match bytes.get(index) {
        None if bytes[10] == b' ' => 0_i64,
        Some(b'Z') if index + 1 == bytes.len() && bytes[10] == b'T' => 0,
        Some(b'+' | b'-') if bytes[10] == b'T' && index + 6 == bytes.len() => {
            if bytes.get(index + 3) != Some(&b':') {
                return None;
            }
            let offset_hour = digits(bytes, index + 1, 2)? as i64;
            let offset_minute = digits(bytes, index + 4, 2)? as i64;
            if offset_hour > 23 || offset_minute > 59 {
                return None;
            }
            let seconds = offset_hour
                .checked_mul(3600)?
                .checked_add(offset_minute * 60)?;
            if bytes[index] == b'-' {
                -seconds
            } else {
                seconds
            }
        }
        _ => return None,
    };

    let days = days_from_civil(year, month, day)?;
    let leap = i64::from(second == 60);
    let normalized_second = if second == 60 { 59 } else { second } as i64;
    let unix_seconds = days
        .checked_mul(86_400)?
        .checked_add((hour as i64) * 3600)?
        .checked_add((minute as i64) * 60)?
        .checked_add(normalized_second)?
        .checked_add(leap)?
        .checked_sub(offset_seconds)?;
    unix_seconds.checked_mul(1_000_000_000)?.checked_add(nanos)
}

fn digits(input: &[u8], start: usize, count: usize) -> Option<u32> {
    let slice = input.get(start..start + count)?;
    slice.iter().try_fold(0_u32, |value, byte| {
        byte.is_ascii_digit()
            .then(|| value * 10 + u32::from(*byte - b'0'))
    })
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => 0,
    }
}

// Howard Hinnant's proleptic-Gregorian civil-date conversion, with checked final range.
fn days_from_civil(year: i64, month: u32, day: u32) -> Option<i64> {
    let adjusted_year = year - i64::from(month <= 2);
    let era = if adjusted_year >= 0 {
        adjusted_year
    } else {
        adjusted_year - 399
    } / 400;
    let year_of_era = adjusted_year - era * 400;
    let shifted_month = i64::from(month) + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era.checked_mul(146_097)?
        .checked_add(day_of_era)?
        .checked_sub(719_468)
}

#[cfg(test)]
mod tests {
    use super::parse_utc_nanos;

    #[test]
    fn parses_both_ledger_grammars_and_leap_second() {
        assert_eq!(parse_utc_nanos("1970-01-01 00:00:00"), Some(0));
        assert_eq!(parse_utc_nanos("1970-01-01T09:00:00+09:00"), Some(0));
        assert_eq!(
            parse_utc_nanos("1970-01-01T00:00:00.123456789Z"),
            Some(123_456_789)
        );
        assert_eq!(
            parse_utc_nanos("2016-12-31T23:59:60Z"),
            parse_utc_nanos("2017-01-01T00:00:00Z")
        );
    }

    #[test]
    fn rejects_uncontracted_or_invalid_forms() {
        for value in [
            "1970-01-01T00:00:00",
            "1970-01-01 00:00:00Z",
            "1970-02-30 00:00:00",
            "1970-01-01T00:00:00.1234567890Z",
        ] {
            assert_eq!(parse_utc_nanos(value), None, "{value}");
        }
    }
}
