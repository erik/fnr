use std::borrow::Cow;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{ensure, Context, Result};
use atty::Stream;
use grep::matcher::{Captures, Matcher};
use grep::regex::{RegexMatcher, RegexMatcherBuilder};
use grep::searcher::{BinaryDetection, SearcherBuilder};
use ignore::{WalkBuilder, WalkState};
use regex::RegexSet;
use structopt::clap::arg_enum;
use structopt::StructOpt;
use termcolor::{BufferWriter, ColorChoice, ColorSpec, WriteColor};
use text_io::read;

mod search;

use crate::search::{Match, RegexSearcherBuilder};

arg_enum! {
    #[derive(Debug)]
    enum ColorPreference {
        Always,
        Auto,
        Never
    }
}

impl ColorPreference {
    fn as_color_choice(&self) -> ColorChoice {
        match *self {
            ColorPreference::Always => ColorChoice::Always,
            ColorPreference::Auto => {
                if atty::is(Stream::Stdout) {
                    ColorChoice::Auto
                } else {
                    ColorChoice::Never
                }
            }
            ColorPreference::Never => ColorChoice::Never,
        }
    }
}

#[derive(Debug, StructOpt)]
#[structopt(name = "fnr")]
/// Recursively find and replace. Like sed, but memorable.
// TODO: Potential features:
//
// /// Search files with the given file extensions.
// #[structopt(short = "T", long, multiple = true, conflicts_with = "include")]
// file_type: Option<String>,
//
// /// Save changes as a .patch file rather than modifying in place.
// #[structopt(long)]
// write_patch: bool
struct Config {
    /// Match case insensitively.
    #[structopt(short = "i", long, conflicts_with = "case_sensitive, smart_case")]
    ignore_case: bool,

    /// Match case sensitively.
    #[structopt(short = "s", long, conflicts_with = "ignore_case, smart_case")]
    case_sensitive: bool,

    /// Match case sensitively if FIND has uppercase characters,
    /// insensitively otherwise. [default: true].
    #[structopt(
        short = "S",
        long,
        conflicts_with = "ignore_case, case_sensitive",
        takes_value = false
    )]
    smart_case: Option<bool>,

    /// Disable printing matches
    #[structopt(short, long, conflicts_with = "prompt")]
    quiet: bool,

    /// Display compacted output format
    #[structopt(short, long, conflicts_with = "prompt, quiet")]
    compact: bool,

    /// Modify files in place.
    #[structopt(short = "W", long, conflicts_with = "prompt")]
    write: bool,

    /// Match FIND only at word boundary.
    #[structopt(short, long)]
    word: bool,

    /// Treat FIND as a string rather than a regular expression.
    #[structopt(long)]
    literal: bool,

    /// Search ALL files in given paths for matches.
    #[structopt(short, long, conflicts_with = "hidden")]
    all_files: bool,

    /// Find replacements in hidden files and directories.
    #[structopt(short = "H", long, conflicts_with = "all_files")]
    hidden: bool,

    /// Confirm each modification before making it. Implies --write.
    #[structopt(short, long, conflicts_with = "write")]
    prompt: bool,

    /// Print lines after matches.
    #[structopt(short = "A", long)]
    after: Option<usize>,

    /// Print lines before matches.
    #[structopt(short = "B", long)]
    before: Option<usize>,

    /// Print lines before and after matches.
    #[structopt(short = "C", long, conflicts_with = "after, before")]
    context: Option<usize>,

    /// Include only files or directories matching pattern.
    #[structopt(short = "I", long)]
    include: Option<Vec<String>>,

    /// Exclude files or directories matching pattern.
    #[structopt(short = "E", long)]
    exclude: Vec<String>,

    /// Control whether terminal output is in color
    #[structopt(
        long,
        possible_values = &ColorPreference::variants(),
        case_insensitive = true,
        default_value = "auto"
    )]
    color: ColorPreference,

    /// What to search for. Literal string or regular expression.
    ///
    /// For supported regular expression syntax, see:
    /// https://docs.rs/regex/latest/regex/#syntax
    #[structopt(name = "FIND", required = true)]
    find: String,

    /// What to replace it with.
    ///
    /// May contain numbered references to capture groups given in
    /// FIND in the form $1, $2, etc.
    #[structopt(name = "REPLACE", required = true)]
    replace: String,

    /// Locations to search. Current directory if not given.
    ///
    /// Paths may also be provided through standard input, e.g.
    ///
    /// $ fd .rs | fnr 'old_fn' 'new_fn'
    #[structopt(name = "PATH", parse(from_os_str))]
    paths: Vec<PathBuf>,
}

impl Config {}

#[derive(Copy, Clone)]
enum MatchPrintMode {
    Silent,
    Compact,
    Full,
}

struct MatchFormatterBuilder {
    print_mode: MatchPrintMode,
    writes_enabled: bool,
}

impl MatchFormatterBuilder {
    fn from_config(cfg: &Config) -> MatchFormatterBuilder {
        MatchFormatterBuilder {
            print_mode: if cfg.quiet {
                MatchPrintMode::Silent
            } else if cfg.compact {
                MatchPrintMode::Compact
            } else {
                MatchPrintMode::Full
            },
            writes_enabled: cfg.write || cfg.prompt,
        }
    }

    fn build<'a, W: WriteColor>(&self, writer: &'a mut W) -> MatchFormatter<'a, W> {
        MatchFormatter {
            writer,
            print_mode: self.print_mode,
            writes_enabled: self.writes_enabled,
            last_line_num: None,
        }
    }
}

struct MatchFormatter<'a, W: WriteColor> {
    writer: &'a mut W,

    print_mode: MatchPrintMode,
    writes_enabled: bool,
    last_line_num: Option<u64>,
}

impl<'a, W: WriteColor> MatchFormatter<'a, W> {
    fn display_header(&mut self, path: &Path, num_matches: usize) -> Result<()> {
        match self.print_mode {
            MatchPrintMode::Silent => Ok(()),
            MatchPrintMode::Compact => Ok(()),
            MatchPrintMode::Full => self.display_header_full(path, num_matches),
        }
    }

    #[inline]
    fn display_header_full(&mut self, path: &Path, num_matches: usize) -> Result<()> {
        self.writer
            .set_color(ColorSpec::new().set_underline(true))?;

        writeln!(
            &mut self.writer,
            "{}\x1B[0m {} match{}",
            path.display(),
            num_matches,
            if num_matches == 1 { "" } else { "es" }
        )?;

        self.last_line_num = None;
        Ok(())
    }

    fn display_match(
        &mut self,
        path: &Path,
        search_match: &Match,
        replacement: &str,
    ) -> Result<()> {
        match self.print_mode {
            MatchPrintMode::Silent => Ok(()),
            MatchPrintMode::Compact => self.display_match_compact(path, search_match, replacement),
            MatchPrintMode::Full => self.display_match_full(search_match, replacement),
        }
    }

    #[inline]
    fn display_match_compact(
        &mut self,
        path: &Path,
        search_match: &Match,
        replacement: &str,
    ) -> Result<()> {
        let path = path.display();

        for line in &search_match.context_pre {
            write!(&mut self.writer, "{}:{}:{}", path, line.0, line.1)?;
        }

        write!(
            &mut self.writer,
            "\x1B[31m{}:{}-{}\x1B[0m",
            path, search_match.line.0, search_match.line.1
        )?;
        write!(
            &mut self.writer,
            "\x1B[32m{}:{}+{}\x1B[0m",
            path, search_match.line.0, replacement
        )?;

        for line in &search_match.context_post {
            write!(&mut self.writer, "{}:{}:{}", path, line.0, line.1)?;
        }

        Ok(())
    }

    #[inline]
    fn display_match_full(&mut self, m: &Match, replacement: &str) -> Result<()> {
        let has_line_break = self
            .last_line_num
            .map(|last_line_num| {
                if !m.context_pre.is_empty() {
                    m.context_pre[0].0 > last_line_num + 1
                } else {
                    m.line.0 > last_line_num + 1
                }
            })
            .unwrap_or(false);

        if has_line_break {
            writeln!(&mut self.writer, "  ---")?;
        }

        for line in &m.context_pre {
            write!(&mut self.writer, " {:4} {}", line.0, line.1)?;
        }

        // TODO: Highlight matching part of line
        // TODO: Disable colors when not atty
        write!(
            &mut self.writer,
            "\x1B[31m-{:4} {}\x1B[0m",
            m.line.0, m.line.1
        )?;
        write!(
            &mut self.writer,
            "\x1B[32m+{:4} {}\x1B[0m",
            m.line.0, replacement
        )?;

        for line in &m.context_post {
            write!(&mut self.writer, " {:4} {}", line.0, line.1)?;
            self.last_line_num.replace(line.0);
        }

        Ok(())
    }

    fn display_footer(&mut self, total_replacements: usize, total_matches: usize) -> Result<()> {
        match self.print_mode {
            MatchPrintMode::Silent => Ok(()),
            MatchPrintMode::Compact => Ok(()),
            MatchPrintMode::Full => self.display_footer_full(total_replacements, total_matches),
        }
    }

    #[inline]
    fn display_footer_full(
        &mut self,
        total_replacements: usize,
        total_matches: usize,
    ) -> Result<()> {
        writeln!(
            &mut self.writer,
            "All done. Replaced {} of {} matches",
            total_replacements, total_matches
        )?;

        if !self.writes_enabled {
            writeln!(
                &mut self.writer,
                "Use -w, --write to modify files in place."
            )?;
        }

        Ok(())
    }
}

struct RegexReplacer {
    matcher: RegexMatcher,
    template: String,
}

impl RegexReplacer {
    fn replace(&self, input: &str) -> Result<String> {
        let mut caps = self.matcher.new_captures().unwrap();
        let mut dst = vec![];

        self.matcher.replace_with_captures(
            input.as_bytes(),
            &mut caps,
            &mut dst,
            |caps, dst| {
                caps.interpolate(
                    |name| self.matcher.capture_index(name),
                    input.as_bytes(),
                    self.template.as_bytes(),
                    dst,
                );
                true
            },
        )?;

        Ok(String::from_utf8_lossy(&dst).to_string())
    }
}

struct MatchReplacement<'a> {
    search_match: Match,
    replacement: Cow<'a, str>,
}

struct MatchProcessor {
    replacer: Arc<RegexReplacer>,
    replacement_decider: ReplacementDecider,
}

impl MatchProcessor {
    fn new(
        replacer: Arc<RegexReplacer>,
        replacement_decider: ReplacementDecider,
    ) -> MatchProcessor {
        MatchProcessor {
            replacer,
            replacement_decider,
        }
    }

    fn consume_matches<W: WriteColor>(
        &mut self,
        path: &Path,
        matches: Vec<Match>,
        match_formatter: &mut MatchFormatter<W>,
    ) -> Result<bool> {
        if matches.is_empty() {
            return Ok(true);
        }

        match_formatter.display_header(path, matches.len())?;

        self.replacement_decider.reset_local_decision();

        let mut replacement_list = Vec::with_capacity(matches.len());
        for m in matches.into_iter() {
            let replacement = self.replacer.replace(&m.line.1)?;
            match_formatter.display_match(path, &m, &replacement)?;

            let match_replacement = match self.replacement_decider.decide() {
                ReplacementDecision::Accept => MatchReplacement {
                    search_match: m,
                    replacement: replacement.into(),
                },
                ReplacementDecision::Ignore => continue,
                ReplacementDecision::Edit => {
                    let mut line = read_input("Replace with [^D to skip] ")?;
                    if line.is_empty() {
                        println!("... skipped ...");
                        continue;
                    }

                    line.push('\n');
                    match_formatter.display_match(path, &m, &line)?;
                    println!("--");
                    MatchReplacement {
                        search_match: m,
                        replacement: line.into(),
                    }
                }
                ReplacementDecision::Terminate => {
                    println!("exiting!");
                    // TODO: represent this code more cleanly?
                    return Ok(false);
                }
            };

            replacement_list.push(match_replacement);
        }

        if !replacement_list.is_empty() {
            self.apply_replacements(path, &replacement_list)?;
        }

        Ok(true)
    }

    fn apply_replacements(&self, path: &Path, mut replacements: &[MatchReplacement]) -> Result<()> {
        let dst_path = path.with_extension("~");
        let src = File::open(path)?;
        let dst = File::create(&dst_path)?;

        let mut reader = BufReader::new(src);
        let mut writer = BufWriter::new(dst);

        let mut line_num = 0;
        let mut line = String::new();
        loop {
            line.clear();
            let bytes_read = reader.read_line(&mut line)?;
            // EOF reached
            if bytes_read == 0 {
                break;
            }

            line_num += 1;

            if !replacements.is_empty() && replacements[0].search_match.line.0 == line_num {
                writer.write_all(replacements[0].replacement.as_bytes())?;
                replacements = &replacements[1..];
            } else {
                writer.write_all(line.as_bytes())?;
            }
        }

        if !replacements.is_empty() {
            panic!("reached EOF with remaining replacements");
        }

        std::fs::rename(dst_path, path)?;
        Ok(())
    }

    fn finalize(&self) -> Result<()> {
        Ok(())
        // match_formatter
        //     .display_footer(self.total_replacements, self.total_matches)
    }
}

#[derive(Debug, Copy, Clone)]
enum ReplacementDecision {
    Accept,
    Ignore,
    Edit,
    Terminate,
}

fn read_input(prompt: &str) -> Result<String, std::io::Error> {
    print!("{}", prompt);
    std::io::stdout().flush()?;

    Ok(read!("{}\n"))
}

#[derive(Clone, Copy)]
struct ReplacementDecider {
    should_prompt: bool,
    global_decision: Option<ReplacementDecision>,
    local_decision: Option<ReplacementDecision>,
}

impl ReplacementDecider {
    fn new(
        should_prompt: bool,
        global_decision: Option<ReplacementDecision>,
    ) -> ReplacementDecider {
        ReplacementDecider {
            global_decision,
            should_prompt,
            local_decision: None,
        }
    }

    fn reset_local_decision(&mut self) {
        self.local_decision = None;
    }

    fn decide(&mut self) -> ReplacementDecision {
        if let Some(global_decision) = self.global_decision {
            return global_decision;
        } else if let Some(local_decision) = self.local_decision {
            return local_decision;
        }

        if !self.should_prompt {
            panic!("[bug] invalid state: no decision, but should not prompt");
        }

        self.prompt_for_decision()
    }

    fn prompt_for_decision(&mut self) -> ReplacementDecision {
        loop {
            let line = read_input("Stage this replacement [y,n,q,a,e,d,?] ").unwrap();

            return match line.as_str() {
                "y" => ReplacementDecision::Accept,
                "n" => ReplacementDecision::Ignore,
                "q" => ReplacementDecision::Terminate,
                "a" => {
                    self.local_decision = Some(ReplacementDecision::Accept);
                    ReplacementDecision::Accept
                }
                "d" => {
                    self.local_decision = Some(ReplacementDecision::Ignore);
                    ReplacementDecision::Ignore
                }
                "e" => ReplacementDecision::Edit,

                _ => {
                    println!(
                        "\x1B[31m
Y - replace this line
n - do not replace this line
q - quit; do not replace this line or any remaining ones
a - replace this line and all remaining ones in this file
d - do not replace this line nor any remaining ones in this file
e - edit this replacement
? - show help
\x1B[0m"
                    );
                    continue;
                }
            };
        }
    }
}

struct FindAndReplacer {
    config: Config,
    file_walker: WalkBuilder,
    path_matcher: PathMatcher,
    match_formatter: MatchFormatterBuilder,
    match_processor_factory: Box<dyn Fn() -> MatchProcessor>,
    searcher_builder: RegexSearcherBuilder,
}

const DEFAULT_BEFORE_CONTEXT_LINES: usize = 2;
const DEFAULT_AFTER_CONTEXT_LINES: usize = 2;

impl FindAndReplacer {
    fn from_config(config: Config) -> Result<FindAndReplacer> {
        let pattern = if config.literal {
            regex::escape(&config.find)
        } else {
            config.find.to_owned()
        };

        let pattern_matcher = RegexMatcherBuilder::new()
            .case_insensitive(!config.case_sensitive && config.ignore_case)
            .case_smart(!config.case_sensitive && config.smart_case.unwrap_or(true))
            .build(&pattern)
            .with_context(|| format!("Failed to parse pattern '{}'", pattern))?;

        // TODO: Confirm that template does not reference more capture groups than exist.
        let replacer = RegexReplacer {
            matcher: pattern_matcher.clone(),
            template: config.replace.to_owned(),
        };

        let match_formatter = MatchFormatterBuilder::from_config(&config);

        let match_processor_factory = {
            let replacer = Arc::new(replacer);

            let global_replacement_decision = if config.prompt {
                None
            } else if config.write {
                Some(ReplacementDecision::Accept)
            } else {
                Some(ReplacementDecision::Ignore)
            };

            let replacement_decider =
                ReplacementDecider::new(config.prompt, global_replacement_decision);

            Box::new(move || MatchProcessor::new(replacer.clone(), replacement_decider))
        };

        let mut searcher_builder = SearcherBuilder::new();
        searcher_builder
            .binary_detection(BinaryDetection::quit(0x00))
            .line_number(true)
            .before_context(
                config
                    .before
                    .or(config.context)
                    .unwrap_or(DEFAULT_BEFORE_CONTEXT_LINES),
            )
            .after_context(
                config
                    .after
                    .or(config.context)
                    .unwrap_or(DEFAULT_AFTER_CONTEXT_LINES),
            );

        let paths = if config.paths.is_empty() {
            // Read paths from standard in if none are specified and
            // there's input piped to the process.
            if !atty::is(Stream::Stdin) {
                ensure!(
                    !config.prompt,
                    "cannot use --prompt when reading files from stdin"
                );
                let mut paths = vec![];
                for line in std::io::stdin().lock().lines() {
                    paths.push(PathBuf::from(line.unwrap()));
                }
                paths
            } else {
                vec![PathBuf::from(".")]
            }
        } else {
            // TODO: remove clone
            config.paths.clone()
        };

        let mut file_walker = WalkBuilder::new(&paths[0]);
        {
            for path in &paths[1..] {
                file_walker.add(path);
            }

            file_walker.threads(num_cpus::get());

            let should_ignore = !config.all_files;
            let should_show_hidden = config.hidden || config.all_files;
            file_walker
                .hidden(!should_show_hidden)
                .ignore(should_ignore)
                .git_ignore(should_ignore)
                .git_exclude(should_ignore)
                .parents(should_ignore);
        }

        let included_paths = config.include.as_ref().map(|included_paths| {
            let escaped = included_paths.iter().map(|p| regex::escape(p));
            RegexSet::new(escaped).unwrap()
        });
        let excluded_paths = {
            let escaped = config.exclude.iter().map(|p| regex::escape(p));
            RegexSet::new(escaped)?
        };

        let path_matcher = PathMatcher {
            included_paths,
            excluded_paths,
        };

        let searcher_builder = RegexSearcherBuilder::new(searcher_builder, pattern_matcher);

        Ok(FindAndReplacer {
            config,
            file_walker,
            path_matcher,
            searcher_builder,
            match_processor_factory,
            match_formatter,
        })
    }

    fn run_with_prompt(&mut self) -> Result<()> {
        // TODO: Write single threaded variant once the basic data
        // structures are stable.
        Ok(())
    }

    fn run_parallel(&mut self) -> Result<()> {
        let buf_writer = BufferWriter::stdout(self.config.color.as_color_choice());

        let file_walker = self.file_walker.build_parallel();
        file_walker.run(|| {
            let buf_writer = &buf_writer;
            let path_matcher = &self.path_matcher;
            let match_formatter = &self.match_formatter;

            let mut searcher = self.searcher_builder.build();
            let mut match_processor = (self.match_processor_factory)();

            Box::new(move |dir_entry| {
                let path = match dir_entry {
                    Ok(ref ent) if ent.file_type().map_or(false, |it| it.is_file()) => ent.path(),
                    Ok(_) => {
                        return WalkState::Continue;
                    }
                    Err(err) => {
                        eprintln!("error: {}", err);
                        return WalkState::Continue;
                    }
                };

                if !path_matcher.is_match(path) {
                    return WalkState::Continue;
                }

                let matches = match searcher.search_path(path) {
                    // No futher processing required for empty matches.
                    Ok(m) if m.is_empty() => {
                        return WalkState::Continue;
                    }

                    Ok(m) => m,

                    err => {
                        eprintln!("search failed: {:?}", err);
                        return WalkState::Quit;
                    }
                };

                let mut buffer = buf_writer.buffer();
                let mut match_formatter = match_formatter.build(&mut buffer);

                let should_proceed =
                    match_processor.consume_matches(path, matches, &mut match_formatter);

                if let Err(err) = buf_writer.print(&buffer) {
                    if err.kind() == std::io::ErrorKind::BrokenPipe {
                        return WalkState::Quit;
                    }
                    eprintln!("{}: {}", path.display(), err);
                }

                match should_proceed {
                    Ok(true) => WalkState::Continue,
                    Ok(false) => WalkState::Quit,
                    Err(err) => {
                        eprintln!("{}: {}", path.display(), err);
                        WalkState::Quit
                    }
                }
            })
        });

        // self.match_processor.finalize()?;

        Ok(())
    }
}

struct PathMatcher {
    included_paths: Option<RegexSet>,
    excluded_paths: RegexSet,
}

impl PathMatcher {
    fn is_match(&self, path: &Path) -> bool {
        let path_str = path
            .to_str()
            .with_context(|| format!("Failed to interpret path name as UTF-8 string: {:?}", path))
            .unwrap();

        if let Some(included_paths) = &self.included_paths {
            return included_paths.is_match(path_str);
        }

        !self.excluded_paths.is_match(path_str)
    }
}

// Main entry point
fn run_find_and_replace() -> Result<()> {
    let config = Config::from_args();
    let mut find_and_replacer = FindAndReplacer::from_config(config)?;

    find_and_replacer.run_parallel()
}

fn main() {
    let exit_code = match run_find_and_replace() {
        Err(e) => {
            eprintln!("{:?}", e);
            1
        }
        Ok(()) => 0,
    };

    std::process::exit(exit_code);
}

#[cfg(test)]
mod tests {
    use super::*;
    mod path_matcher {
        use super::*;

        fn as_regex_set(v: Vec<&str>) -> RegexSet {
            let escaped = v.iter().map(|r| regex::escape(r));
            RegexSet::new(escaped).unwrap()
        }

        #[test]
        fn test_empty_included_set() {
            let disallow_list: Vec<&str> = vec![];

            let matcher = PathMatcher {
                included_paths: None,
                excluded_paths: as_regex_set(disallow_list),
            };

            assert_eq!(matcher.is_match(&Path::new("foo")), true);
        }

        #[test]
        fn test_included_set() {
            let allow_list: Vec<&str> = vec!["foo", "bar"];
            let disallow_list: Vec<&str> = vec![];

            let matcher = PathMatcher {
                included_paths: Some(as_regex_set(allow_list)),
                excluded_paths: as_regex_set(disallow_list),
            };

            assert_eq!(matcher.is_match(&Path::new("foo.rs")), true);
            assert_eq!(matcher.is_match(&Path::new("bar.rs")), true);
            assert_eq!(matcher.is_match(&Path::new("baz.rs")), false);
        }

        #[test]
        fn test_excluded_set() {
            let disallow_list = vec!["foo", "bar"];
            let matcher = PathMatcher {
                included_paths: None,
                excluded_paths: as_regex_set(disallow_list),
            };

            assert_eq!(matcher.is_match(&Path::new("foo.rs")), false);
            assert_eq!(matcher.is_match(&Path::new("bar.rs")), false);
            assert_eq!(matcher.is_match(&Path::new("baz.rs")), true);
        }

        // Inclusion should take precedence
        #[test]
        fn test_included_and_excluded_set() {
            let allow_list = vec!["foo", "bar"];
            let disallow_list = vec!["foo", "bar"];

            let matcher = PathMatcher {
                included_paths: Some(as_regex_set(allow_list)),
                excluded_paths: as_regex_set(disallow_list),
            };

            assert_eq!(matcher.is_match(&Path::new("foo.rs")), true);
            assert_eq!(matcher.is_match(&Path::new("bar.rs")), true);
            assert_eq!(matcher.is_match(&Path::new("baz.rs")), false);
        }
    }
}
