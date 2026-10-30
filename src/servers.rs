//! Server list: `~/.ssh/config` parsing, `config.toml` `[[servers]]` merge,
//! and per-server theme resolution. Pure logic (file reads only in `load_ssh_config`).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::Deserialize;

const MAX_INCLUDE_DEPTH: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshHost {
    pub alias: String,
    pub hostname: Option<String>,
    pub user: Option<String>,
    pub port: Option<u16>,
    /// File the host came from (empty when produced by `parse_ssh_config` alone).
    pub source: PathBuf,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct ServerConfig {
    pub name: Option<String>,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub path: Option<String>,
    pub theme: Option<String>,
    #[serde(rename = "match")]
    pub match_glob: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Server {
    pub name: String,
    /// ssh destination: alias or user@host
    pub host: String,
    pub port: Option<u16>,
    pub start_path: Option<String>,
    pub theme: Option<String>,
    pub from_ssh_config: bool,
}

/// Split a value string into whitespace-separated tokens, honoring double
/// quotes; an unquoted `#` at a token start begins a comment.
fn tokenize(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_tok = false;
    let mut quoted = false;
    for c in s.chars() {
        if quoted {
            if c == '"' {
                quoted = false;
            } else {
                cur.push(c);
            }
        } else if c == '"' {
            quoted = true;
            in_tok = true;
        } else if c.is_whitespace() {
            if in_tok {
                out.push(std::mem::take(&mut cur));
                in_tok = false;
            }
        } else if c == '#' && !in_tok {
            break;
        } else {
            cur.push(c);
            in_tok = true;
        }
    }
    if in_tok {
        out.push(cur);
    }
    out
}

/// Split a config line into (lowercased keyword, value tokens).
fn split_line(line: &str) -> Option<(String, Vec<String>)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let end = line
        .find(|c: char| c.is_whitespace() || c == '=')
        .unwrap_or(line.len());
    let kw = line[..end].to_lowercase();
    let rest = line[end..].trim_start();
    let rest = rest.strip_prefix('=').unwrap_or(rest).trim_start();
    Some((kw, tokenize(rest)))
}

fn has_wildcard(s: &str) -> bool {
    s.contains(['*', '?', '!'])
}

fn resolve_include(target: &str, base_dir: &Path) -> PathBuf {
    if let Some(rest) = target.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    let p = Path::new(target);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        base_dir.join(p)
    }
}

/// Parse ssh_config text. Returns hosts (without `source`), Include targets
/// (resolved against `base_dir`, globs left unexpanded) and warnings.
pub fn parse_ssh_config(text: &str, base_dir: &Path) -> (Vec<SshHost>, Vec<PathBuf>, Vec<String>) {
    let mut hosts: Vec<SshHost> = Vec::new();
    let mut includes = Vec::new();
    let mut warnings = Vec::new();
    // Indices into `hosts` for the current Host block; empty for Match/global/wildcard blocks.
    let mut current: Vec<usize> = Vec::new();
    // Per-block "already set" flags (first value wins).
    let (mut got_host, mut got_user, mut got_port) = (false, false, false);

    for (n, line) in text.lines().enumerate() {
        let Some((kw, vals)) = split_line(line) else {
            continue;
        };
        match kw.as_str() {
            "host" => {
                current.clear();
                (got_host, got_user, got_port) = (false, false, false);
                for v in vals.iter().filter(|v| !has_wildcard(v)) {
                    hosts.push(SshHost {
                        alias: v.clone(),
                        hostname: None,
                        user: None,
                        port: None,
                        source: PathBuf::new(),
                    });
                    current.push(hosts.len() - 1);
                }
            }
            "match" => current.clear(),
            "include" => includes.extend(vals.iter().map(|v| resolve_include(v, base_dir))),
            "hostname" if !got_host && !current.is_empty() => {
                if let Some(v) = vals.first() {
                    got_host = true;
                    current
                        .iter()
                        .for_each(|&i| hosts[i].hostname = Some(v.clone()));
                }
            }
            "user" if !got_user && !current.is_empty() => {
                if let Some(v) = vals.first() {
                    got_user = true;
                    current
                        .iter()
                        .for_each(|&i| hosts[i].user = Some(v.clone()));
                }
            }
            "port" if !got_port && !current.is_empty() => {
                if let Some(v) = vals.first() {
                    match v.parse::<u16>() {
                        Ok(p) => {
                            got_port = true;
                            current.iter().for_each(|&i| hosts[i].port = Some(p));
                        }
                        Err(_) => warnings.push(format!("line {}: invalid Port '{v}'", n + 1)),
                    }
                }
            }
            _ => {}
        }
    }
    (hosts, includes, warnings)
}

/// Expand `*`/`?`/`[..]` in any path component against the filesystem.
fn expand_glob(pattern: &Path) -> Vec<PathBuf> {
    let mut frontier = vec![PathBuf::new()];
    for comp in pattern.components() {
        let name = comp.as_os_str().to_string_lossy().into_owned();
        let meta =
            matches!(comp, std::path::Component::Normal(_)) && name.contains(['*', '?', '[']);
        let mut next = Vec::new();
        for base in &frontier {
            if !meta {
                next.push(base.join(comp.as_os_str()));
                continue;
            }
            let Ok(rd) = std::fs::read_dir(base) else {
                continue;
            };
            let mut names: Vec<_> = rd
                .flatten()
                .map(|e| e.file_name())
                .filter(|f| glob_match_impl(&name, &f.to_string_lossy(), false))
                .collect();
            names.sort();
            next.extend(names.into_iter().map(|f| base.join(f)));
        }
        frontier = next;
    }
    frontier
}

fn load_into(
    path: &Path,
    base_dir: &Path,
    depth: usize,
    stack: &mut Vec<PathBuf>,
    hosts: &mut Vec<SshHost>,
    warnings: &mut Vec<String>,
) {
    let canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if stack.contains(&canon) {
        warnings.push(format!("include cycle skipped: {}", path.display()));
        return;
    }
    let Ok(text) = std::fs::read_to_string(path) else {
        if path.exists() {
            warnings.push(format!("cannot read {}", path.display()));
        }
        return;
    };
    let (mut found, includes, warns) = parse_ssh_config(&text, base_dir);
    for h in &mut found {
        h.source = path.to_path_buf();
    }
    hosts.extend(found);
    warnings.extend(
        warns
            .into_iter()
            .map(|w| format!("{}: {w}", path.display())),
    );
    if includes.is_empty() {
        return;
    }
    if depth >= MAX_INCLUDE_DEPTH {
        warnings.push(format!(
            "include depth limit ({MAX_INCLUDE_DEPTH}) reached at {}",
            path.display()
        ));
        return;
    }
    stack.push(canon);
    for inc in includes {
        for file in expand_glob(&inc) {
            if file.is_file() {
                load_into(&file, base_dir, depth + 1, stack, hosts, warnings);
            }
        }
    }
    stack.pop();
}

/// Load an ssh config file, following `Include` (relative to the file's directory,
/// i.e. `~/.ssh`), with globs, max depth 8 and cycle protection.
pub fn load_ssh_config(path: &Path) -> (Vec<SshHost>, Vec<String>) {
    let base = path.parent().unwrap_or(Path::new("."));
    let (mut hosts, mut warnings) = (Vec::new(), Vec::new());
    load_into(path, base, 0, &mut Vec::new(), &mut hosts, &mut warnings);
    (hosts, warnings)
}

/// Case-insensitive glob (`*`, `?`, `[abc]`, `[a-z]`, `[!x]`).
pub fn glob_match(pattern: &str, text: &str) -> bool {
    glob_match_impl(pattern, text, true)
}

fn glob_match_impl(pattern: &str, text: &str, ci: bool) -> bool {
    let norm = |s: &str| -> Vec<char> {
        if ci {
            s.to_lowercase().chars().collect()
        } else {
            s.chars().collect()
        }
    };
    glob_rec(&norm(pattern), &norm(text))
}

fn glob_rec(p: &[char], t: &[char]) -> bool {
    match p.first() {
        None => t.is_empty(),
        Some('*') => (0..=t.len()).any(|i| glob_rec(&p[1..], &t[i..])),
        Some('?') => !t.is_empty() && glob_rec(&p[1..], &t[1..]),
        Some('[') => {
            let Some(close) = p.iter().skip(2).position(|&c| c == ']').map(|i| i + 2) else {
                return t.first() == Some(&'[') && glob_rec(&p[1..], &t[1..]);
            };
            let Some(&c) = t.first() else { return false };
            let mut class = &p[1..close];
            let neg = matches!(class.first(), Some('!' | '^'));
            if neg {
                class = &class[1..];
            }
            let mut hit = false;
            let mut i = 0;
            while i < class.len() {
                if i + 2 < class.len() && class[i + 1] == '-' {
                    hit |= class[i] <= c && c <= class[i + 2];
                    i += 3;
                } else {
                    hit |= class[i] == c;
                    i += 1;
                }
            }
            hit != neg && glob_rec(&p[close + 1..], &t[1..])
        }
        Some(&c) => t.first() == Some(&c) && glob_rec(&p[1..], &t[1..]),
    }
}

/// Merge ssh aliases with `[[servers]]` entries: config entries first (file
/// order; same-named ssh aliases are overridden in place), then remaining ssh
/// aliases alphabetically. Entries with only `match` are theme rules.
pub fn merge(ssh: &[SshHost], cfg: &[ServerConfig]) -> (Vec<Server>, Vec<String>) {
    let mut warnings = Vec::new();
    let mut aliases: Vec<&str> = ssh.iter().map(|h| h.alias.as_str()).collect();
    aliases.sort();
    aliases.dedup();

    let mut servers = Vec::new();
    let mut used: HashSet<String> = HashSet::new();
    for (i, c) in cfg.iter().enumerate() {
        let Some(host) = c.host.clone() else {
            if c.match_glob.is_none() {
                warnings.push(format!("[[servers]] #{}: needs host or match", i + 1));
            }
            continue; // match-only entry: theme rule
        };
        let name = c.name.clone().unwrap_or_else(|| host.clone());
        if !used.insert(name.clone()) {
            warnings.push(format!("[[servers]] #{}: duplicate name '{name}'", i + 1));
            continue;
        }
        // an explicit host that differs from the alias name wins over the ssh alias
        let is_alias = aliases.contains(&name.as_str()) && host == name;
        servers.push(Server {
            host,
            name,
            port: c.port,
            start_path: c.path.clone(),
            theme: c.theme.clone(),
            from_ssh_config: is_alias,
        });
    }
    for a in aliases {
        if !used.contains(a) {
            servers.push(Server {
                name: a.to_string(),
                host: a.to_string(),
                port: None,
                start_path: None,
                theme: None,
                from_ssh_config: true,
            });
        }
    }
    (servers, warnings)
}

/// Theme resolution: exact name entry -> first `match` glob -> global.
pub fn theme_for(server: &Server, cfg: &[ServerConfig], global: &str) -> String {
    if let Some(t) = cfg
        .iter()
        .find(|c| c.name.as_deref() == Some(server.name.as_str()) && c.theme.is_some())
        .and_then(|c| c.theme.clone())
    {
        return t;
    }
    cfg.iter()
        .filter_map(|c| Some((c.match_glob.as_deref()?, c.theme.as_ref()?)))
        .find(|(g, _)| glob_match(g, &server.name) || glob_match(g, &server.host))
        .map(|(_, t)| t.clone())
        .unwrap_or_else(|| global.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn parse(text: &str) -> Vec<SshHost> {
        parse_ssh_config(text, Path::new("/home/u/.ssh")).0
    }

    fn aliases(h: &[SshHost]) -> Vec<&str> {
        h.iter().map(|h| h.alias.as_str()).collect()
    }

    #[test]
    fn basic_parse() {
        let h = parse(
            "Host prod\n  HostName 10.0.0.1\n  User deploy\n  Port 2222\nHost dev\n  HostName dev.example.com\n",
        );
        assert_eq!(h.len(), 2);
        assert_eq!(h[0].alias, "prod");
        assert_eq!(h[0].hostname.as_deref(), Some("10.0.0.1"));
        assert_eq!(h[0].user.as_deref(), Some("deploy"));
        assert_eq!(h[0].port, Some(2222));
        assert_eq!(h[1].alias, "dev");
        assert_eq!(h[1].user, None);
    }

    #[test]
    fn case_insensitive_keywords() {
        let h = parse("hOsT a\n  hostname x\n  USER bob\n  PORT 22\n");
        assert_eq!(h[0].hostname.as_deref(), Some("x"));
        assert_eq!(h[0].user.as_deref(), Some("bob"));
        assert_eq!(h[0].port, Some(22));
    }

    #[test]
    fn equals_syntax() {
        let h = parse("Host=a\n  HostName=1.2.3.4\n  Port = 99\n  User =bob\n");
        assert_eq!(aliases(&h), ["a"]);
        assert_eq!(h[0].hostname.as_deref(), Some("1.2.3.4"));
        assert_eq!(h[0].port, Some(99));
        assert_eq!(h[0].user.as_deref(), Some("bob"));
    }

    #[test]
    fn quoted_values() {
        let h = parse("Host \"my box\" other\n  User \"john doe\"\n");
        assert_eq!(aliases(&h), ["my box", "other"]);
        assert_eq!(h[0].user.as_deref(), Some("john doe"));
    }

    #[test]
    fn comments_ignored() {
        let h = parse("# top\nHost a # trailing\n  # inner\n  HostName x # c\n\n");
        assert_eq!(aliases(&h), ["a"]);
        assert_eq!(h[0].hostname.as_deref(), Some("x"));
    }

    #[test]
    fn korean_comments_and_alias() {
        let h = parse("# 운영 서버\nHost 운영\n  # 호스트 이름\n  HostName 1.1.1.1\n");
        assert_eq!(aliases(&h), ["운영"]);
        assert_eq!(h[0].hostname.as_deref(), Some("1.1.1.1"));
    }

    #[test]
    fn multiple_aliases() {
        let h = parse("Host a b c\n  User u\n");
        assert_eq!(aliases(&h), ["a", "b", "c"]);
        assert!(h.iter().all(|h| h.user.as_deref() == Some("u")));
    }

    #[test]
    fn wildcards_and_negation_skipped() {
        let h = parse("Host *\n  User all\nHost a* b?\nHost real !bad *.x\n  User r\n");
        assert_eq!(aliases(&h), ["real"]);
        assert_eq!(h[0].user.as_deref(), Some("r"));
    }

    #[test]
    fn match_block_skipped() {
        let h =
            parse("Host a\n  User one\nMatch host foo\n  User two\n  Port 1\nHost b\n  Port 5\n");
        assert_eq!(aliases(&h), ["a", "b"]);
        assert_eq!(h[0].user.as_deref(), Some("one"));
        assert_eq!(h[0].port, None);
        assert_eq!(h[1].port, Some(5));
    }

    #[test]
    fn global_options_before_host_ignored() {
        let h = parse("User global\nPort 1\nHost a\n");
        assert_eq!(h[0].user, None);
        assert_eq!(h[0].port, None);
    }

    #[test]
    fn first_value_wins() {
        let h = parse("Host a\n  HostName first\n  HostName second\n  Port 1\n  Port 2\n");
        assert_eq!(h[0].hostname.as_deref(), Some("first"));
        assert_eq!(h[0].port, Some(1));
    }

    #[test]
    fn bad_port_warns() {
        let (h, _, w) = parse_ssh_config("Host a\n  Port abc\n", Path::new("/x"));
        assert_eq!(h[0].port, None);
        assert_eq!(w.len(), 1);
    }

    #[test]
    fn include_targets_resolved() {
        let (_, inc, _) = parse_ssh_config(
            "Include config.d/*\nInclude /abs/file \"sp ace\"\n",
            Path::new("/home/u/.ssh"),
        );
        assert_eq!(
            inc,
            [
                PathBuf::from("/home/u/.ssh/config.d/*"),
                PathBuf::from("/abs/file"),
                PathBuf::from("/home/u/.ssh/sp ace"),
            ]
        );
    }

    #[test]
    fn glob_matcher() {
        assert!(glob_match("stg-*", "stg-web"));
        assert!(glob_match("STG-*", "stg-web"));
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "ac"));
        assert!(glob_match("web[12]", "web2"));
        assert!(!glob_match("web[12]", "web3"));
        assert!(glob_match("web[!1]", "web3"));
        assert!(glob_match("w[a-c]", "wb"));
        assert!(glob_match("*", ""));
        assert!(glob_match("*운영*", "서울운영1"));
        assert!(!glob_match("stg-*", "prod"));
    }

    #[test]
    fn load_with_relative_include_and_glob() {
        let d = tempfile::tempdir().unwrap();
        let ssh = d.path();
        fs::create_dir(ssh.join("config.d")).unwrap();
        fs::write(
            ssh.join("config"),
            "Include config.d/*\nHost main\n  User m\n",
        )
        .unwrap();
        fs::write(ssh.join("config.d/a.conf"), "Host inc-a\n  Port 10\n").unwrap();
        fs::write(ssh.join("config.d/b.conf"), "Host inc-b\n").unwrap();
        let (h, w) = load_ssh_config(&ssh.join("config"));
        assert!(w.is_empty(), "{w:?}");
        let mut names: Vec<_> = h.iter().map(|h| h.alias.clone()).collect();
        names.sort();
        assert_eq!(names, ["inc-a", "inc-b", "main"]);
        let a = h.iter().find(|h| h.alias == "inc-a").unwrap();
        assert_eq!(a.port, Some(10));
        assert!(a.source.ends_with("config.d/a.conf"));
    }

    #[test]
    fn load_missing_file_is_empty() {
        let d = tempfile::tempdir().unwrap();
        let (h, _) = load_ssh_config(&d.path().join("nope"));
        assert!(h.is_empty());
    }

    #[test]
    fn include_cycle_terminates() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path();
        fs::write(p.join("config"), "Host a\nInclude other\n").unwrap();
        fs::write(p.join("other"), "Host b\nInclude config\n").unwrap();
        let (h, w) = load_ssh_config(&p.join("config"));
        let mut n: Vec<_> = h.iter().map(|h| h.alias.as_str()).collect();
        n.sort();
        assert_eq!(n, ["a", "b"]);
        assert!(!w.is_empty());
    }

    #[test]
    fn include_depth_limited() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path();
        for i in 0..12 {
            fs::write(
                p.join(format!("f{i}")),
                format!("Host h{i}\nInclude f{}\n", i + 1),
            )
            .unwrap();
        }
        let (h, w) = load_ssh_config(&p.join("f0"));
        assert!(h.len() <= MAX_INCLUDE_DEPTH + 1, "{}", h.len());
        assert!(h.len() >= 8);
        assert!(w.iter().any(|w| w.contains("depth")), "{w:?}");
    }

    fn ssh(alias: &str) -> SshHost {
        SshHost {
            alias: alias.into(),
            hostname: None,
            user: None,
            port: None,
            source: PathBuf::new(),
        }
    }

    fn entry(
        name: Option<&str>,
        host: Option<&str>,
        m: Option<&str>,
        theme: Option<&str>,
    ) -> ServerConfig {
        ServerConfig {
            name: name.map(Into::into),
            host: host.map(Into::into),
            match_glob: m.map(Into::into),
            theme: theme.map(Into::into),
            ..Default::default()
        }
    }

    #[test]
    fn merge_ssh_only_sorted() {
        let (s, w) = merge(&[ssh("zeta"), ssh("alpha")], &[]);
        assert!(w.is_empty());
        assert_eq!(
            s.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["alpha", "zeta"]
        );
        assert!(s.iter().all(|s| s.from_ssh_config && s.host == s.name));
    }

    #[test]
    fn merge_override_existing_alias() {
        let mut c = entry(Some("alpha"), Some("alpha"), None, Some("red"));
        c.port = Some(2200);
        c.path = Some("/var/www".into());
        let (s, _) = merge(&[ssh("alpha")], &[c]);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].port, Some(2200));
        assert_eq!(s[0].start_path.as_deref(), Some("/var/www"));
        assert_eq!(s[0].theme.as_deref(), Some("red"));
        assert!(s[0].from_ssh_config);
    }

    #[test]
    fn merge_explicit_host_beats_same_named_alias() {
        let (s, _) = merge(
            &[ssh("alpha")],
            &[entry(Some("alpha"), Some("me@9.9.9.9"), None, None)],
        );
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].name, "alpha");
        assert_eq!(s[0].host, "me@9.9.9.9");
        assert!(!s[0].from_ssh_config);
    }

    #[test]
    fn merge_adds_new_server() {
        let (s, _) = merge(&[], &[entry(Some("n"), Some("me@1.2.3.4"), None, None)]);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].host, "me@1.2.3.4");
        assert!(!s[0].from_ssh_config);
    }

    #[test]
    fn merge_match_only_is_not_server() {
        let (s, w) = merge(&[ssh("a")], &[entry(None, None, Some("a*"), Some("t"))]);
        assert_eq!(s.len(), 1);
        assert!(w.is_empty());
    }

    #[test]
    fn merge_warns_without_host_and_match() {
        let (s, w) = merge(&[], &[entry(Some("lonely"), None, None, Some("t"))]);
        assert!(s.is_empty());
        assert_eq!(w.len(), 1);
    }

    #[test]
    fn merge_order_config_first_then_ssh_alpha() {
        let cfg = [
            entry(Some("zz-cfg"), Some("h1"), None, None),
            entry(Some("beta"), Some("beta"), None, Some("x")),
            entry(Some("aa-cfg"), Some("h2"), None, None),
        ];
        let (s, _) = merge(&[ssh("gamma"), ssh("beta"), ssh("alpha")], &cfg);
        let n: Vec<_> = s.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(n, ["zz-cfg", "beta", "aa-cfg", "alpha", "gamma"]);
    }

    #[test]
    fn merge_dedups_ssh_aliases() {
        let (s, _) = merge(&[ssh("a"), ssh("a")], &[]);
        assert_eq!(s.len(), 1);
    }

    fn server(name: &str) -> Server {
        Server {
            name: name.into(),
            host: name.into(),
            port: None,
            start_path: None,
            theme: None,
            from_ssh_config: true,
        }
    }

    #[test]
    fn theme_name_beats_match_beats_global() {
        let cfg = [
            entry(None, None, Some("stg-*"), Some("green")),
            entry(Some("stg-web"), Some("stg-web"), None, Some("red")),
        ];
        assert_eq!(theme_for(&server("stg-web"), &cfg, "g"), "red");
        assert_eq!(theme_for(&server("stg-db"), &cfg, "g"), "green");
        assert_eq!(theme_for(&server("prod"), &cfg, "g"), "g");
    }

    #[test]
    fn theme_first_match_wins_and_case_insensitive() {
        let cfg = [
            entry(None, None, Some("STG-*"), Some("one")),
            entry(None, None, Some("stg-*"), Some("two")),
        ];
        assert_eq!(theme_for(&server("stg-a"), &cfg, "g"), "one");
    }

    #[test]
    fn theme_name_entry_without_theme_falls_through() {
        let cfg = [
            entry(Some("a"), Some("a"), None, None),
            entry(None, None, Some("a"), Some("m")),
        ];
        assert_eq!(theme_for(&server("a"), &cfg, "g"), "m");
    }

    #[test]
    fn server_config_toml() {
        #[derive(Deserialize)]
        struct W {
            servers: Vec<ServerConfig>,
        }
        let w: W = toml::from_str(
            "[[servers]]\nname=\"p\"\nhost=\"p\"\nport=22\n[[servers]]\nmatch=\"stg-*\"\ntheme=\"g\"\n",
        )
        .unwrap();
        assert_eq!(w.servers[1].match_glob.as_deref(), Some("stg-*"));
        assert_eq!(w.servers[0].port, Some(22));
    }
}
