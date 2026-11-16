//! `grep`: print lines matching a pattern, ported from conch-shell and
//! included under eryx's licence (MIT OR Apache-2.0) by its author.
//!
//! ```text
//! grep [-vinclLHhqsEFwx] [-m NUM] [-e PATTERN]... [PATTERN] [FILE]...
//! ```
//!
//! Patterns are GNU basic regular expressions by default, as in grep: `\|`,
//! `\+`, `\?`, `\(..\)` and `\{n,m\}` are operators, and plain `| + ? ( ) { }`
//! are literal. `-E` takes extended (regex-lite) syntax instead, and `-F` takes
//! patterns literally. Lines are matched even if they aren't valid UTF-8 and
//! are printed as they are. With no file, or `-`, it reads stdin. Exit
//! status: 0 if a line matched, 1 if none did, 2 on an error (unless `-q`
//! already found a match).

use std::io::Write as _;

use brush_core::{ExecutionContext, ExecutionResult, ShellExtensions, builtins, error};

use crate::input::{read_file, read_stdin};

pub struct GrepCommand {
    args: Vec<String>,
}

impl builtins::ParseArgs for GrepCommand {
    fn content(
        _name: &str,
        content_type: builtins::ContentType,
        _options: &builtins::ContentOptions,
    ) -> Result<String, brush_core::Error> {
        match content_type {
            builtins::ContentType::ManPage => error::unimp("no man page"),
            builtins::ContentType::ShortUsage => {
                Ok("grep [-vinclLHhqsEFwx] [-m NUM] [-e PATTERN]... [PATTERN] [FILE]...".into())
            }
            _ => Ok("grep - print lines that match patterns".into()),
        }
    }

    fn parse(invocation: builtins::Invocation<'_>) -> Result<Self, builtins::UsageError> {
        let args = invocation
            .into_args()
            .iter()
            .map(ToString::to_string)
            .collect();
        Ok(Self { args })
    }
}

impl<SE: ShellExtensions> builtins::Builtin<SE> for GrepCommand {
    type Error = brush_core::Error;

    async fn execute(
        self,
        context: ExecutionContext<'_, SE>,
    ) -> Result<ExecutionResult, Self::Error> {
        let opts = match Opts::parse(&self.args) {
            Ok(o) => o,
            Err(e) => {
                writeln!(context.stderr(), "grep: {e}")?;
                return Ok(ExecutionResult::new(2));
            }
        };
        let regex = match opts.regex() {
            Ok(r) => r,
            Err(e) => {
                writeln!(context.stderr(), "grep: {e}")?;
                return Ok(ExecutionResult::new(2));
            }
        };

        let files = if opts.files.is_empty() {
            vec!["-".to_owned()]
        } else {
            opts.files.clone()
        };
        let show_name = opts.with_filename.unwrap_or(files.len() > 1);
        let (mut matched, mut failed) = (false, false);
        for file in &files {
            let data = if file == "-" {
                read_stdin(&context).await
            } else {
                read_file(&context, file)
            };
            let data = match data {
                Ok(d) => d,
                Err(e) => {
                    failed = true;
                    if !opts.no_messages {
                        writeln!(context.stderr(), "grep: {file}: {e}")?;
                    }
                    continue;
                }
            };
            let name = if file == "-" {
                "(standard input)"
            } else {
                file
            };
            matched |= search(&data, &regex, &opts, show_name.then_some(name), &context)?;
            if matched && opts.quiet {
                break;
            }
        }
        context.stdout().flush()?;
        let code = match (matched, failed) {
            (true, _) if opts.quiet => 0,
            (_, true) => 2,
            (true, false) => 0,
            (false, false) => 1,
        };
        Ok(ExecutionResult::new(code))
    }
}

/// Search one input, printing what the options ask for; whether a line matched.
fn search<SE: ShellExtensions>(
    data: &[u8],
    regex: &regex_lite::Regex,
    opts: &Opts,
    name: Option<&str>,
    context: &ExecutionContext<'_, SE>,
) -> Result<bool, brush_core::Error> {
    let mut out = context.stdout();
    let mut count = 0usize;
    for (number, line) in lines(data).enumerate() {
        if regex.is_match(&String::from_utf8_lossy(line)) == opts.invert {
            continue;
        }
        count += 1;
        if opts.quiet || opts.files_with_matches {
            break;
        }
        if !opts.count {
            if let Some(name) = name {
                write!(out, "{name}:")?;
            }
            if opts.line_number {
                write!(out, "{}:", number + 1)?;
            }
            // The line as it was, invalid UTF-8 included.
            out.write_all(line)?;
            out.write_all(b"\n")?;
        }
        if opts.max_count.is_some_and(|max| count >= max) {
            break;
        }
    }
    if opts.quiet {
        return Ok(count > 0);
    }
    match name {
        _ if opts.files_with_matches && count > 0 => writeln!(out, "{}", name_or_stdin(name))?,
        _ if opts.files_without_match && count == 0 => writeln!(out, "{}", name_or_stdin(name))?,
        Some(name) if opts.count => writeln!(out, "{name}:{count}")?,
        None if opts.count => writeln!(out, "{count}")?,
        _ => {}
    }
    Ok(count > 0)
}

/// The lines of `data`, without their `\n`. Bytes that aren't UTF-8 stay in
/// their line rather than dropping it.
fn lines(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    let body = data.strip_suffix(b"\n").unwrap_or(data);
    (!data.is_empty())
        .then(|| body.split(|&b| b == b'\n'))
        .into_iter()
        .flatten()
}

fn name_or_stdin(name: Option<&str>) -> &str {
    name.unwrap_or("(standard input)")
}

#[derive(Debug, Default)]
struct Opts {
    patterns: Vec<String>,
    files: Vec<String>,
    invert: bool,
    ignore_case: bool,
    line_number: bool,
    count: bool,
    files_with_matches: bool,
    files_without_match: bool,
    /// `-H` (true) or `-h` (false); otherwise names show for several files.
    with_filename: Option<bool>,
    quiet: bool,
    no_messages: bool,
    fixed: bool,
    /// `-E`: the patterns are already ERE; otherwise they're GNU BRE.
    extended: bool,
    word: bool,
    line: bool,
    max_count: Option<usize>,
}

impl Opts {
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut opts = Self::default();
        let mut positional = Vec::new();
        let mut it = args.iter();
        while let Some(arg) = it.next() {
            if arg == "--" {
                positional.extend(it.by_ref().cloned());
                break;
            }
            if let Some(long) = arg.strip_prefix("--") {
                let (name, value) = match long.split_once('=') {
                    Some((n, v)) => (n, Some(v.to_owned())),
                    None => (long, None),
                };
                let mut value = || {
                    value
                        .clone()
                        .or_else(|| it.next().cloned())
                        .ok_or_else(|| format!("option '--{name}' requires an argument"))
                };
                match name {
                    "invert-match" => opts.invert = true,
                    "ignore-case" => opts.ignore_case = true,
                    "line-number" => opts.line_number = true,
                    "count" => opts.count = true,
                    "files-with-matches" => opts.files_with_matches = true,
                    "files-without-match" => opts.files_without_match = true,
                    "with-filename" => opts.with_filename = Some(true),
                    "no-filename" => opts.with_filename = Some(false),
                    "quiet" | "silent" => opts.quiet = true,
                    "no-messages" => opts.no_messages = true,
                    "extended-regexp" => opts.extended = true,
                    "fixed-strings" => opts.fixed = true,
                    "word-regexp" => opts.word = true,
                    "line-regexp" => opts.line = true,
                    "regexp" => opts.patterns.push(value()?),
                    "max-count" => opts.max_count = Some(parse_count(&value()?)?),
                    _ => return Err(format!("unrecognized option '--{name}'")),
                }
                continue;
            }
            let Some(shorts) = arg.strip_prefix('-').filter(|s| !s.is_empty()) else {
                positional.push(arg.clone());
                continue;
            };
            for (i, c) in shorts.char_indices() {
                // `-e` and `-m` take the rest of this argument, or the next one.
                let mut value = || {
                    let rest = &shorts[i + c.len_utf8()..];
                    if rest.is_empty() {
                        it.next()
                            .cloned()
                            .ok_or_else(|| format!("option requires an argument -- '{c}'"))
                    } else {
                        Ok(rest.to_owned())
                    }
                };
                match c {
                    'v' => opts.invert = true,
                    'i' => opts.ignore_case = true,
                    'n' => opts.line_number = true,
                    'c' => opts.count = true,
                    'l' => opts.files_with_matches = true,
                    'L' => opts.files_without_match = true,
                    'H' => opts.with_filename = Some(true),
                    'h' => opts.with_filename = Some(false),
                    'q' => opts.quiet = true,
                    's' => opts.no_messages = true,
                    'E' => opts.extended = true,
                    'F' => opts.fixed = true,
                    'w' => opts.word = true,
                    'x' => opts.line = true,
                    'e' => {
                        opts.patterns.push(value()?);
                        break;
                    }
                    'm' => {
                        opts.max_count = Some(parse_count(&value()?)?);
                        break;
                    }
                    _ => return Err(format!("invalid option -- '{c}'")),
                }
            }
        }
        if opts.patterns.is_empty() {
            if positional.is_empty() {
                return Err("usage: grep [OPTION]... PATTERN [FILE]...".into());
            }
            opts.patterns.push(positional.remove(0));
        }
        opts.files = positional;
        Ok(opts)
    }

    /// One regex for all patterns, with the options applied.
    fn regex(&self) -> Result<regex_lite::Regex, regex_lite::Error> {
        let alternatives: Vec<String> = self
            .patterns
            .iter()
            // A newline separates patterns, as in GNU grep.
            .flat_map(|p| p.split('\n'))
            .map(|p| {
                if self.fixed {
                    regex_lite::escape(p)
                } else if self.extended {
                    p.to_owned()
                } else {
                    bre_to_ere(p)
                }
            })
            .map(|p| format!("(?:{p})"))
            .collect();
        let mut pattern = alternatives.join("|");
        if self.word {
            pattern = format!(r"\b(?:{pattern})\b");
        }
        if self.line {
            pattern = format!("^(?:{pattern})$");
        }
        regex_lite::RegexBuilder::new(&pattern)
            .case_insensitive(self.ignore_case)
            .build()
    }
}

/// Translate a GNU basic regular expression (grep's default) to the extended
/// syntax regex-lite reads: `\|`, `\+`, `\?`, `\(`, `\)`, `\{` and `\}`
/// are operators, while plain `| + ? ( ) { }` are literal. A leading `*` is
/// literal too. Bracket expressions are copied as they are.
fn bre_to_ere(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len());
    let mut chars = pattern.chars().peekable();
    let mut at_start = true;
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some(op @ ('|' | '+' | '?' | '(' | ')' | '{' | '}')) => out.push(op),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push_str("\\\\"),
            },
            '|' | '+' | '?' | '(' | ')' | '{' | '}' => {
                out.push('\\');
                out.push(c);
            }
            '*' if at_start => out.push_str("\\*"),
            '[' => {
                // `[`, an optional `^`, then `]` as the first member is literal.
                out.push('[');
                if chars.peek() == Some(&'^') {
                    out.push(chars.next().unwrap_or('^'));
                }
                if chars.peek() == Some(&']') {
                    out.push_str("\\]");
                    chars.next();
                }
                for c in chars.by_ref() {
                    if c == '\\' {
                        // A backslash is literal inside a bracket expression.
                        out.push_str("\\\\");
                        continue;
                    }
                    out.push(c);
                    if c == ']' {
                        break;
                    }
                }
            }
            _ => out.push(c),
        }
        at_start = c == '^' && out.len() == 1;
    }
    out
}

fn parse_count(text: &str) -> Result<usize, String> {
    text.parse()
        .map_err(|_| format!("invalid max count '{text}'"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(args: &[&str]) -> Opts {
        let args: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
        Opts::parse(&args).unwrap()
    }

    #[test]
    fn parses_bundled_short_options_and_values() {
        let o = opts(&["-inm2", "-efoo", "-e", "bar", "a.txt", "b.txt"]);
        assert!(o.ignore_case && o.line_number);
        assert_eq!(o.max_count, Some(2));
        assert_eq!(o.patterns, ["foo", "bar"]);
        assert_eq!(o.files, ["a.txt", "b.txt"]);

        let o = opts(&["--max-count=1", "--", "-v", "f"]);
        assert_eq!(o.patterns, ["-v"]);
        assert_eq!(o.files, ["f"]);
        assert!(Opts::parse(&["-Z".to_owned(), "x".to_owned()]).is_err());
        assert!(Opts::parse(&[]).is_err());
    }

    #[test]
    fn lines_keep_invalid_utf8_and_drop_the_final_newline() {
        let data = b"good\nba\xffd\nlast\n";
        let got: Vec<&[u8]> = lines(data).collect();
        assert_eq!(got, [&b"good"[..], b"ba\xffd", b"last"]);
        assert_eq!(lines(b"").count(), 0);
        assert_eq!(lines(b"\n").collect::<Vec<_>>(), [&b""[..]]);
        assert_eq!(lines(b"a").count(), 1);
        // A line with invalid UTF-8 still matches on its valid parts.
        let re = opts(&["d"]).regex().unwrap();
        let hits: Vec<_> = lines(data)
            .filter(|l| re.is_match(&String::from_utf8_lossy(l)))
            .collect();
        assert_eq!(hits, [&b"good"[..], b"ba\xffd"]);
    }

    #[test]
    fn basic_regexps_are_translated_unless_extended() {
        let m = |args: &[&str], line: &str| opts(args).regex().unwrap().is_match(line);
        // BRE (the default): escaped operators, literal plain ones.
        assert!(m(&["a\\|b"], "b"));
        assert!(m(&["ab\\+c"], "abbc"));
        assert!(m(&["^\\(ab\\)\\{2\\}$"], "abab"));
        assert!(m(&["a|b"], "a|b"));
        assert!(!m(&["a|b"], "a"));
        assert!(m(&["(x)"], "(x)"));
        assert!(m(&["a+"], "a+"));
        assert!(m(&["*x"], "*x"));
        assert!(m(&["[|+]"], "+"));
        assert!(m(&["[]a]"], "]"));
        assert!(m(&["a\\.b"], "a.b") && !m(&["a\\.b"], "axb"));
        // ERE with -E: as written.
        assert!(m(&["-E", "a|b"], "b"));
        assert!(m(&["-E", "ab+c"], "abbc"));
        assert!(m(&["-E", "a\\|b"], "a|b") && !m(&["-E", "a\\|b"], "b"));
    }

    #[test]
    fn regex_applies_fixed_word_line_and_case() {
        let m = |args: &[&str], line: &str| opts(args).regex().unwrap().is_match(line);
        assert!(m(&["a.c"], "abc"));
        assert!(!m(&["-F", "a.c"], "abc"));
        assert!(m(&["-F", "a.c"], "xa.cx"));
        assert!(!m(&["-w", "ab"], "abc"));
        assert!(m(&["-w", "ab"], "x ab y"));
        assert!(!m(&["-x", "ab"], "abc"));
        assert!(m(&["-i", "ABC"], "xabcx"));
        assert!(m(&["-e", "zz", "-e", "b"], "abc"));
    }
}
