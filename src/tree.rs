//! Directory tree view (Mdir's MCD): the folders around the active panel's
//! folder, expanded along its path, to pick a folder to go to.
//!
//! Pure state: listings arrive through [`TreeView::loaded`] (read on a
//! worker by the app), so nothing here touches the filesystem.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// One subfolder in a listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sub {
    pub name: String,
    pub path: PathBuf,
    /// A link to a folder: shown, never expanded (no loops).
    pub link: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Children {
    Loading,
    Ready(Vec<Sub>),
    Failed(String),
}

/// What a row shows besides its name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowKind {
    Folder {
        expanded: bool,
        link: bool,
    },
    /// Placeholder under an expanded folder still being read.
    Loading,
    /// Placeholder under an expanded folder that could not be read.
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub path: PathBuf,
    pub name: String,
    pub depth: usize,
    pub kind: RowKind,
    /// For each level above and this one: is it the last child there?
    /// (Draws `│`, `├─`, `└─`.)
    pub lasts: Vec<bool>,
}

#[derive(Debug, Clone)]
pub struct TreeView {
    pub root: PathBuf,
    /// The folder the panel was in.
    pub start: PathBuf,
    expanded: HashSet<PathBuf>,
    children: HashMap<PathBuf, Children>,
    /// The folder under the cursor (stays on it while rows change).
    cursor: PathBuf,
    pub scroll: usize,
}

/// `path` and its ancestors, root first.
fn chain(path: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = path.ancestors().map(Path::to_path_buf).collect();
    v.reverse();
    v
}

impl TreeView {
    /// A tree whose root is `start`'s filesystem root, expanded down to
    /// `start`. [`TreeView::wanted`] lists the folders to read.
    pub fn new(start: &Path) -> Self {
        let ch = chain(start);
        let root = ch.first().cloned().unwrap_or_else(|| start.to_path_buf());
        Self {
            root,
            start: start.to_path_buf(),
            expanded: ch.into_iter().collect(),
            children: HashMap::new(),
            cursor: start.to_path_buf(),
            scroll: 0,
        }
    }

    /// Expanded folders with no listing yet; each is marked as loading.
    pub fn wanted(&mut self) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = self
            .expanded
            .iter()
            .filter(|p| !self.children.contains_key(*p))
            .cloned()
            .collect();
        out.sort();
        for p in &out {
            self.children.insert(p.clone(), Children::Loading);
        }
        out
    }

    /// A listing of `dir` arrived, already in [`sort_subs`] order.
    pub fn loaded(&mut self, dir: &Path, result: Result<Vec<Sub>, String>) {
        let c = match result {
            Ok(mut subs) => {
                // a hidden folder on the way to the start is always shown
                if let Some(next) = self
                    .start
                    .ancestors()
                    .find(|a| a.parent() == Some(dir))
                    .filter(|n| !subs.iter().any(|s| s.path == *n))
                {
                    let s = Sub {
                        name: next
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default(),
                        path: next.to_path_buf(),
                        link: false,
                    };
                    let at = subs.partition_point(|x| sort_key(x) < sort_key(&s));
                    subs.insert(at, s);
                }
                Children::Ready(subs)
            }
            Err(e) => Children::Failed(e),
        };
        self.children.insert(dir.to_path_buf(), c);
    }

    pub fn rows(&self) -> Vec<Row> {
        let mut out = Vec::new();
        let name = self.root.to_string_lossy().into_owned();
        self.push(&mut out, &self.root, name, false, 0, Vec::new());
        out
    }

    fn push(
        &self,
        out: &mut Vec<Row>,
        path: &Path,
        name: String,
        link: bool,
        depth: usize,
        lasts: Vec<bool>,
    ) {
        let expanded = !link && self.expanded.contains(path);
        out.push(Row {
            path: path.to_path_buf(),
            name,
            depth,
            kind: RowKind::Folder { expanded, link },
            lasts: lasts.clone(),
        });
        if !expanded {
            return;
        }
        let placeholder = |kind: RowKind| Row {
            path: path.to_path_buf(),
            name: String::new(),
            depth: depth + 1,
            kind,
            lasts: [lasts.as_slice(), &[true]].concat(),
        };
        match self.children.get(path) {
            None | Some(Children::Loading) => out.push(placeholder(RowKind::Loading)),
            Some(Children::Failed(e)) => out.push(placeholder(RowKind::Error(e.clone()))),
            Some(Children::Ready(subs)) => {
                for (i, s) in subs.iter().enumerate() {
                    let last = i + 1 == subs.len();
                    let l = [lasts.as_slice(), &[last]].concat();
                    self.push(out, &s.path, s.name.clone(), s.link, depth + 1, l);
                }
            }
        }
    }

    /// Index of the cursor row (the nearest folder row when the cursor's
    /// folder is not shown).
    pub fn cursor_index(&self, rows: &[Row]) -> usize {
        rows.iter()
            .position(|r| r.path == self.cursor && matches!(r.kind, RowKind::Folder { .. }))
            .or_else(|| {
                // the deepest shown ancestor of the cursor
                chain(&self.cursor).iter().rev().find_map(|a| {
                    rows.iter()
                        .position(|r| r.path == *a && matches!(r.kind, RowKind::Folder { .. }))
                })
            })
            .unwrap_or(0)
    }

    pub fn cursor(&self) -> &Path {
        &self.cursor
    }

    /// Move by `delta` folder rows (placeholders are skipped).
    pub fn move_by(&mut self, delta: isize) {
        let rows = self.rows();
        let folders: Vec<usize> = (0..rows.len())
            .filter(|&i| matches!(rows[i].kind, RowKind::Folder { .. }))
            .collect();
        let here = self.cursor_index(&rows);
        let at = folders.iter().position(|&i| i == here).unwrap_or(0) as isize;
        let to = (at + delta).clamp(0, folders.len() as isize - 1) as usize;
        if let Some(&i) = folders.get(to) {
            self.cursor = rows[i].path.clone();
        }
    }

    pub fn home(&mut self) {
        self.cursor = self.root.clone();
    }

    pub fn end(&mut self) {
        self.move_by(isize::MAX / 2);
    }

    /// Expand the cursor folder; the listings to read (empty if none).
    pub fn expand(&mut self) -> Vec<PathBuf> {
        let rows = self.rows();
        let i = self.cursor_index(&rows);
        if let RowKind::Folder { link: false, .. } = rows[i].kind {
            self.expanded.insert(rows[i].path.clone());
        }
        self.wanted()
    }

    /// Collapse the cursor folder, or when it is collapsed (or a link) go
    /// to its parent.
    pub fn collapse(&mut self) {
        let rows = self.rows();
        let i = self.cursor_index(&rows);
        let p = rows[i].path.clone();
        match rows[i].kind {
            RowKind::Folder {
                expanded: true,
                link: false,
            } if p != self.root || self.expanded.contains(&p) => {
                self.expanded.remove(&p);
                // a later expand reads it afresh
                self.children.remove(&p);
            }
            _ => {
                if let Some(parent) = p.parent().filter(|_| p != self.root) {
                    self.cursor = parent.to_path_buf();
                }
            }
        }
    }
}

fn sort_key(s: &Sub) -> (String, String) {
    (s.name.to_lowercase(), s.name.clone())
}

/// Case-insensitive name order (on the worker: a big folder is not sorted
/// on the UI thread).
pub fn sort_subs(subs: &mut [Sub]) {
    subs.sort_by_cached_key(sort_key);
}

/// The subfolders of `dir` (and links to folders) in [`sort_subs`] order,
/// hidden ones only with `hidden`. Blocking: call off the UI thread.
pub fn read_subdirs(dir: &Path, hidden: bool) -> Result<Vec<Sub>, String> {
    let rd = std::fs::read_dir(dir).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for e in rd.flatten() {
        let Ok(ft) = e.file_type() else { continue };
        let path = e.path();
        let link = ft.is_symlink();
        let is_dir = if link {
            std::fs::metadata(&path).is_ok_and(|m| m.is_dir())
        } else {
            ft.is_dir()
        };
        if !is_dir {
            continue;
        }
        let name = e.file_name().to_string_lossy().into_owned();
        if !hidden
            && e.metadata().map_or(name.starts_with('.'), |m| {
                crate::panel::is_hidden(&name, &m)
            })
        {
            continue;
        }
        out.push(Sub { name, path, link });
    }
    sort_subs(&mut out);
    Ok(out)
}

/// `│ `, `├─`, `└─` prefix of a row.
pub fn prefix(row: &Row) -> String {
    let mut s = String::new();
    // the root has no branch; levels below it draw their parents' lines
    for (i, last) in row.lasts.iter().enumerate() {
        let leaf = i + 1 == row.lasts.len();
        s.push_str(match (leaf, last) {
            (true, true) => "└─",
            (true, false) => "├─",
            (false, true) => "  ",
            (false, false) => "│ ",
        });
    }
    s
}

/// The text of a row: branch lines, `+`/`-` (closed / open) or `→` (link),
/// then the name.
pub fn row_text(row: &Row) -> String {
    let mark = match &row.kind {
        RowKind::Folder { link: true, .. } => "→",
        RowKind::Folder { expanded: true, .. } => "-",
        RowKind::Folder {
            expanded: false, ..
        } => "+",
        RowKind::Loading => {
            return format!("{}{}", prefix(row), crate::i18n::t("읽는 중…", "Loading…"));
        }
        RowKind::Error(e) => {
            return format!(
                "{}{}",
                prefix(row),
                crate::tr!("읽을 수 없음: {e}", "Cannot read: {e}")
            );
        }
    };
    format!("{}{mark}{}", prefix(row), row.name)
}

/// Full-screen tree with `hint` in the bottom border.
pub fn render(
    frame: &mut ratatui::Frame,
    tree: &mut TreeView,
    theme: &crate::theme::Theme,
    hint: &str,
) {
    use ratatui::style::Style;
    use ratatui::text::Line;
    use ratatui::widgets::{Block, Borders, Paragraph};
    let area = frame.area();
    let base = Style::default().fg(theme.fg).bg(theme.bg);
    let title = crate::i18n::t("디렉터리 트리", "Directory tree");
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border_active).bg(theme.bg))
        .title(format!(" {title} "))
        .title_bottom(format!(" {hint} "))
        .style(base);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let rows = tree.rows();
    let h = inner.height as usize;
    let cur = tree.cursor_index(&rows);
    tree.scroll = crate::ui::visible_offset(cur, tree.scroll, h);
    let w = inner.width as usize;
    let lines: Vec<Line> = rows
        .iter()
        .enumerate()
        .skip(tree.scroll)
        .take(h)
        .map(|(i, r)| {
            let style = if i == cur {
                Style::default().fg(theme.cursor_fg).bg(theme.cursor_bg)
            } else {
                match r.kind {
                    RowKind::Folder { link: true, .. } => base.fg(theme.symlink),
                    RowKind::Folder { .. } if r.path == tree.start => base.fg(theme.title),
                    RowKind::Folder { .. } => base.fg(theme.dir),
                    RowKind::Loading => base.fg(theme.hidden),
                    RowKind::Error(_) => base.fg(theme.error),
                }
            };
            Line::styled(crate::ui::fit_width(&row_text(r), w), style)
        })
        .collect();
    frame.render_widget(Paragraph::new(lines).style(base), inner);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sorted(mut v: Vec<Sub>) -> Result<Vec<Sub>, String> {
        sort_subs(&mut v);
        Ok(v)
    }

    fn sub(parent: &str, name: &str) -> Sub {
        Sub {
            name: name.into(),
            path: Path::new(parent).join(name),
            link: false,
        }
    }

    /// `/`, `/home`, `/home/me` loaded; cursor on /home/me.
    fn tree() -> TreeView {
        let mut t = TreeView::new(Path::new("/home/me"));
        let mut want = t.wanted();
        want.sort();
        assert_eq!(want, ["/", "/home", "/home/me"].map(PathBuf::from));
        t.loaded(
            Path::new("/"),
            sorted(vec![sub("/", "usr"), sub("/", "home")]),
        );
        t.loaded(
            Path::new("/home"),
            Ok(vec![sub("/home", "me"), sub("/home", "공유 폴더")]),
        );
        t.loaded(Path::new("/home/me"), sorted(vec![sub("/home/me", "src")]));
        t
    }

    fn names(t: &TreeView) -> Vec<String> {
        t.rows()
            .iter()
            .map(|r| format!("{}{}", "  ".repeat(r.depth), r.name))
            .collect()
    }

    #[test]
    fn opens_along_the_start_folder_with_the_cursor_on_it() {
        let t = tree();
        assert_eq!(
            names(&t),
            [
                "/",
                "  home",
                "    me",
                "      src",
                "    공유 폴더",
                "  usr"
            ]
        );
        let rows = t.rows();
        assert_eq!(rows[t.cursor_index(&rows)].path, Path::new("/home/me"));
        assert_eq!(
            rows[4].lasts,
            [false, true],
            "home is not the last under /, 공유 폴더 is the last under home"
        );
    }

    #[test]
    fn moving_skips_placeholders_and_stays_in_bounds() {
        let mut t = TreeView::new(Path::new("/a"));
        t.wanted();
        t.loaded(Path::new("/"), sorted(vec![sub("/", "a"), sub("/", "b")]));
        // /a is still loading: its placeholder row is not a stop
        assert!(matches!(t.rows()[2].kind, RowKind::Loading));
        t.move_by(1);
        assert_eq!(t.cursor(), Path::new("/b"));
        t.move_by(5);
        assert_eq!(t.cursor(), Path::new("/b"));
        t.home();
        assert_eq!(t.cursor(), Path::new("/"));
        t.move_by(-3);
        assert_eq!(t.cursor(), Path::new("/"));
        t.end();
        assert_eq!(t.cursor(), Path::new("/b"));
    }

    #[test]
    fn expand_reads_once_and_collapse_goes_up_when_closed() {
        let mut t = tree();
        t.move_by(1); // src
        assert_eq!(t.cursor(), Path::new("/home/me/src"));
        assert_eq!(t.expand(), [PathBuf::from("/home/me/src")]);
        assert!(t.expand().is_empty(), "already being read");
        t.loaded(Path::new("/home/me/src"), sorted(vec![]));
        assert!(names(&t).contains(&"      src".to_string()));
        t.collapse(); // closes src
        assert!(t.expand() == [PathBuf::from("/home/me/src")], "read afresh");
        t.collapse();
        t.collapse(); // src closed: up to me
        assert_eq!(t.cursor(), Path::new("/home/me"));
        t.collapse(); // me open: closes it
        assert!(!names(&t).contains(&"      src".to_string()));
        t.collapse();
        assert_eq!(t.cursor(), Path::new("/home"));
    }

    #[test]
    fn links_and_unreadable_folders() {
        let mut t = TreeView::new(Path::new("/"));
        t.wanted();
        let mut l = sub("/", "loop");
        l.link = true;
        t.loaded(Path::new("/"), sorted(vec![l, sub("/", "locked")]));
        t.move_by(1); // locked (sorted first)
        assert_eq!(t.cursor(), Path::new("/locked"));
        t.move_by(1);
        assert_eq!(t.cursor(), Path::new("/loop"));
        assert!(t.expand().is_empty(), "a link is never expanded");
        t.move_by(-1);
        assert_eq!(t.expand(), [PathBuf::from("/locked")]);
        t.loaded(Path::new("/locked"), Err("permission denied".into()));
        let rows = t.rows();
        assert_eq!(rows[2].kind, RowKind::Error("permission denied".into()));
    }

    #[test]
    fn rows_draw_branches_marks_and_placeholders() {
        let mut t = tree();
        t.move_by(1);
        t.expand(); // src: loading
        let text: Vec<String> = t.rows().iter().map(row_text).collect();
        assert_eq!(
            text,
            [
                "-/",
                "├─-home",
                "│ ├─-me",
                "│ │ └─-src",
                "│ │   └─읽는 중…",
                "│ └─+공유 폴더",
                "└─+usr",
            ]
        );
    }

    #[test]
    fn real_subdirs_skip_files_and_hidden_ones_and_mark_links() {
        let t = tempfile::tempdir().unwrap();
        std::fs::create_dir(t.path().join("한글 폴더")).unwrap();
        std::fs::create_dir(t.path().join(".hidden")).unwrap();
        std::fs::write(t.path().join("file.txt"), "x").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(t.path().join("한글 폴더"), t.path().join("link")).unwrap();
        let mut got = read_subdirs(t.path(), false).unwrap();
        got.sort_by(|a, b| a.name.cmp(&b.name));
        let names: Vec<(&str, bool)> = got.iter().map(|s| (s.name.as_str(), s.link)).collect();
        if cfg!(unix) {
            assert_eq!(names, [("link", true), ("한글 폴더", false)]);
        } else {
            assert_eq!(names, [("한글 폴더", false)]);
        }
        assert!(
            read_subdirs(t.path(), true)
                .unwrap()
                .iter()
                .any(|s| s.name == ".hidden")
        );
        assert!(read_subdirs(&t.path().join("missing"), false).is_err());
    }

    #[test]
    fn a_hidden_folder_on_the_way_to_the_start_is_shown() {
        let mut t = TreeView::new(Path::new("/home/.config/app"));
        t.wanted();
        t.loaded(Path::new("/"), sorted(vec![sub("/", "home")]));
        t.loaded(Path::new("/home"), sorted(vec![sub("/home", "docs")]));
        t.loaded(
            Path::new("/home/.config"),
            Ok(vec![sub("/home/.config", "app")]),
        );
        t.loaded(Path::new("/home/.config/app"), sorted(vec![]));
        assert_eq!(
            names(&t),
            ["/", "  home", "    .config", "      app", "    docs"]
        );
        let rows = t.rows();
        assert_eq!(
            rows[t.cursor_index(&rows)].path,
            Path::new("/home/.config/app")
        );
    }

    fn draw(t: &mut TreeView, w: u16, h: u16) -> ratatui::buffer::Buffer {
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        let theme = crate::theme::builtin("mdir").unwrap();
        term.draw(|f| render(f, t, &theme, "Esc")).unwrap();
        term.backend().buffer().clone()
    }

    fn text_rows(b: &ratatui::buffer::Buffer) -> Vec<String> {
        (0..b.area.height)
            .map(|y| {
                let mut row = String::new();
                let mut x = 0;
                while x < b.area.width {
                    let sym = b[(x, y)].symbol();
                    row.push_str(sym);
                    x += unicode_width::UnicodeWidthStr::width(sym).max(1) as u16;
                }
                row
            })
            .collect()
    }

    #[test]
    fn tree_screen_snapshot_with_cursor_and_styles() {
        let theme = crate::theme::builtin("mdir").unwrap();
        let mut t = tree();
        let b = crate::i18n::with_lang(crate::i18n::Lang::En, || draw(&mut t, 30, 9));
        assert_eq!(
            text_rows(&b),
            [
                "┌ Directory tree ────────────┐",
                "│-/                          │",
                "│├─-home                     │",
                "││ ├─-me                     │",
                "││ │ └─+src                  │",
                "││ └─+공유 폴더              │",
                "│└─+usr                      │",
                "│                            │",
                "└ Esc ───────────────────────┘",
            ]
        );
        assert_eq!(b[(5, 3)].bg, theme.cursor_bg, "cursor on me");
        assert_eq!(b[(5, 2)].fg, theme.dir);
        assert_eq!(b[(5, 7)].bg, theme.bg);
    }

    #[test]
    fn a_short_screen_scrolls_to_the_cursor() {
        let theme = crate::theme::builtin("mdir").unwrap();
        let mut t = tree();
        t.end(); // usr, the last row
        let b = draw(&mut t, 30, 4);
        let rows = text_rows(&b);
        assert!(rows[2].contains("usr"), "{rows:?}");
        assert_eq!(b[(5, 2)].bg, theme.cursor_bg);
        // tiny screens never panic
        for (w, h) in [(0, 0), (1, 1), (5, 2), (3, 10)] {
            draw(&mut t, w, h);
        }
    }

    #[test]
    fn the_cursor_falls_back_to_the_nearest_shown_ancestor() {
        let mut t = tree();
        t.home();
        t.collapse(); // closes the root
        assert_eq!(t.rows().len(), 1);
        let mut t = tree();
        t.cursor = PathBuf::from("/home/me/src/deep");
        let rows = t.rows();
        assert_eq!(rows[t.cursor_index(&rows)].path, Path::new("/home/me/src"));
    }
}
