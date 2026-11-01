//! `tendr guide [TOPIC]` — the self-documenting usage guide.
//!
//! The guide's single source of truth is `docs/guide.md`, embedded at build
//! time. `tendr guide` prints the whole document; `tendr guide <topic>` slices
//! out one section by mapping the topic to a heading and returning everything
//! from that heading down to the next same-or-higher-level heading. The
//! Markdown holds all the content; this module holds only the topic index and
//! the slicer.

/// The usage guide, embedded from the repo's source of truth.
const GUIDE: &str = include_str!("../../docs/guide.md");

/// Topic keyword → heading needle. The needle is matched case-insensitively as a
/// substring of a heading's text; the printed section runs from the first
/// matching heading to the next heading of the same or higher level. Each needle
/// is unique across the guide's headings (guarded by a unit test), so the
/// mapping is unambiguous.
const TOPICS: &[(&str, &str)] = &[
    ("install", "Install and build"),
    ("exec", "Drive it with"),
    ("remote", "Reach remote hosts"),
    ("python", "Python /"),
    ("duckdb", "DuckDB"),
    ("powershell", "PowerShell"),
    ("ndjson", "NDJSON"),
    ("boundary", "--boundary"),
    ("cgroup", "Cap a session's memory"),
    ("herdr", "inside herdr"),
];

pub fn cmd_guide(topic: Option<&str>) -> anyhow::Result<()> {
    match topic {
        None => {
            print_all();
            Ok(())
        }
        Some(topic) => print_topic(topic),
    }
}

/// Print the entire guide, ensuring a trailing newline so the shell prompt lands
/// cleanly even if the source file were to lack one.
fn print_all() {
    out!("{GUIDE}");
    if !GUIDE.ends_with('\n') {
        outln!();
    }
}

/// Print the section for a single topic. An unknown topic is a usage error
/// (exit 2) that lists the available topics on stderr.
fn print_topic(topic: &str) -> anyhow::Result<()> {
    let key = topic.to_lowercase();
    let Some((_, needle)) = TOPICS.iter().find(|(name, _)| *name == key) else {
        eprintln!("unknown guide topic: {topic}");
        eprintln!("available topics: {}", available_topics());
        std::process::exit(2);
    };

    // A registered needle that no longer matches a heading is a build-time drift
    // bug, not user error — the unit test below exists to prevent it, but fail
    // loudly rather than print nothing if it ever slips through.
    let section = slice_section(GUIDE, needle).ok_or_else(|| {
        anyhow::anyhow!("internal: guide topic '{topic}' has no matching section")
    })?;
    outln!("{section}");
    Ok(())
}

/// Comma-separated list of the available topic keywords, in registry order.
fn available_topics() -> String {
    TOPICS
        .iter()
        .map(|(name, _)| *name)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Parse an ATX Markdown heading line into `(level, text)`. A heading is a line
/// with 0–3 leading spaces, then 1–6 `#` characters, then a space. This rejects
/// `#` glued to text (a shell comment like `#foo`) and 4-space-indented lines
/// (Markdown code blocks), so `#` characters inside code fences never slice.
fn parse_heading(line: &str) -> Option<(usize, &str)> {
    let indent = line.bytes().take_while(|&b| b == b' ').count();
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    let level = rest.bytes().take_while(|&b| b == b'#').count();
    if level == 0 || level > 6 {
        return None;
    }
    let after = &rest[level..];
    if !after.starts_with(' ') {
        return None;
    }
    Some((level, after.trim()))
}

/// Slice the section beginning at the first heading whose text contains `needle`
/// (case-insensitive), running to the next heading of the same or higher level
/// (or end of document). Returns `None` when no heading matches.
fn slice_section(guide: &str, needle: &str) -> Option<String> {
    let needle = needle.to_lowercase();
    let lines: Vec<&str> = guide.lines().collect();

    let (start, level) = lines.iter().enumerate().find_map(|(i, line)| {
        let (level, text) = parse_heading(line)?;
        text.to_lowercase().contains(&needle).then_some((i, level))
    })?;

    let end = lines
        .iter()
        .enumerate()
        .skip(start + 1)
        .find(|(_, line)| parse_heading(line).is_some_and(|(lvl, _)| lvl <= level))
        .map_or(lines.len(), |(i, _)| i);

    Some(lines[start..end].join("\n").trim_end().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_topic_slices_a_nonempty_section() {
        for (topic, needle) in TOPICS {
            let section = slice_section(GUIDE, needle).unwrap_or_else(|| {
                panic!("topic '{topic}' (needle '{needle}') matched no heading")
            });
            assert!(
                !section.trim().is_empty(),
                "topic '{topic}' sliced an empty section"
            );
            // The section starts at its own heading.
            assert!(
                parse_heading(section.lines().next().unwrap()).is_some(),
                "topic '{topic}' section does not start at a heading"
            );
        }
    }

    #[test]
    fn each_needle_matches_exactly_one_heading() {
        for (topic, needle) in TOPICS {
            let needle_lc = needle.to_lowercase();
            let matches = GUIDE
                .lines()
                .filter_map(parse_heading)
                .filter(|(_, text)| text.to_lowercase().contains(&needle_lc))
                .count();
            assert_eq!(
                matches, 1,
                "topic '{topic}' needle '{needle}' must match exactly one heading, matched {matches}"
            );
        }
    }

    #[test]
    fn exec_section_excludes_neighbouring_sections() {
        let exec = slice_section(GUIDE, "Drive it with").unwrap();
        assert!(exec.contains("takes argv, not a shell snippet"));
        assert!(exec.contains("exec_truncated"));
        assert!(
            !exec.contains("Reach remote hosts"),
            "exec section must not bleed into the remote section"
        );
        assert!(
            !exec.contains("The REPL and database lanes"),
            "exec section stops before the REPL lanes"
        );
    }

    #[test]
    fn remote_section_includes_frame_subsection() {
        let remote = slice_section(GUIDE, "Reach remote hosts").unwrap();
        // The nested `### Scripting: exec --frame-from-stdin` rides along because
        // slicing stops only at the next same-or-higher-level (`##`) heading.
        assert!(remote.contains("frame-from-stdin"));
        assert!(remote.contains("default ssh shell"));
    }

    #[test]
    fn install_section_mentions_crate_and_binary_names() {
        let install = slice_section(GUIDE, "Install and build").unwrap();
        assert!(install.contains("tendr"));
        assert!(install.contains("cargo install tendr"));
        assert!(install.contains("cargo-zigbuild"));
    }

    #[test]
    fn duckdb_and_python_sections_are_distinct() {
        let duckdb = slice_section(GUIDE, "DuckDB").unwrap();
        let python = slice_section(GUIDE, "Python /").unwrap();
        assert!(duckdb.contains("duckdb :memory:"));
        assert!(!duckdb.contains("persist between calls"));
        assert!(python.contains("persist between calls"));
    }

    #[test]
    fn cgroup_section_covers_linux_and_cross_platform() {
        let cg = slice_section(GUIDE, "Cap a session's memory").unwrap();
        // Linux cgroup recipe.
        assert!(cg.contains("tendr.slice"));
        assert!(cg.contains("systemd-run --user"));
        assert!(cg.contains("enable-linger"));
        // The nested `### Windows and macOS` subsection rides along (slicing
        // stops only at the next same-or-higher-level `##` heading).
        assert!(cg.contains("Job Object"));
        assert!(cg.contains("macOS"));
        assert!(
            !cg.contains("Record where a session runs"),
            "cgroup section must not bleed into the boundary section"
        );
    }

    #[test]
    fn herdr_section_covers_survival_hook_and_limits() {
        let herdr = slice_section(GUIDE, "inside herdr").unwrap();
        assert!(herdr.contains("herdr session stop"));
        assert!(herdr.contains("HERDR_BIN_PATH"));
        assert!(herdr.contains("--on-exit"));
        assert!(herdr.contains("herdr pane read"));
        assert!(
            !herdr.contains("## See also"),
            "herdr section must not bleed into See also"
        );
    }

    #[test]
    fn parse_heading_rejects_non_headings() {
        assert_eq!(parse_heading("## The model"), Some((2, "The model")));
        assert_eq!(parse_heading("### DuckDB"), Some((3, "DuckDB")));
        assert_eq!(parse_heading("#comment-glued"), None); // no space after #
        assert_eq!(parse_heading("not a heading"), None);
        assert_eq!(parse_heading("    # four-space code block"), None); // indented
    }
}
