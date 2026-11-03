//! Mdir-style list rows for the Classic screen: fixed-width name,
//! extension, size (or `[ SubDir ]`), date and time columns.

use std::time::SystemTime;

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::clock::{LocalTime, group_digits};
use crate::panel::{Entry, EntryKind};

/// Width of the size column: fits `[ SubDir ]` and `99,999,999`.
pub const SIZE_W: usize = 10;
const DATE_W: usize = 8;
const TIME_W: usize = 6;

/// Column widths for a list `width` cells wide; a missing column is `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cols {
    pub name: usize,
    pub ext: Option<usize>,
    pub size: bool,
    pub date: bool,
    pub time: bool,
}

/// The widest layout that still leaves the name 8 cells (Mdir's 8.3): time,
/// then date, then the extension column go first as the list narrows.
pub fn cols(width: usize) -> Cols {
    let ext = if width >= 48 { 4 } else { 3 };
    let full = ext + SIZE_W + DATE_W + TIME_W + 4;
    let no_time = ext + SIZE_W + DATE_W + 3;
    let no_date = ext + SIZE_W + 2;
    let pick = |fixed: usize, date: bool, time: bool| Cols {
        name: width - fixed,
        ext: Some(ext),
        size: true,
        date,
        time,
    };
    if width >= full + 8 {
        pick(full, true, true)
    } else if width >= no_time + 8 {
        pick(no_time, true, false)
    } else if width >= no_date + 6 {
        pick(no_date, false, false)
    } else if width > SIZE_W + 4 {
        Cols {
            name: width - SIZE_W - 1,
            ext: None,
            size: true,
            date: false,
            time: false,
        }
    } else {
        Cols {
            name: width,
            ext: None,
            size: false,
            date: false,
            time: false,
        }
    }
}

/// `s` cut to `width` cells, ending in `~` when cut, padded with spaces to
/// exactly `width`.
pub fn fit_tilde(s: &str, width: usize) -> String {
    if s.width() <= width {
        return format!("{s}{}", " ".repeat(width - s.width()));
    }
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for g in s.graphemes(true) {
        let w = g.width();
        if used + w > width - 1 {
            break;
        }
        out.push_str(g);
        used += w;
    }
    out.push('~');
    used += 1;
    out.push_str(&" ".repeat(width - used));
    out
}

pub fn is_dirlike(e: &Entry) -> bool {
    matches!(
        e.kind,
        EntryKind::Dir
            | EntryKind::Symlink {
                target_is_dir: true
            }
    )
}

/// `(stem, extension)` of a file name: `a.tar.gz` -> (`a.tar`, `gz`).
/// Folders, `..`, dotfiles like `.bashrc` and names ending in `.` keep the
/// whole name.
pub fn split_name(e: &Entry) -> (&str, &str) {
    if is_dirlike(e) || e.kind == EntryKind::Parent {
        return (&e.name, "");
    }
    match e.name.rfind('.') {
        Some(i) if i > 0 && i + 1 < e.name.len() => (&e.name[..i], &e.name[i + 1..]),
        _ => (&e.name, ""),
    }
}

/// `[ UpDir ]`, `[ SubDir ]`, or the size with separators (a short unit
/// form when that does not fit).
pub fn size_text(e: &Entry) -> String {
    if e.kind == EntryKind::Parent {
        return "[ UpDir ]".to_string();
    }
    if is_dirlike(e) {
        return "[ SubDir ]".to_string();
    }
    let g = group_digits(e.size);
    if g.len() <= SIZE_W {
        g
    } else {
        crate::ui::human_size(e.size)
    }
}

/// One list row, exactly `width` cells. `local` turns the modification time
/// into local time (injected so tests do not depend on the time zone).
pub fn row(e: &Entry, width: usize, local: impl Fn(SystemTime) -> Option<LocalTime>) -> String {
    let c = cols(width);
    let mut out = String::new();
    match c.ext {
        Some(ext_w) => {
            let (stem, ext) = split_name(e);
            if ext.is_empty() {
                // a folder or a name without extension may use both columns
                out.push_str(&fit_tilde(stem, c.name + 1 + ext_w));
            } else {
                out.push_str(&fit_tilde(stem, c.name));
                out.push(' ');
                out.push_str(&fit_tilde(ext, ext_w));
            }
        }
        None => out.push_str(&fit_tilde(&e.name, c.name)),
    }
    if c.size {
        out.push_str(&format!(" {:>SIZE_W$}", size_text(e)));
    }
    let when = (e.kind != EntryKind::Parent)
        .then_some(e.modified)
        .flatten()
        .and_then(&local);
    if c.date {
        let d = when.map(|l| l.short_date()).unwrap_or_default();
        out.push_str(&format!(" {d:>DATE_W$}"));
    }
    if c.time {
        let t = when.map(|l| l.short_time()).unwrap_or_default();
        out.push_str(&format!(" {t:>TIME_W$}"));
    }
    out
}

/// [`row`] in the real local time zone.
pub fn row_local(e: &Entry, width: usize) -> String {
    row(e, width, |t| {
        crate::clock::local(crate::clock::unix_secs(t))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn entry(name: &str, kind: EntryKind, size: u64) -> Entry {
        Entry {
            name: name.to_string(),
            os_name: OsString::from(name),
            kind,
            size,
            modified: Some(SystemTime::UNIX_EPOCH),
            mode: 0o644,
            hidden: false,
        }
    }

    fn fixed(_: SystemTime) -> Option<LocalTime> {
        Some(LocalTime {
            year: 2025,
            month: 11,
            day: 17,
            hour: 22,
            minute: 20,
            weekday: 1,
        })
    }

    #[test]
    fn rows_line_up_like_mdir_at_its_usual_width() {
        let w = 39;
        assert_eq!(
            row(&entry("GAMES", EntryKind::Dir, 0), w, fixed),
            "GAMES        [ SubDir ] 25-11-17 10:20p"
        );
        assert_eq!(
            row(&entry("ARJ.ZIP", EntryKind::File, 1_238_417), w, fixed),
            "ARJ      ZIP  1,238,417 25-11-17 10:20p"
        );
        assert_eq!(
            row(&entry("..", EntryKind::Parent, 0), w, fixed),
            "..            [ UpDir ]                "
        );
        assert_eq!(
            row(&entry("README", EntryKind::File, 83), w, fixed),
            "README               83 25-11-17 10:20p"
        );
        for r in [
            row(&entry("GAMES", EntryKind::Dir, 0), w, fixed),
            row(&entry("..", EntryKind::Parent, 0), w, fixed),
        ] {
            assert_eq!(r.width(), w, "{r:?}");
        }
    }

    #[test]
    fn long_and_korean_names_are_cut_with_a_tilde() {
        let w = 39;
        let r = row(
            &entry("2026 여행 계획.docx", EntryKind::File, 28_734),
            w,
            fixed,
        );
        assert_eq!(r, "2026 여~ do~     28,734 25-11-17 10:20p");
        assert_eq!(r.width(), w);
        // a wide character never straddles the cut
        assert_eq!(fit_tilde("가나다라", 6), "가나~ ");
        assert_eq!(fit_tilde("ab", 4), "ab  ");
    }

    #[test]
    fn names_split_at_the_last_dot_except_folders_and_dotfiles() {
        let f = |n: &str| entry(n, EntryKind::File, 0);
        assert_eq!(split_name(&f("a.tar.gz")), ("a.tar", "gz"));
        assert_eq!(split_name(&f(".bashrc")), (".bashrc", ""));
        assert_eq!(split_name(&f("trail.")), ("trail.", ""));
        assert_eq!(split_name(&entry("v1.2", EntryKind::Dir, 0)), ("v1.2", ""));
    }

    #[test]
    fn huge_sizes_fall_back_to_units() {
        assert_eq!(
            size_text(&entry("a", EntryKind::File, 9_999_999_999)),
            "9.3G"
        );
        assert_eq!(
            size_text(&entry("a", EntryKind::File, 99_999_999)),
            "99,999,999"
        );
    }

    #[test]
    fn narrow_lists_drop_time_then_date_then_the_extension() {
        assert!(cols(39).time && cols(39).date);
        let c = cols(33);
        assert!(!c.time && c.date, "{c:?}");
        let c = cols(22);
        assert!(!c.date && c.ext.is_some(), "{c:?}");
        let c = cols(16);
        assert!(c.ext.is_none() && c.size, "{c:?}");
        let c = cols(8);
        assert!(!c.size && c.name == 8, "{c:?}");
        for w in 1..120 {
            for e in [
                entry("한글 이름이 아주 긴 파일.txt", EntryKind::File, 5),
                entry("dir", EntryKind::Dir, 0),
            ] {
                assert_eq!(row(&e, w, fixed).width(), w, "width {w}");
            }
        }
    }
}
