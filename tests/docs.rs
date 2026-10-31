//! Documentation checks: drift fails the build instead of waiting for a reader.
//!
//! - Every `tendr` command, long flag and guide topic shown in a document that
//!   describes current behavior exists in the binary.
//! - `docs/plans/` holds only open plans, named and described by the rules in
//!   `docs/plans/README.md`. The folder is the index: the `NN_` prefix sets the
//!   queue order, and each plan's status line says what is left.
//!
//! These tests read the repository's documents. The Nix package's source set
//! leaves them out, so `flake.nix` skips tests whose names start with `docs_`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(rel: &str) -> String {
    fs::read_to_string(repo().join(rel)).unwrap_or_else(|e| panic!("read {rel}: {e}"))
}

/// Documents that describe what the current binary does. Plans are left out:
/// they name commands that do not exist yet.
fn current_docs() -> Vec<String> {
    let mut docs: Vec<String> = [
        "README.md",
        "docs/guide.md",
        "docs/analytics-recipes.md",
        "docs/design.md",
        "src/embedded/SKILL.md",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    for entry in fs::read_dir(repo().join("docs/architecture")).unwrap() {
        let name = entry.unwrap().file_name().into_string().unwrap();
        if name.ends_with(".md") {
            docs.push(format!("docs/architecture/{name}"));
        }
    }
    docs.sort();
    docs
}

const SHELL_FENCES: &[&str] = &["bash", "sh", "shell", "console", "zsh"];

/// Shell snippets in a Markdown document, with their 1-based line numbers:
/// each line of a shell code block (continuation lines joined, comments
/// removed), and each inline code span outside code blocks.
fn snippets(text: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut fence: Option<String> = None;
    let mut pending: Option<(usize, String)> = None;
    for (i, line) in text.lines().enumerate() {
        let n = i + 1;
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            fence = match fence {
                Some(_) => None,
                None => Some(trimmed.trim_start_matches('`').trim().to_string()),
            };
            continue;
        }
        match &fence {
            Some(lang) if SHELL_FENCES.contains(&lang.as_str()) => {
                let code = match line.find(" #") {
                    Some(at) => &line[..at],
                    None if trimmed.starts_with('#') => "",
                    None => line,
                };
                let (start, mut joined) = pending.take().unwrap_or((n, String::new()));
                let code = code.trim_end();
                if let Some(head) = code.strip_suffix('\\') {
                    joined.push_str(head);
                    joined.push(' ');
                    pending = Some((start, joined));
                } else {
                    joined.push_str(code);
                    out.push((start, joined));
                }
            }
            Some(_) => {}
            None => {
                for (k, span) in line.split('`').enumerate() {
                    if k % 2 == 1 {
                        out.push((n, span.to_string()));
                    }
                }
            }
        }
    }
    out
}

/// The words of each `tendr` invocation in a snippet, up to the end of its
/// arguments (`--`, a pipe, `;`, `&&`, `||` or a closing parenthesis).
/// Square brackets of a synopsis such as `[--from-now | --since <ts>]` are
/// dropped, and a `|` inside them separates alternatives instead of ending
/// the invocation.
fn invocations(snippet: &str) -> Vec<Vec<String>> {
    let tokens: Vec<&str> = snippet.split_whitespace().collect();
    let mut out = Vec::new();
    for (i, tok) in tokens.iter().enumerate() {
        if tok.trim_start_matches("$(").trim_start_matches('(') != "tendr" {
            continue;
        }
        let mut words = Vec::new();
        let mut depth = 0usize;
        for t in &tokens[i + 1..] {
            let opens = t.chars().take_while(|c| *c == '[').count();
            depth += opens;
            let t = &t[opens..];
            if depth > 0 && t == "|" {
                continue;
            }
            if matches!(t, "--" | "|" | "||" | "&&" | ";" | ">" | "2>&1") || t.starts_with('#') {
                break;
            }
            let closes = t.chars().rev().take_while(|c| *c == ']').count();
            depth = depth.saturating_sub(closes);
            let t = &t[..t.len() - closes];
            let end = t.ends_with(')') || t.ends_with(';');
            words.push(t.trim_end_matches([')', ';']).to_string());
            if end {
                break;
            }
        }
        out.push(words);
    }
    out
}

fn is_word(s: &str) -> bool {
    s.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && s.chars().all(|c| c.is_ascii_lowercase() || c == '-')
}

/// `tendr <args> --help`, cached.
struct Help(BTreeMap<Vec<String>, Option<String>>);

impl Help {
    fn get(&mut self, args: &[&str]) -> Option<String> {
        let key: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        self.0
            .entry(key)
            .or_insert_with(|| {
                let out = Command::cargo_bin("tendr")
                    .unwrap()
                    .args(args)
                    .arg("--help")
                    .output()
                    .unwrap();
                out.status
                    .success()
                    .then(|| String::from_utf8(out.stdout).unwrap())
            })
            .clone()
    }
}

/// Names listed under "Commands:" in a help text.
fn commands(help: &str) -> BTreeSet<String> {
    help.lines()
        .skip_while(|l| !l.starts_with("Commands:"))
        .skip(1)
        .take_while(|l| !l.trim().is_empty())
        .filter_map(|l| l.split_whitespace().next().map(str::to_string))
        .collect()
}

/// Whether `tendr <path> <flag>` parses. Asking for help after the flag makes
/// clap reject an unknown flag without running anything, and hidden flags
/// (which `--help` does not list) still count.
fn accepts_flag(path: &[&str], flag: &str) -> bool {
    let out = Command::cargo_bin("tendr")
        .unwrap()
        .args(path)
        .args([flag, "--help"])
        .output()
        .unwrap();
    !String::from_utf8_lossy(&out.stderr).contains("unexpected argument")
}

/// Whether `flag` appears in a help text as a whole flag name.
fn mentions_flag(help: &str, flag: &str) -> bool {
    help.match_indices(flag).any(|(at, _)| {
        let next = help[at + flag.len()..].chars().next();
        !next.is_some_and(|c| c.is_ascii_alphanumeric() || c == '-')
    })
}

/// Directory entries, without hidden files such as `.DS_Store`.
fn entries(dir: &Path) -> Vec<String> {
    fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|name| !name.starts_with('.'))
        .collect()
}

fn guide_topic_exists(topic: &str) -> bool {
    Command::cargo_bin("tendr")
        .unwrap()
        .args(["guide", topic])
        .output()
        .unwrap()
        .status
        .success()
}

#[test]
fn docs_show_only_commands_flags_and_topics_that_exist() {
    let mut help = Help(BTreeMap::new());
    let top = help.get(&[]).expect("tendr --help");
    let subcommands = commands(&top);
    let mut topics: BTreeMap<String, bool> = BTreeMap::new();
    let mut errors = Vec::new();

    // Every long flag any command lists, for flags a document names on their
    // own, as in a table of `start` options.
    let mut all_help = top.clone();
    for sub in &subcommands {
        let text = help.get(&[sub]).unwrap_or_default();
        for subsub in commands(&text) {
            all_help.push_str(&help.get(&[sub, &subsub]).unwrap_or_default());
        }
        all_help.push_str(&text);
    }

    for doc in current_docs() {
        for (line, snippet) in snippets(&read(&doc)) {
            if let Some(flag) = snippet
                .split_whitespace()
                .next()
                .filter(|w| w.starts_with("--") && w.len() > 2)
            {
                let name = flag.split('=').next().unwrap();
                if !mentions_flag(&all_help, name) && !accepts_flag(&[], name) {
                    errors.push(format!(
                        "{doc}:{line}: `{snippet}`: no command has the flag {name}"
                    ));
                }
            }
            for words in invocations(&snippet) {
                let at = format!("{doc}:{line}: `tendr {}`", words.join(" "));
                let mut rest = words.iter().map(String::as_str).peekable();
                while let Some(flag) = rest.next_if(|w| w.starts_with('-')) {
                    let name = flag.split('=').next().unwrap();
                    if !accepts_flag(&[], name) {
                        errors.push(format!("{at}: no global flag {name}"));
                    }
                    if name == "--host" && !flag.contains('=') {
                        rest.next();
                    }
                }
                let Some(sub) = rest.next() else { continue };
                if !is_word(sub) {
                    continue; // a placeholder such as <command> or …
                }
                if !subcommands.contains(sub) {
                    errors.push(format!("{at}: no command {sub}"));
                    continue;
                }
                let mut path = vec![sub];
                if let Some(&next) = rest.peek() {
                    let text = help.get(&[sub]).unwrap_or_default();
                    if commands(&text).contains(next) {
                        path.push(next);
                    } else if sub == "guide" && is_word(next) {
                        let ok = *topics
                            .entry(next.to_string())
                            .or_insert_with(|| guide_topic_exists(next));
                        if !ok {
                            errors.push(format!("{at}: no guide topic {next}"));
                        }
                    }
                }
                for word in rest.filter(|w| w.starts_with("--") && w.len() > 2) {
                    let name = word.split('=').next().unwrap();
                    if !accepts_flag(&path, name) {
                        errors.push(format!(
                            "{at}: `tendr {}` has no flag {name}",
                            path.join(" ")
                        ));
                    }
                }
            }
        }
    }
    assert!(
        errors.is_empty(),
        "documents show things the binary does not have:\n{}",
        errors.join("\n")
    );
}

struct Plan {
    slug: String,
    path: String,
}

fn plans(dir: &str) -> Vec<Plan> {
    let mut plans: Vec<Plan> = entries(&repo().join("docs/plans").join(dir))
        .into_iter()
        .map(|name| Plan {
            slug: name.trim_end_matches(".md").to_string(),
            path: format!("plans/{dir}/{name}"),
        })
        .collect();
    plans.sort_by(|a, b| a.path.cmp(&b.path));
    plans
}

fn is_slug(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

struct FrontMatter {
    id: Option<String>,
    depends_on: Vec<String>,
    /// Keys other than `id` and `depends_on`.
    other: Vec<String>,
}

fn front_matter(path: &Path) -> FrontMatter {
    let text = fs::read_to_string(path).unwrap();
    let mut fm = FrontMatter {
        id: None,
        depends_on: Vec::new(),
        other: Vec::new(),
    };
    let mut lines = text.lines();
    if lines.next() != Some("---") {
        return fm;
    }
    let mut in_deps = false;
    for line in lines.take_while(|l| *l != "---") {
        if let Some(v) = line.strip_prefix("id:") {
            fm.id = Some(v.trim().to_string());
            in_deps = false;
        } else if let Some(v) = line.strip_prefix("depends_on:") {
            let v = v.trim().trim_start_matches('[').trim_end_matches(']');
            fm.depends_on.extend(
                v.split(',')
                    .map(str::trim)
                    .filter(|d| !d.is_empty())
                    .map(str::to_string),
            );
            in_deps = true;
        } else if let Some(v) = line.trim_start().strip_prefix("- ").filter(|_| in_deps) {
            fm.depends_on.push(v.trim().to_string());
        } else if let Some((key, _)) = line.split_once(':').filter(|_| !line.starts_with(' ')) {
            fm.other.push(key.to_string());
            in_deps = false;
        }
    }
    fm
}

#[test]
fn docs_plans_folder_holds_only_open_plans() {
    let entries: BTreeSet<String> = entries(&repo().join("docs/plans")).into_iter().collect();
    let expected: BTreeSet<String> = ["README.md", "active", "backlog"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert_eq!(
        entries, expected,
        "docs/plans/ may hold only README.md, active/ and backlog/"
    );

    let mut errors = Vec::new();
    let mut ids = BTreeSet::new();
    let mut numbers = BTreeSet::new();
    let mut all = Vec::new();
    for mut plan in plans("active") {
        match plan.slug.split_once('_') {
            Some((n, slug))
                if n.len() == 2 && n.chars().all(|c| c.is_ascii_digit()) && is_slug(slug) =>
            {
                if !numbers.insert(n.to_string()) {
                    errors.push(format!("{}: queue number {n} is used twice", plan.path));
                }
                plan.slug = slug.to_string();
            }
            _ => errors.push(format!("{}: active plans are named NN_slug.md", plan.path)),
        }
        all.push(plan);
    }
    for plan in plans("backlog") {
        if !is_slug(&plan.slug) {
            errors.push(format!("{}: backlog plans are named slug.md", plan.path));
        }
        all.push(plan);
    }
    for plan in &all {
        ids.insert(plan.slug.clone());
    }
    for plan in &all {
        let fm = front_matter(&repo().join("docs").join(&plan.path));
        let text = read(&format!("docs/{}", plan.path));
        let after_title = text
            .lines()
            .skip_while(|l| !l.starts_with("# "))
            .skip(1)
            .find(|l| !l.trim().is_empty());
        if !after_title.is_some_and(|l| l.starts_with("**Status:**")) {
            errors.push(format!(
                "{}: the line after the title must be the `**Status:**` line",
                plan.path
            ));
        }
        if fm.id.as_deref() != Some(plan.slug.as_str()) {
            errors.push(format!(
                "{}: front matter id must be `{}`, found {:?}",
                plan.path, plan.slug, fm.id
            ));
        }
        for key in &fm.other {
            errors.push(format!(
                "{}: front matter holds only id and depends_on, found `{key}`",
                plan.path
            ));
        }
        for dep in fm.depends_on.iter().filter(|d| !ids.contains(*d)) {
            errors.push(format!(
                "{}: depends_on names `{dep}`, which is not an open plan",
                plan.path
            ));
        }
    }
    assert!(errors.is_empty(), "{}", errors.join("\n"));
}
