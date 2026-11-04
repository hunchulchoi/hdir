//! Local time and the Mdir-style date, time and size texts.

use std::time::{SystemTime, UNIX_EPOCH};

/// A moment in local time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalTime {
    pub year: i32,
    /// 1-12
    pub month: u32,
    /// 1-31
    pub day: u32,
    /// 0-23
    pub hour: u32,
    pub minute: u32,
    /// 0 = Sunday
    pub weekday: u32,
}

impl LocalTime {
    /// `YY-MM-DD`, as Mdir lists dates.
    pub fn short_date(&self) -> String {
        format!(
            "{:02}-{:02}-{:02}",
            self.year.rem_euclid(100),
            self.month,
            self.day
        )
    }

    /// `h:mma` / `h:mmp`, 12-hour like Mdir (`10:20p`, `9:28a`).
    pub fn short_time(&self) -> String {
        let h = match self.hour % 12 {
            0 => 12,
            h => h,
        };
        let ap = if self.hour < 12 { 'a' } else { 'p' };
        format!("{h}:{:02}{ap}", self.minute)
    }

    /// `YYYY-MM-DD HH:MM`.
    pub fn long(&self) -> String {
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}",
            self.year, self.month, self.day, self.hour, self.minute
        )
    }

    /// Short weekday name in the screen language.
    pub fn weekday_name(&self) -> &'static str {
        const KO: [&str; 7] = ["일", "월", "화", "수", "목", "금", "토"];
        const EN: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
        let i = (self.weekday % 7) as usize;
        crate::i18n::t(KO[i], EN[i])
    }
}

/// Whole seconds since the epoch, rounded down (negative before it).
pub fn unix_secs(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        // round down: 0.5 s before the epoch is second -1
        Err(e) => {
            let d = e.duration();
            -(d.as_secs() as i64) - i64::from(d.subsec_nanos() > 0)
        }
    }
}

/// `ts` (unix seconds) in the local time zone, with the rules in force at
/// that instant.
#[cfg(unix)]
pub fn local(ts: i64) -> Option<LocalTime> {
    let t = ts as libc::time_t;
    // SAFETY: `tm` is plain data; localtime_r only writes into it.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers are valid for the duration of the call.
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
        return None;
    }
    Some(LocalTime {
        year: tm.tm_year + 1900,
        month: (tm.tm_mon + 1) as u32,
        day: tm.tm_mday as u32,
        hour: tm.tm_hour as u32,
        minute: tm.tm_min as u32,
        weekday: tm.tm_wday as u32,
    })
}

#[cfg(windows)]
pub fn local(ts: i64) -> Option<LocalTime> {
    use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows_sys::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
    // Unix seconds -> 100ns ticks since 1601-01-01.
    let ticks = ts
        .checked_add(11_644_473_600)
        .filter(|s| *s >= 0)
        .and_then(|s| (s as u64).checked_mul(10_000_000))?;
    let utc = FILETIME {
        dwLowDateTime: ticks as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    };
    // SAFETY: plain-data structs; all pointers are valid for the call.
    let mut utc_st: SYSTEMTIME = unsafe { std::mem::zeroed() };
    let mut st: SYSTEMTIME = unsafe { std::mem::zeroed() };
    // The zone conversion uses the DST rules in force *at that instant*
    // (FileTimeToLocalFileTime would apply today's offset to every date).
    // SAFETY: as above; a null zone means the current time zone.
    let ok = unsafe {
        FileTimeToSystemTime(&utc, &mut utc_st) != 0
            && SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc_st, &mut st) != 0
    };
    ok.then_some(LocalTime {
        year: st.wYear as i32,
        month: st.wMonth as u32,
        day: st.wDay as u32,
        hour: st.wHour as u32,
        minute: st.wMinute as u32,
        weekday: st.wDayOfWeek as u32,
    })
}

/// `n` with thousands separators: `1,238,417`.
pub fn group_digits(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(hour: u32, minute: u32) -> LocalTime {
        LocalTime {
            year: 2025,
            month: 11,
            day: 7,
            hour,
            minute,
            weekday: 5,
        }
    }

    #[test]
    fn mdir_dates_and_times() {
        assert_eq!(at(22, 20).short_date(), "25-11-07");
        assert_eq!(at(22, 20).short_time(), "10:20p");
        assert_eq!(at(9, 28).short_time(), "9:28a");
        assert_eq!(at(0, 5).short_time(), "12:05a");
        assert_eq!(at(12, 0).short_time(), "12:00p");
        assert_eq!(at(9, 5).long(), "2025-11-07 09:05");
        let l = LocalTime {
            year: 2000,
            ..at(1, 1)
        };
        assert_eq!(l.short_date(), "00-11-07");
    }

    #[test]
    fn weekday_names_follow_the_language() {
        use crate::i18n::{Lang, with_lang};
        assert_eq!(with_lang(Lang::Ko, || at(1, 1).weekday_name()), "금");
        assert_eq!(with_lang(Lang::En, || at(1, 1).weekday_name()), "Fri");
    }

    #[test]
    fn digits_are_grouped_by_three() {
        assert_eq!(group_digits(0), "0");
        assert_eq!(group_digits(999), "999");
        assert_eq!(group_digits(1000), "1,000");
        assert_eq!(group_digits(1_238_417), "1,238,417");
        assert_eq!(group_digits(202_584_666_112), "202,584,666,112");
    }

    #[test]
    fn the_epoch_is_a_real_local_moment() {
        let l = local(0).expect("local time");
        assert!(l.year == 1970 || l.year == 1969, "{l:?}");
        assert!(l.weekday < 7 && (1..=12).contains(&l.month));
        assert_eq!(unix_secs(UNIX_EPOCH + std::time::Duration::from_secs(5)), 5);
        // before the epoch, a part second rounds down, not toward zero
        let half = std::time::Duration::from_millis(500);
        assert_eq!(unix_secs(UNIX_EPOCH - half), -1);
        assert_eq!(
            unix_secs(UNIX_EPOCH - std::time::Duration::from_secs(2)),
            -2
        );
    }
}
