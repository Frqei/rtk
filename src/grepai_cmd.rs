use crate::tracking;
use crate::utils::strip_ansi;
use anyhow::{Context, Result};
use lazy_static::lazy_static;
use regex::Regex;
use std::collections::HashMap;
use std::process::Command;

lazy_static! {
    // filter_search regexes
    static ref SEARCH_HEADER: Regex =
        Regex::new(r#"^Found (\d+) results? for: "(.+)"$"#).unwrap();
    static ref SEARCH_SEPARATOR: Regex =
        Regex::new(r"^[─]+ Result \d+ \(score: ([\d.]+)\) [─]+$").unwrap();
    static ref SEARCH_FILE_LINE: Regex = Regex::new(r"^File: (.+):(\d+)-(\d+)$").unwrap();
    static ref SEARCH_MORE_LINES: Regex = Regex::new(r"\.\.\. \((\d+) more lines\)").unwrap();

    // filter_trace regexes
    static ref TRACE_SYMBOL: Regex = Regex::new(r"^Symbol: (.+) \((\w+)\)$").unwrap();
    static ref TRACE_SYMBOL_FILE: Regex = Regex::new(r"^File: (.+?)(?::(\d+))?$").unwrap();
    static ref TRACE_CALLER_COUNT: Regex = Regex::new(r"^(Callers|Callees) \((\d+)\):$").unwrap();
    static ref TRACE_CALLS_AT: Regex = Regex::new(r"^\s*Calls at: (.+):(\d+)$").unwrap();
}

struct SearchResult {
    score: String,
    file_path: String,
    line_start: String,
    line_end: String,
    more_lines: Option<u32>,
}

pub fn run(args: &[String], verbose: u8) -> Result<()> {
    let timer = tracking::TimedExecution::start();

    let mut cmd = Command::new("grepai");
    for arg in args {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!("Running: grepai {}", args.join(" "));
    }

    let output = cmd
        .output()
        .context("Failed to run grepai. Is it installed? Try: pip install grepai")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let raw = format!("{}\n{}", stdout, stderr);
    let clean = strip_ansi(&raw);

    let subcommand = args.first().map(|s| s.as_str()).unwrap_or("");

    let filtered = match subcommand {
        "search" => filter_search(&clean),
        "trace" => {
            let trace_sub = args.get(1).map(|s| s.as_str()).unwrap_or("");
            match trace_sub {
                "callers" | "callees" => filter_trace(&clean),
                _ => clean.trim().to_string(),
            }
        }
        _ => clean.trim().to_string(),
    };

    let exit_code = output
        .status
        .code()
        .unwrap_or(if output.status.success() { 0 } else { 1 });
    if let Some(hint) = crate::tee::tee_and_hint(&raw, "grepai", exit_code) {
        println!("{}\n{}", filtered, hint);
    } else {
        println!("{}", filtered);
    }

    timer.track(
        &format!("grepai {}", args.join(" ")),
        &format!("rtk grepai {}", args.join(" ")),
        &raw,
        &filtered,
    );

    if !output.status.success() {
        std::process::exit(exit_code);
    }

    Ok(())
}

/// Filter grepai search output: strip code bodies, keep one line per result.
///
/// Input format (default grepai search output):
/// ```text
/// Found 10 results for: "authentication logic"
/// ────────────────────── Result 1 (score: 0.68) ──────────────────────
/// File: backend/src/some/path/api/auth.py:223-228
///
///     def authenticate_user(credentials):
///         """Authenticate user with the given credentials."""
///         ...
///
/// ────────────────────── Result 2 (score: 0.38) ──────────────────────
/// File: .claude/skills/auth-flow/SKILL.md:330-394
///
///     ... (51 more lines)
/// ```
///
/// Output format:
/// ```text
/// grepai: "authentication logic" — 10 results
///
///   0.68  auth.py:223-228              (backend/src/.../api/)
///   0.38  SKILL.md:330-394 (+51 lines) (.claude/skills/auth-flow/)
/// ```
pub fn filter_search(output: &str) -> String {
    let lines: Vec<&str> = output.lines().collect();

    // Parse header from first line (always line 0 in grepai output)
    let (query, total_results) = match lines.first().and_then(|l| SEARCH_HEADER.captures(l)) {
        Some(caps) => (caps[2].to_string(), caps[1].parse().unwrap_or(0u32)),
        None => return output.trim().to_string(),
    };

    // Single-pass: parse results from line 1 onward
    let mut results: Vec<SearchResult> = Vec::new();
    let mut current: Option<SearchResult> = None;

    for line in &lines[1..] {
        if let Some(caps) = SEARCH_SEPARATOR.captures(line) {
            // Flush previous result
            if let Some(result) = current.take() {
                results.push(result);
            }
            current = Some(SearchResult {
                score: caps[1].to_string(),
                file_path: String::new(),
                line_start: String::new(),
                line_end: String::new(),
                more_lines: None,
            });
        } else if let Some(ref mut r) = current {
            if r.file_path.is_empty() {
                if let Some(caps) = SEARCH_FILE_LINE.captures(line) {
                    r.file_path = caps[1].to_string();
                    r.line_start = caps[2].to_string();
                    r.line_end = caps[3].to_string();
                }
            }
            if let Some(caps) = SEARCH_MORE_LINES.captures(line) {
                r.more_lines = Some(caps[1].parse().unwrap_or(0));
            }
        }
    }
    // Flush last result
    if let Some(result) = current.take() {
        results.push(result);
    }

    if results.is_empty() {
        return format!("grepai: \"{}\" — 0 results", query);
    }

    // Build output
    let mut out = format!("grepai: \"{}\" — {} results\n\n", query, total_results);

    for r in &results {
        let (filename, parent) = split_path(&r.file_path);
        let range = format!("{}:{}-{}", filename, r.line_start, r.line_end);

        let more = match r.more_lines {
            Some(n) if n > 0 => format!(" (+{} lines)", n),
            _ => String::new(),
        };

        let parent_display = if parent.is_empty() {
            String::new()
        } else {
            format!(" ({})", compact_parent(&parent))
        };

        out.push_str(&format!(
            "  {}  {}{}{}\n",
            r.score, range, more, parent_display
        ));
    }

    out.trim_end().to_string()
}

/// Filter grepai trace callers/callees output: group by file, deduplicate.
///
/// Input format:
/// ```text
/// Symbol: login (function)
/// File: backend/src/auth.py:117
///
/// Callers (27):
///
///   Caller 1:
///     Function: test_login_success (function)
///     Calls at: tests/authRepository.test.ts:61
///     ... code ...
///
///   Caller 2:
///     Function: test_login_failure (function)
///     Calls at: tests/authRepository.test.ts:96
///     ... code ...
/// ```
///
/// Output format:
/// ```text
/// login (function) @ auth.py:117 — 27 callers in 8 files:
///
///   authRepository.test.ts  18x (lines: 61,96,126,...)
///   test_exercise_library.py  2x (line: 120)
///   conftest.py:598  1x
/// ```
pub fn filter_trace(output: &str) -> String {
    let lines: Vec<&str> = output.lines().collect();

    // Parse header positionally (fixed structure: Symbol, File, blank, Callers/Callees)
    let mut symbol_name = String::new();
    let mut symbol_type = String::new();
    let mut symbol_file = String::new();
    let mut symbol_line = String::new();
    let mut relation = String::new();
    let mut total_count = 0u32;
    let mut header_end = 0;

    for (i, line) in lines.iter().enumerate().take(6) {
        if let Some(caps) = TRACE_SYMBOL.captures(line) {
            symbol_name = caps[1].to_string();
            symbol_type = caps[2].to_string();
        } else if symbol_file.is_empty() {
            if let Some(caps) = TRACE_SYMBOL_FILE.captures(line) {
                symbol_file = caps[1].to_string();
                symbol_line = caps
                    .get(2)
                    .map(|m| m.as_str().to_string())
                    .unwrap_or_default();
            }
        }
        if let Some(caps) = TRACE_CALLER_COUNT.captures(line) {
            relation = caps[1].to_lowercase();
            total_count = caps[2].parse().unwrap_or(0);
            header_end = i + 1;
            break;
        }
    }

    if symbol_name.is_empty() {
        return output.trim().to_string();
    }

    // Parse call sites from after the header only
    let mut by_file: HashMap<String, Vec<usize>> = HashMap::new();
    for line in lines.iter().skip(header_end) {
        if let Some(caps) = TRACE_CALLS_AT.captures(line) {
            let file = caps[1].to_string();
            let line_num: usize = caps[2].parse().unwrap_or(0);
            by_file.entry(file).or_default().push(line_num);
        }
    }

    // Sort files by call count (descending)
    let mut file_entries: Vec<_> = by_file.into_iter().collect();
    file_entries.sort_by(|a, b| b.1.len().cmp(&a.1.len()));

    let num_files = file_entries.len();

    // Build header
    let (sym_filename, _) = split_path(&symbol_file);
    let sym_loc = if symbol_line.is_empty() {
        sym_filename.to_string()
    } else {
        format!("{}:{}", sym_filename, symbol_line)
    };

    let mut out = format!(
        "{} ({}) @ {} — {} {} in {} files:\n\n",
        symbol_name, symbol_type, sym_loc, total_count, relation, num_files
    );

    for (file, mut line_nums) in file_entries {
        line_nums.sort();
        line_nums.dedup();
        let count = line_nums.len();
        let (filename, _) = split_path(&file);

        if count == 1 {
            out.push_str(&format!("  {}:{}  1x\n", filename, line_nums[0]));
        } else {
            let lines_str = if line_nums.len() <= 5 {
                line_nums
                    .iter()
                    .map(|n| n.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            } else {
                let first_four: Vec<_> = line_nums[..4].iter().map(|n| n.to_string()).collect();
                format!("{},...", first_four.join(","))
            };
            out.push_str(&format!(
                "  {}  {}x (lines: {})\n",
                filename, count, lines_str
            ));
        }
    }

    out.trim_end().to_string()
}

/// Split a path into (filename, parent_directory).
fn split_path(path: &str) -> (String, String) {
    let path = path.replace('\\', "/");
    if let Some(pos) = path.rfind('/') {
        (path[pos + 1..].to_string(), path[..pos].to_string())
    } else {
        (path.clone(), String::new())
    }
}

/// Compact a parent directory path for display.
/// e.g. "backend/src/some/deep/path/api" → "backend/src/.../api/"
fn compact_parent(parent: &str) -> String {
    let parts: Vec<&str> = parent.split('/').filter(|s| !s.is_empty()).collect();
    if parts.len() <= 3 {
        format!("{}/", parent)
    } else {
        format!("{}/{}/.../{}/", parts[0], parts[1], parts[parts.len() - 1])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use insta::assert_snapshot;

    fn count_tokens(text: &str) -> usize {
        text.split_whitespace().count()
    }

    // ── filter_search tests ──

    const SEARCH_RAW: &str = r#"Found 3 results for: "authentication logic"
────────────────────── Result 1 (score: 0.68) ──────────────────────
File: backend/src/some/path/api/auth.py:223-228

    def authenticate_user(credentials):
        """Authenticate user with the given credentials."""
        validated = validate_credentials(credentials)
        if not validated:
            raise AuthenticationError("Invalid credentials")
        return create_session(validated)

────────────────────── Result 2 (score: 0.38) ──────────────────────
File: .claude/skills/auth-flow/SKILL.md:330-394

    ## Authentication Flow
    The authentication flow begins when the user submits their credentials.
    The system validates the credentials against the database, checks for
    rate limiting, verifies two-factor authentication if enabled, and
    ... (51 more lines)

────────────────────── Result 3 (score: 0.25) ──────────────────────
File: docs/architecture.md:10-15

    # Authentication Architecture
    The authentication module handles all user authentication
    including OAuth, JWT tokens, and session management.
"#;

    #[test]
    fn test_filter_search_basic() {
        let result = filter_search(SEARCH_RAW);
        assert!(result.contains("grepai: \"authentication logic\" — 3 results"));
        assert!(result.contains("0.68"));
        assert!(result.contains("auth.py:223-228"));
        assert!(result.contains("0.38"));
        assert!(result.contains("SKILL.md:330-394"));
        assert!(result.contains("0.25"));
        assert!(result.contains("architecture.md:10-15"));
        // Code bodies must be stripped
        assert!(!result.contains("def authenticate_user"));
        assert!(!result.contains("validate_credentials"));
        assert!(!result.contains("Authentication Flow"));
    }

    #[test]
    fn test_filter_search_snapshot() {
        let result = filter_search(SEARCH_RAW);
        assert_snapshot!(result);
    }

    #[test]
    fn test_filter_search_more_lines_marker() {
        let result = filter_search(SEARCH_RAW);
        assert!(result.contains("(+51 lines)"));
    }

    #[test]
    fn test_filter_search_parent_dirs() {
        let result = filter_search(SEARCH_RAW);
        // Parent directory should be shown compactly
        assert!(result.contains("(backend/src/.../api/)"));
        assert!(result.contains("(.claude/skills/auth-flow/)"));
    }

    #[test]
    fn test_filter_search_token_savings() {
        let input_tokens = count_tokens(SEARCH_RAW);
        let output = filter_search(SEARCH_RAW);
        let output_tokens = count_tokens(&output);

        let savings = 100.0 - (output_tokens as f64 / input_tokens as f64 * 100.0);
        assert!(
            savings >= 80.0,
            "Search filter: expected >=80% savings, got {:.1}% (in={}, out={})",
            savings,
            input_tokens,
            output_tokens
        );
    }

    #[test]
    fn test_filter_search_empty_results() {
        let input = r#"Found 0 results for: "nonexistent query""#;
        let result = filter_search(input);
        assert!(result.contains("grepai: \"nonexistent query\" — 0 results"));
    }

    #[test]
    fn test_filter_search_malformed_input() {
        let input = "this is not grepai output at all";
        let result = filter_search(input);
        // Should return as-is (fallback)
        assert_eq!(result, input);
    }

    // ── filter_trace tests ──

    const TRACE_RAW: &str = r#"Symbol: login (function)
File: backend/src/auth.py:117

Callers (6):

  Caller 1:
    Function: test_login_success (function)
    Calls at: tests/authRepository.test.ts:61
    Code:
        const result = await login(validCredentials);

  Caller 2:
    Function: test_login_failure (function)
    Calls at: tests/authRepository.test.ts:96
    Code:
        const result = await login(invalidCredentials);

  Caller 3:
    Function: test_login_rate_limit (function)
    Calls at: tests/authRepository.test.ts:126
    Code:
        await login(credentials);

  Caller 4:
    Function: test_exercise_login (function)
    Calls at: tests/test_exercise_library.py:120
    Code:
        login(test_user)

  Caller 5:
    Function: test_exercise_login_again (function)
    Calls at: tests/test_exercise_library.py:145
    Code:
        login(another_user)

  Caller 6:
    Function: setup_session (function)
    Calls at: tests/conftest.py:598
    Code:
        session = login(admin_creds)
"#;

    #[test]
    fn test_filter_trace_basic() {
        let result = filter_trace(TRACE_RAW);
        assert!(result.contains("login (function) @ auth.py:117"));
        assert!(result.contains("6 callers in 3 files"));
        // Code bodies must be stripped
        assert!(!result.contains("const result = await"));
        assert!(!result.contains("login(test_user)"));
        assert!(!result.contains("Code:"));
    }

    #[test]
    fn test_filter_trace_snapshot() {
        let result = filter_trace(TRACE_RAW);
        assert_snapshot!(result);
    }

    #[test]
    fn test_filter_trace_deduplication() {
        let result = filter_trace(TRACE_RAW);
        // authRepository.test.ts has 3 callers → should be grouped as 3x
        assert!(result.contains("authRepository.test.ts  3x"));
        // test_exercise_library.py has 2 callers → 2x
        assert!(result.contains("test_exercise_library.py  2x"));
        // conftest.py has 1 caller → 1x with line number
        assert!(result.contains("conftest.py:598  1x"));
    }

    #[test]
    fn test_filter_trace_line_numbers() {
        let result = filter_trace(TRACE_RAW);
        assert!(result.contains("lines: 61,96,126"));
        assert!(result.contains("lines: 120,145"));
    }

    #[test]
    fn test_filter_trace_token_savings() {
        let input_tokens = count_tokens(TRACE_RAW);
        let output = filter_trace(TRACE_RAW);
        let output_tokens = count_tokens(&output);

        let savings = 100.0 - (output_tokens as f64 / input_tokens as f64 * 100.0);
        assert!(
            savings >= 70.0,
            "Trace filter: expected >=70% savings, got {:.1}% (in={}, out={})",
            savings,
            input_tokens,
            output_tokens
        );
    }

    #[test]
    fn test_filter_trace_empty_callers() {
        let input = r#"Symbol: isolated_func (function)
File: src/utils.py:42

Callers (0):
"#;
        let result = filter_trace(input);
        assert!(result.contains("isolated_func (function) @ utils.py:42"));
        assert!(result.contains("0 callers in 0 files"));
    }

    #[test]
    fn test_filter_trace_malformed_input() {
        let input = "random text that is not trace output";
        let result = filter_trace(input);
        assert_eq!(result, input);
    }

    #[test]
    fn test_filter_trace_callees() {
        let input = r#"Symbol: process (function)
File: src/pipeline.py:50

Callees (2):

  Callee 1:
    Function: validate (function)
    Calls at: src/validator.py:10
    Code:
        validate(data)

  Callee 2:
    Function: transform (function)
    Calls at: src/transformer.py:25
    Code:
        transform(validated)
"#;
        let result = filter_trace(input);
        assert!(result.contains("process (function) @ pipeline.py:50"));
        assert!(result.contains("2 callees in 2 files"));
        assert!(result.contains("validator.py:10  1x"));
        assert!(result.contains("transformer.py:25  1x"));
    }

    // ── helper tests ──

    #[test]
    fn test_split_path() {
        let (name, parent) = split_path("backend/src/api/auth.py");
        assert_eq!(name, "auth.py");
        assert_eq!(parent, "backend/src/api");
    }

    #[test]
    fn test_split_path_no_parent() {
        let (name, parent) = split_path("file.py");
        assert_eq!(name, "file.py");
        assert_eq!(parent, "");
    }

    #[test]
    fn test_compact_parent_short() {
        assert_eq!(compact_parent("src/api"), "src/api/");
    }

    #[test]
    fn test_compact_parent_long() {
        assert_eq!(
            compact_parent("backend/src/some/deep/path/api"),
            "backend/src/.../api/"
        );
    }
}
