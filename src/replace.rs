use std::borrow::Cow;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use grep::matcher::{Captures, Matcher};
use grep::regex::RegexMatcher;
use termcolor::WriteColor;
use text_io::read;

use crate::printer::MatchPrinter;
use crate::search::Match;

struct MatchReplacement<'a> {
    search_match: Match,
    replacement: Cow<'a, str>,
}

pub struct ReplacerFactory {
    regex_matcher: Arc<RegexMatcher>,
    replacement_template: Arc<String>,
    replacement_decider: ReplacementDecider,
}

impl ReplacerFactory {
    pub fn new(
        regex_matcher: Arc<RegexMatcher>,
        replacement_template: Arc<String>,
        replacement_decider: ReplacementDecider,
    ) -> ReplacerFactory {
        ReplacerFactory {
            regex_matcher,
            replacement_template,
            replacement_decider,
        }
    }

    pub fn build(&self) -> Replacer {
        Replacer {
            // These clones are basically free due to Arc
            regex_matcher: self.regex_matcher.clone(),
            replacement_template: self.replacement_template.clone(),

            // This one isn't but is small.
            replacement_decider: self.replacement_decider.clone(),
        }
    }
}

#[derive(Clone)]
pub enum ReplacementDecider {
    Constantly(ReplacementDecision),
    WithPrompt {
        local_decision: Option<ReplacementDecision>,
    },
}

impl ReplacementDecider {
    pub fn constantly(decision: ReplacementDecision) -> ReplacementDecider {
        ReplacementDecider::Constantly(decision)
    }

    pub fn with_prompt() -> ReplacementDecider {
        ReplacementDecider::WithPrompt {
            local_decision: None,
        }
    }

    fn decide(&mut self) -> ReplacementDecision {
        match self {
            Self::Constantly(decision) => *decision,
            Self::WithPrompt { local_decision } => loop {
                let line = read_input("Stage this replacement [y,n,q,a,e,d,?] ").unwrap();

                return match line.as_str() {
                    "y" => ReplacementDecision::Accept,
                    "n" => ReplacementDecision::Ignore,
                    "q" => ReplacementDecision::Terminate,
                    "a" => {
                        *local_decision = Some(ReplacementDecision::Accept);
                        ReplacementDecision::Accept
                    }
                    "d" => {
                        *local_decision = Some(ReplacementDecision::Ignore);
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
            },
        }
    }

    fn reset(&mut self) {
        match self {
            Self::Constantly(_) => (),
            Self::WithPrompt { local_decision } => {
                *local_decision = None;
            }
        }
    }
}

pub struct Replacer {
    regex_matcher: Arc<RegexMatcher>,
    replacement_template: Arc<String>,
    replacement_decider: ReplacementDecider,
}

impl Replacer {
    pub fn consume_matches<W: WriteColor>(
        &mut self,
        path: &Path,
        matches: Vec<Match>,
        match_printer: &mut MatchPrinter<W>,
    ) -> Result<bool> {
        if matches.is_empty() {
            return Ok(true);
        }

        match_printer.display_header(path, matches.len())?;

        self.replacement_decider.reset();

        // TODO: filter-map
        let mut replacement_list = Vec::with_capacity(matches.len());
        for m in matches.into_iter() {
            let replacement = self.replace_with_captures(&m.line.1)?;
            match_printer.display_match(path, &m, &replacement)?;

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
                    match_printer.display_match(path, &m, &line)?;
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

    fn replace_with_captures(&self, input: &str) -> Result<String> {
        let mut caps = self.regex_matcher.new_captures().unwrap();
        let mut dst = vec![];

        self.regex_matcher.replace_with_captures(
            input.as_bytes(),
            &mut caps,
            &mut dst,
            |caps, dst| {
                caps.interpolate(
                    |name| self.regex_matcher.capture_index(name),
                    input.as_bytes(),
                    self.replacement_template.as_bytes(),
                    dst,
                );
                true
            },
        )?;

        Ok(String::from_utf8_lossy(&dst).to_string())
    }

    fn finalize(&self) -> Result<()> {
        Ok(())
        // match_printer
        //     .display_footer(self.total_replacements, self.total_matches)
    }
}

#[derive(Debug, Copy, Clone)]
pub enum ReplacementDecision {
    Accept,
    Ignore,
    Edit,
    Terminate,
}

// TODO: global mutex.
fn read_input(prompt: &str) -> Result<String, std::io::Error> {
    print!("{}", prompt);
    std::io::stdout().flush()?;

    Ok(read!("{}\n"))
}