use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use grep::regex::RegexMatcher;
use grep::searcher::{SinkContext, SinkContextKind, SinkMatch};

#[derive(Debug, Clone)]
pub struct Line(pub u64, pub String);

#[derive(Debug)]
pub struct Match {
    pub line: Line,
    pub context_pre: Vec<Line>,
    pub context_post: Vec<Line>,
}

#[derive(Debug, Copy, Clone, PartialEq)]
enum MatchState {
    Before,
    Match,
    After,
}

#[derive(Debug)]
struct MatchCollector {
    state: MatchState,

    cur_context_pre: Vec<Line>,
    cur_context_post: Vec<Line>,
    cur_match_line: Option<Line>,

    matches: Vec<Match>,
}

impl MatchCollector {
    fn new() -> MatchCollector {
        MatchCollector {
            state: MatchState::Before,
            cur_match_line: None,
            cur_context_pre: Vec::new(),
            cur_context_post: Vec::new(),
            matches: Vec::new(),
        }
    }

    fn maybe_emit(&mut self) {
        let mut cur_match_line = None;
        std::mem::swap(&mut cur_match_line, &mut self.cur_match_line);

        if let Some(line) = cur_match_line {
            let mut context_pre = vec![];
            let mut context_post = vec![];
            std::mem::swap(&mut context_pre, &mut self.cur_context_pre);
            std::mem::swap(&mut context_post, &mut self.cur_context_post);

            let search_match = Match {
                line,
                context_pre,
                context_post,
            };

            self.matches.push(search_match);
            self.state = MatchState::Before;
        }
    }

    #[inline]
    fn transition(&mut self, next: MatchState) {
        match (self.state, next) {
            // Beginning a new match or ending a previous one
            (MatchState::Match, MatchState::Before)       // Have before context, no after context
            | (MatchState::Match, MatchState::Match)      // No before context, no after context
            | (MatchState::After, MatchState::Before)     // Have before context, have after context
            | (MatchState::After, MatchState::Match) => { // Have after context, no before context
                self.maybe_emit();
            }

            (_prev, next) => {
                self.state = next;
            }
        }
    }

    fn collect(&mut self) -> Vec<Match> {
        self.maybe_emit();

        let mut matches = vec![];
        std::mem::swap(&mut self.matches, &mut matches);

        matches
    }
}

impl grep::searcher::Sink for MatchCollector {
    type Error = std::io::Error;

    fn matched(
        &mut self,
        _searcher: &grep::searcher::Searcher,
        mat: &SinkMatch<'_>,
    ) -> Result<bool, std::io::Error> {
        self.transition(MatchState::Match);

        let line = Line(
            mat.line_number().unwrap(),
            String::from_utf8_lossy(mat.bytes()).to_string(),
        );

        self.cur_match_line = Some(line);

        Ok(true)
    }

    fn context(
        &mut self,
        _searcher: &grep::searcher::Searcher,
        ctx: &SinkContext<'_>,
    ) -> Result<bool, std::io::Error> {
        let line = Line(
            ctx.line_number().unwrap(),
            String::from_utf8_lossy(ctx.bytes()).to_string(),
        );

        match *ctx.kind() {
            SinkContextKind::Before => {
                self.transition(MatchState::Before);
                self.cur_context_pre.push(line);
            }
            SinkContextKind::After => {
                self.transition(MatchState::After);
                self.cur_context_post.push(line);
            }
            SinkContextKind::Other => {}
        }

        Ok(true)
    }
}

pub struct RegexSearcher {
    searcher: grep::searcher::Searcher,
    matcher: Arc<RegexMatcher>,
}

impl RegexSearcher {
    pub fn search_path(&mut self, path: &'_ Path) -> Result<Vec<Match>> {
        let mut collector = MatchCollector::new();

        self.searcher
            .search_path(self.matcher.as_ref(), path, &mut collector)?;

        let matches = collector.collect();
        Ok(matches)
    }
}

pub struct RegexSearcherFactory {
    searcher_builder: grep::searcher::SearcherBuilder,
    pattern_matcher: Arc<RegexMatcher>,
}

impl RegexSearcherFactory {
    pub fn new(
        searcher_builder: grep::searcher::SearcherBuilder,
        pattern_matcher: Arc<RegexMatcher>,
    ) -> RegexSearcherFactory {
        RegexSearcherFactory {
            searcher_builder,
            pattern_matcher,
        }
    }

    pub fn build(&self) -> RegexSearcher {
        RegexSearcher {
            searcher: self.searcher_builder.build(),
            matcher: self.pattern_matcher.clone(),
        }
    }
}
