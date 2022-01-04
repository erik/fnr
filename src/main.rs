use std::borrow::Cow;
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{ensure, Context, Result};
use atty::Stream;
use grep::regex::{RegexMatcher, RegexMatcherBuilder};
use grep::searcher::{BinaryDetection, SearcherBuilder};
use ignore::{WalkBuilder, WalkState};
use regex::RegexSet;
use structopt::clap::arg_enum;
use structopt::StructOpt;
use termcolor::{BufferWriter, ColorChoice};

mod printer;
mod replace;
mod search;

use crate::printer::{MatchPrintMode, MatchPrinterBuilder};
use crate::replace::{ReplacementDecider, ReplacementDecision, ReplacerFactory};
use crate::search::RegexSearcherFactory;

arg_enum! {
    #[derive(Debug)]
    enum ColorPreference {
        Always,
        Auto,
        Never
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
//
// /// Match FIND only at word boundary.
// #[structopt(short, long)]
// word: bool
pub struct Config {
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
                !self.prompt,
                "cannot use --prompt when reading files from stdin"
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
        if self.prompt {
            ReplacementDecider::with_prompt()
        } else if self.write {
            ReplacementDecider::constantly(ReplacementDecision::Accept)
        } else {
            ReplacementDecider::constantly(ReplacementDecision::Ignore)
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
            writes_enabled: self.write || self.prompt,
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

    fn run_with_prompt(&mut self) -> Result<()> {
        // TODO: Write single threaded variant once the basic data
        // structures are stable.
        Ok(())
    }

    fn run_parallel(&mut self) -> Result<()> {
        let writer = BufferWriter::stdout(self.config.color_choice());

        let file_walker = self.file_walker.build_parallel();
        file_walker.run(|| {
            let writer = &writer;
            let path_matcher = &self.path_matcher;
            let match_printer = &self.match_printer;

            let mut searcher = self.searcher_factory.build();
            let mut replacer = self.replacer_factory.build();

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

                let mut buffer = writer.buffer();
                let mut match_printer = match_printer.build(&mut buffer);

                let should_proceed = replacer.consume_matches(path, matches, &mut match_printer);

                if let Err(err) = writer.print(&buffer) {
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
