use std::borrow::Cow;
use std::fmt;
use std::io;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, ensure, Context, Result};
use atty::Stream;
use clap::Parser;
use grep::regex::{RegexMatcher, RegexMatcherBuilder};
use grep::searcher::{BinaryDetection, SearcherBuilder};
use ignore::{DirEntry, WalkBuilder, WalkState};
use regex::RegexSet;
use termcolor::{BufferWriter, ColorChoice, StandardStream};

mod printer;
mod replace;
mod search;

use crate::printer::{MatchPrintMode, MatchPrinterBuilder};
use crate::replace::{ReplacementDecider, ReplacementDecision, ReplacerFactory};
use crate::search::RegexSearcherFactory;

#[derive(Debug)]
pub struct Statistics {
    wall_time_ns: AtomicU64,
    search_time_ns: AtomicU64,
    files_total: AtomicUsize,
    files_searched: AtomicUsize,
    files_ignored: AtomicUsize,
    files_with_matches: AtomicUsize,
    files_with_replacements: AtomicUsize,
    num_matches: AtomicUsize,
    num_replacements: AtomicUsize,
}

struct StatSearchTimer<'a> {
    started_at: Instant,
    stats: &'a Statistics,
}

impl<'a> Drop for StatSearchTimer<'a> {
    fn drop(&mut self) {
        let elapsed = self.started_at.elapsed();
        self.stats.add_elapsed_search_time(elapsed);
    }
}

impl Statistics {
    fn new() -> Statistics {
        Statistics {
            wall_time_ns: 0.into(),
            search_time_ns: 0.into(),
            files_total: 0.into(),
            files_searched: 0.into(),
            files_ignored: 0.into(),
            files_with_matches: 0.into(),
            files_with_replacements: 0.into(),

            num_matches: 0.into(),
            num_replacements: 0.into(),
        }
    }

    fn num_matches(&self) -> usize {
        self.num_matches.load(Ordering::Relaxed)
    }

    fn num_replacements(&self) -> usize {
        self.num_replacements.load(Ordering::Relaxed)
    }

    fn search_timer(&self) -> StatSearchTimer {
        StatSearchTimer {
            stats: self,
            started_at: Instant::now(),
        }
    }

    #[inline]
    fn add_elapsed_wall_time(&self, d: Duration) {
        self.wall_time_ns
            .fetch_add(d.as_nanos().try_into().unwrap_or(0), Ordering::Relaxed);
    }

    #[inline]
    fn add_elapsed_search_time(&self, d: Duration) {
        self.search_time_ns
            .fetch_add(d.as_nanos().try_into().unwrap_or(0), Ordering::Relaxed);
    }

    #[inline]
    fn visit_file(&self, searched: bool) {
        self.files_total.fetch_add(1, Ordering::Relaxed);
        if searched {
            self.files_searched.fetch_add(1, Ordering::Relaxed);
        } else {
            self.files_ignored.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[inline]
    fn add_matches(&self, num_matches: usize) {
        self.files_with_matches.fetch_add(1, Ordering::Relaxed);
        self.num_matches.fetch_add(num_matches, Ordering::Relaxed);
    }

    #[inline]
    fn add_replacements(&self, num_replacements: usize) {
        self.files_with_replacements.fetch_add(1, Ordering::Relaxed);
        self.num_replacements
            .fetch_add(num_replacements, Ordering::Relaxed);
    }
}

impl fmt::Display for Statistics {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "\
wall time               {wall_time_secs} s
search time             {search_time_secs} s
num matches             {num_matches:?}
num replacements        {num_replacements:?}
total files             {files_total:?}
  ... ignored           {files_ignored:?}
  ... searched          {files_searched:?}
  ... with matches      {files_with_matches:?}
  ... with replacements {files_with_replacements:?}",
            wall_time_secs =
                Duration::from_nanos(self.wall_time_ns.load(Ordering::Relaxed)).as_secs_f32(),
            search_time_secs =
                Duration::from_nanos(self.search_time_ns.load(Ordering::Relaxed)).as_secs_f32(),
            num_matches = self.num_matches,
            num_replacements = self.num_replacements,
            files_total = self.files_total,
            files_ignored = self.files_ignored,
            files_searched = self.files_searched,
            files_with_matches = self.files_with_matches,
            files_with_replacements = self.files_with_replacements,
        )
    }
}

#[derive(Debug, clap::ArgEnum, Clone)]
enum ColorPreference {
    Always,
    Auto,
    Never,
}

#[derive(Debug, clap::Parser)]
#[clap(name = "fnr", about, version, author)]
/// Recursively find and replace. Like sed, but memorable.
// TODO: Potential features:
//
// /// Search files with the given file extensions.
// #[clap(short = "T", long, multiple = true, conflicts_with = "include")]
// file_type: Option<String>,
//
// /// Save changes as a .patch file rather than modifying in place.
// #[clap(long)]
// write_patch: bool
struct Config {
    /// Match case insensitively.
    #[clap(short = 'i', long)]
    ignore_case: bool,

    /// Match case sensitively.
    #[clap(short = 's', long)]
    case_sensitive: bool,

    /// Match case sensitively if FIND has uppercase characters,
    /// insensitively otherwise. [default: true].
    #[clap(
        short = 'S',
        long,
        conflicts_with_all = &["ignore-case", "case-sensitive"],
        takes_value = false
    )]
    smart_case: Option<bool>,

    /// Disable printing matches
    #[clap(short, long)]
    quiet: bool,

    /// Display compacted output format
    #[clap(short, long, conflicts_with_all = &["quiet"])]
    compact: bool,

    /// Modify files in place.
    #[clap(short = 'W', long)]
    write: bool,

    /// Perform search and replace without modifying files.
    #[clap(long)]
    dry_run: bool,

    /// Treat FIND as a string rather than a regular expression.
    #[clap(short = 'Q', long)]
    literal: bool,

    /// Match FIND only at word boundary.
    #[clap(short, long)]
    word: bool,

    /// Search ALL files in given paths for matches.
    #[clap(short, long, conflicts_with = "hidden")]
    all_files: bool,

    /// Find replacements in hidden files and directories.
    #[clap(short = 'H', long, conflicts_with = "all-files")]
    hidden: bool,

    /// Print lines after matches.
    #[clap(short = 'A', long)]
    after: Option<usize>,

    /// Print lines before matches.
    #[clap(short = 'B', long)]
    before: Option<usize>,

    /// Print lines before and after matches.
    #[clap(short = 'C', long, conflicts_with_all = &["after", "before"])]
    context: Option<usize>,

    /// Include only files or directories matching pattern.
    #[clap(short = 'I', long)]
    include: Option<Vec<String>>,

    /// Exclude files or directories matching pattern.
    #[clap(short = 'E', long)]
    exclude: Vec<String>,

    /// Control whether terminal output is in color.
    #[clap(arg_enum, long, ignore_case = true, default_value = "auto")]
    color: ColorPreference,

    /// Print debug statistics about match.
    #[clap(long = "stats")]
    print_stats: bool,

    /// What to search for. Literal string or regular expression.
    ///
    /// For supported regular expression syntax, see:
    /// https://docs.rs/regex/latest/regex/#syntax
    #[clap(name = "FIND", required = true)]
    find: String,

    /// What to replace it with.
    ///
    /// May contain numbered references to capture groups given in
    /// FIND in the form $1, $2, etc.
    #[clap(name = "REPLACE", required = true)]
    replace: String,

    /// Locations to search. Current directory if not given.
    ///
    /// Paths may also be provided through standard input, e.g.
    ///
    /// $ fd .rs | fnr 'old_fn' 'new_fn'
    #[clap(name = "PATH", parse(from_os_str))]
    paths: Vec<PathBuf>,
}

impl Config {
    fn path_matcher(&self) -> Result<PathMatcher> {
        let included_paths = self.include.as_ref().map(|included_paths| {
            let escaped = included_paths.iter().map(|p| regex::escape(p));
            RegexSet::new(escaped).unwrap()
        });

        let excluded_paths = {
            let escaped = self.exclude.iter().map(|p| regex::escape(p));
            RegexSet::new(escaped)?
        };

        Ok(PathMatcher {
            included_paths,
            excluded_paths,
        })
    }

    fn pattern(&self) -> Cow<str> {
        if self.literal {
            regex::escape(&self.find).into()
        } else {
            self.find.as_str().into()
        }
    }

    fn regex_matcher(&self) -> Result<RegexMatcher> {
        let pattern = self.pattern();

        RegexMatcherBuilder::new()
            .case_insensitive(!self.case_sensitive && self.ignore_case)
            .case_smart(!self.case_sensitive && self.smart_case.unwrap_or(true))
            .word(self.word)
            .build(&pattern)
            .with_context(|| format!("Failed to parse pattern '{}'", pattern))
    }

    fn search_paths(&self) -> Result<Cow<[PathBuf]>> {
        if !self.paths.is_empty() {
            return Ok(Cow::from(&self.paths));
        }

        // Read paths from standard in if none are specified and
        // there's input piped to the process.
        //
        // Otherwise, we just search the current directory.
        let paths = if !atty::is(Stream::Stdin) {
            ensure!(
                !self.is_interactive(),
                "cannot use interactive prompt when reading files from stdin"
            );
            let mut paths = vec![];
            for line in std::io::stdin().lock().lines() {
                paths.push(PathBuf::from(line.unwrap()));
            }
            paths
        } else {
            vec![PathBuf::from(".")]
        };

        Ok(Cow::from(paths))
    }

    fn file_walker(&self) -> Result<WalkBuilder> {
        let paths = self.search_paths()?;

        let mut file_walker = WalkBuilder::new(&paths[0]);
        for path in &paths[1..] {
            file_walker.add(path);
        }

        // This is copied over from ripgrep, and seems to work well.
        file_walker.threads(std::cmp::min(12, num_cpus::get()));

        let should_ignore = !self.all_files;
        let should_show_hidden = self.hidden || self.all_files;
        file_walker
            .hidden(!should_show_hidden)
            .ignore(should_ignore)
            .git_ignore(should_ignore)
            .git_exclude(should_ignore)
            .parents(should_ignore);

        Ok(file_walker)
    }

    fn replacement_decider(&self) -> ReplacementDecider {
        if self.write {
            ReplacementDecider::constantly(ReplacementDecision::Accept)
        } else if self.dry_run {
            ReplacementDecider::constantly(ReplacementDecision::Ignore)
        } else {
            ReplacementDecider::with_prompt()
        }
    }

    fn searcher_builder(&self) -> SearcherBuilder {
        let mut searcher_builder = SearcherBuilder::new();
        searcher_builder
            .binary_detection(BinaryDetection::quit(0x00))
            .line_number(true)
            .before_context(
                self.before
                    .or(self.context)
                    .unwrap_or(DEFAULT_CONTEXT_LINES),
            )
            .after_context(self.after.or(self.context).unwrap_or(DEFAULT_CONTEXT_LINES));

        searcher_builder
    }

    fn match_printer(&self) -> MatchPrinterBuilder {
        MatchPrinterBuilder {
            print_mode: if self.quiet {
                MatchPrintMode::Silent
            } else if self.compact {
                MatchPrintMode::Compact
            } else {
                MatchPrintMode::Full
            },
            writes_enabled: self.write || self.is_interactive(),
        }
    }

    fn color_choice(&self) -> ColorChoice {
        match self.color {
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

    /// Whether to run with an interactive prompt
    fn is_interactive(&self) -> bool {
        !self.dry_run && !self.write
    }
}

struct FindAndReplacer {
    config: Config,
    file_walker: WalkBuilder,
    path_matcher: PathMatcher,
    match_printer: MatchPrinterBuilder,
    replacer_factory: ReplacerFactory,
    searcher_factory: RegexSearcherFactory,
}

const DEFAULT_CONTEXT_LINES: usize = 2;

impl FindAndReplacer {
    fn from_config(config: Config) -> Result<FindAndReplacer> {
        let regex_matcher = Arc::new(config.regex_matcher()?);

        // TODO: Confirm that template does not reference more capture groups than exist.
        let replacer_factory = ReplacerFactory::new(
            regex_matcher.clone(),
            Arc::new(config.replace.to_owned()),
            config.replacement_decider(),
        );

        let searcher_factory = RegexSearcherFactory::new(config.searcher_builder(), regex_matcher);

        Ok(FindAndReplacer {
            file_walker: config.file_walker()?,
            path_matcher: config.path_matcher()?,
            match_printer: config.match_printer(),
            searcher_factory,
            replacer_factory,

            config,
        })
    }

    fn run(&mut self) -> Result<()> {
        // We can only use parallel mode if we're not using the interactive prompt.
        if self.config.is_interactive() {
            self.run_with_prompt()
        } else {
            self.run_parallel()
        }
    }

    fn run_with_prompt(&mut self) -> Result<()> {
        let stats = Arc::new(Statistics::new());
        let start_time = Instant::now();

        let mut searcher = self.searcher_factory.build();
        let mut replacer = self.replacer_factory.build();

        let mut writer = StandardStream::stdout(self.config.color_choice());
        let mut match_printer = self.match_printer.build(&mut writer);

        for dir_entry in self.file_walker.build() {
            let _search_timer = stats.search_timer();

            let path = match dir_entry {
                Ok(ref entry) => {
                    if !self.path_matcher.should_search(entry) {
                        stats.visit_file(false);
                        continue;
                    }

                    entry.path()
                }

                Err(err) => {
                    eprintln!("error: {}", err);
                    continue;
                }
            };

            stats.visit_file(true);

            let matches = match searcher.search_path(path) {
                Ok(matches) => {
                    // No futher processing required for empty matches.
                    if matches.is_empty() {
                        continue;
                    }

                    stats.add_matches(matches.len());
                    matches
                }

                err => {
                    eprintln!("search failed: {:?}", err);
                    break;
                }
            };

            let mut should_quit = false;
            let num_replaced =
                replacer.consume_matches(path, matches, &mut match_printer, &mut should_quit);

            match num_replaced {
                Ok(num) => {
                    if num > 0 {
                        stats.add_replacements(num);
                    }
                }
                Err(err) => {
                    eprintln!("{}: {}", path.display(), err);
                    break;
                }
            }

            if should_quit {
                break;
            }
        }

        stats.add_elapsed_wall_time(start_time.elapsed());
        match_printer.display_footer(&stats)?;

        if self.config.print_stats {
            writeln!(&mut writer, "{}", stats)?;
        }

        Ok(())
    }

    fn run_parallel(&mut self) -> Result<()> {
        let writer = BufferWriter::stdout(self.config.color_choice());
        let stats = Arc::new(Statistics::new());
        let start_time = Instant::now();

        let file_walker = self.file_walker.build_parallel();
        file_walker.run(|| {
            let writer = &writer;
            let path_matcher = &self.path_matcher;
            let match_printer = &self.match_printer;
            let stats = &stats;

            let mut searcher = self.searcher_factory.build();
            let mut replacer = self.replacer_factory.build();

            Box::new(move |dir_entry| {
                let _search_timer = stats.search_timer();

                let path = match dir_entry {
                    Ok(ref entry) => {
                        if !path_matcher.should_search(entry) {
                            // TODO: this counts directories
                            stats.visit_file(false);
                            return WalkState::Continue;
                        }

                        entry.path()
                    }

                    Err(err) => {
                        eprintln!("error: {}", err);
                        return WalkState::Continue;
                    }
                };

                stats.visit_file(true);

                let matches = match searcher.search_path(path) {
                    Ok(matches) => {
                        // No futher processing required for empty matches.
                        if matches.is_empty() {
                            return WalkState::Continue;
                        }

                        stats.add_matches(matches.len());
                        matches
                    }

                    err => {
                        eprintln!("search failed: {:?}", err);
                        return WalkState::Quit;
                    }
                };

                let mut buffer = writer.buffer();
                let mut match_printer = match_printer.build(&mut buffer);

                let mut should_quit = false;
                let num_replaced =
                    replacer.consume_matches(path, matches, &mut match_printer, &mut should_quit);

                match num_replaced {
                    Ok(num) => {
                        if num > 0 {
                            stats.add_replacements(num);
                        }
                    }
                    Err(err) => {
                        eprintln!("{}: {}", path.display(), err);
                        return WalkState::Quit;
                    }
                }

                if let Err(err) = writer.print(&buffer) {
                    if err.kind() == io::ErrorKind::BrokenPipe {
                        return WalkState::Quit;
                    }
                    eprintln!("{}: {}", path.display(), err);
                }

                if should_quit {
                    WalkState::Quit
                } else {
                    WalkState::Continue
                }
            })
        });

        stats.add_elapsed_wall_time(start_time.elapsed());

        let mut buffer = writer.buffer();
        let mut match_printer = self.match_printer.build(&mut buffer);
        match_printer.display_footer(&stats)?;

        if self.config.print_stats {
            writeln!(&mut buffer, "{}", stats)?;
        }

        if let Err(err) = writer.print(&buffer) {
            if err.kind() != io::ErrorKind::BrokenPipe {
                return Err(anyhow!(err));
            }
        }

        Ok(())
    }
}

struct PathMatcher {
    included_paths: Option<RegexSet>,
    excluded_paths: RegexSet,
}

impl PathMatcher {
    fn should_search(&self, dir_entry: &DirEntry) -> bool {
        // Don't need to consider directories
        let is_file = dir_entry.file_type().map_or(false, |it| it.is_file());

        is_file && self.path_matches(dir_entry.path())
    }

    fn path_matches(&self, path: &Path) -> bool {
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
    let config = Config::parse();
    let mut find_and_replacer = FindAndReplacer::from_config(config)?;

    find_and_replacer.run()
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

            assert_eq!(matcher.path_matches(&Path::new("foo")), true);
        }

        #[test]
        fn test_included_set() {
            let allow_list: Vec<&str> = vec!["foo", "bar"];
            let disallow_list: Vec<&str> = vec![];

            let matcher = PathMatcher {
                included_paths: Some(as_regex_set(allow_list)),
                excluded_paths: as_regex_set(disallow_list),
            };

            assert_eq!(matcher.path_matches(&Path::new("foo.rs")), true);
            assert_eq!(matcher.path_matches(&Path::new("bar.rs")), true);
            assert_eq!(matcher.path_matches(&Path::new("baz.rs")), false);
        }

        #[test]
        fn test_excluded_set() {
            let disallow_list = vec!["foo", "bar"];
            let matcher = PathMatcher {
                included_paths: None,
                excluded_paths: as_regex_set(disallow_list),
            };

            assert_eq!(matcher.path_matches(&Path::new("foo.rs")), false);
            assert_eq!(matcher.path_matches(&Path::new("bar.rs")), false);
            assert_eq!(matcher.path_matches(&Path::new("baz.rs")), true);
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

            assert_eq!(matcher.path_matches(&Path::new("foo.rs")), true);
            assert_eq!(matcher.path_matches(&Path::new("bar.rs")), true);
            assert_eq!(matcher.path_matches(&Path::new("baz.rs")), false);
        }
    }
}
