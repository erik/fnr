use std::io::Write;
use std::path::Path;

use anyhow::Result;
use termcolor::{ColorSpec, WriteColor};

use crate::search::Match;

#[derive(Copy, Clone)]
pub enum MatchPrintMode {
    Silent,
    Compact,
    Full,
}

pub struct MatchPrinterBuilder {
    pub print_mode: MatchPrintMode,
    pub writes_enabled: bool,
}

impl MatchPrinterBuilder {
    pub fn build<'a, W: WriteColor>(&self, writer: &'a mut W) -> MatchPrinter<'a, W> {
        MatchPrinter {
            writer,
            print_mode: self.print_mode,
            writes_enabled: self.writes_enabled,
            last_line_num: None,
        }
    }
}

pub struct MatchPrinter<'a, W: WriteColor> {
    writer: &'a mut W,

    print_mode: MatchPrintMode,
    writes_enabled: bool,
    last_line_num: Option<u64>,
}

impl<'a, W: WriteColor> MatchPrinter<'a, W> {
    pub fn display_header(&mut self, path: &Path, num_matches: usize) -> Result<()> {
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

    pub fn display_match(
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

    pub fn display_footer(
        &mut self,
        total_replacements: usize,
        total_matches: usize,
    ) -> Result<()> {
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
