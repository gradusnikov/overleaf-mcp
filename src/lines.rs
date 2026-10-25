//! Line selection for long text outputs such as TeX logs: grep-like matching with context, then paging.

use anyhow::{Result, anyhow};
use regex::{Regex, RegexBuilder};

pub const DEFAULT_LIMIT: usize = 400;
pub const MAX_LIMIT: usize = 5000;
pub const MAX_CONTEXT: usize = 20;

pub struct LineFilter {
    pub pattern: Option<Regex>,
    pub context: usize,
    /// Number of selected lines to skip; negative counts from the end.
    pub offset: i64,
    pub limit: usize,
}

impl LineFilter {
    pub fn new(pattern: Option<&str>, context: Option<u32>, offset: Option<i64>, limit: Option<u32>) -> Result<Self> {
        let pattern = pattern
            .filter(|p| !p.is_empty())
            .map(|p| {
                RegexBuilder::new(p)
                    .size_limit(1 << 20)
                    .build()
                    .map_err(|e| anyhow!("invalid pattern: {e}"))
            })
            .transpose()?;
        Ok(Self {
            pattern,
            context: (context.unwrap_or(0) as usize).min(MAX_CONTEXT),
            offset: offset.unwrap_or(0),
            limit: (limit.map(|l| l as usize).unwrap_or(DEFAULT_LIMIT)).clamp(1, MAX_LIMIT),
        })
    }

    /// Returns a header describing the selection, followed by the selected lines prefixed with their
    /// line numbers; with a pattern, non-adjacent groups are separated by `--`.
    pub fn apply(&self, name: &str, text: &str) -> String {
        let lines: Vec<&str> = text.lines().collect();
        let mut matches = 0;
        let selected: Vec<usize> = match &self.pattern {
            None => (0..lines.len()).collect(),
            Some(re) => {
                let mut keep = vec![false; lines.len()];
                for (i, line) in lines.iter().enumerate() {
                    if re.is_match(line) {
                        matches += 1;
                        let end = (i + self.context).min(lines.len() - 1);
                        keep[i.saturating_sub(self.context)..=end].fill(true);
                    }
                }
                (0..lines.len()).filter(|&i| keep[i]).collect()
            }
        };
        let total = selected.len();
        let start = if self.offset < 0 {
            total.saturating_sub(self.offset.unsigned_abs() as usize)
        } else {
            (self.offset as usize).min(total)
        };
        let end = (start + self.limit).min(total);

        let mut header = format!("{name}: {} lines", lines.len());
        if let Some(re) = &self.pattern {
            header += &format!(", {matches} matching /{re}/");
            if self.context > 0 {
                header += &format!(" ({total} lines with context)");
            }
        }
        if start < end {
            header += &format!("; showing {}-{} of {total}", start + 1, end);
        } else {
            header += "; nothing to show";
        }
        if end < total {
            header += &format!("; more with offset={end}");
        }

        let width = lines.len().to_string().len();
        let mut out = header;
        let mut previous: Option<usize> = None;
        for &i in &selected[start..end] {
            if self.pattern.is_some() && previous.is_some_and(|p| i > p + 1) {
                out += "\n--";
            }
            out += &format!("\n{:>width$}: {}", i + 1, lines[i]);
            previous = Some(i);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOG: &str = "a\nb Warning: one\nc\nd\ne\nf Warning: two\ng";

    #[test]
    fn unfiltered_is_paged() {
        let f = LineFilter::new(None, None, Some(2), Some(2)).unwrap();
        assert_eq!(
            f.apply("x.log", LOG),
            "x.log: 7 lines; showing 3-4 of 7; more with offset=4\n3: c\n4: d"
        );
    }

    #[test]
    fn negative_offset_is_a_tail() {
        let f = LineFilter::new(None, None, Some(-2), None).unwrap();
        assert_eq!(
            f.apply("x.log", LOG),
            "x.log: 7 lines; showing 6-7 of 7\n6: f Warning: two\n7: g"
        );
    }

    #[test]
    fn matches_with_context_are_grouped() {
        let f = LineFilter::new(Some("Warning"), Some(1), None, None).unwrap();
        assert_eq!(
            f.apply("x.log", LOG),
            "x.log: 7 lines, 2 matching /Warning/ (6 lines with context); showing 1-6 of 6\n\
             1: a\n2: b Warning: one\n3: c\n--\n5: e\n6: f Warning: two\n7: g"
        );
    }

    #[test]
    fn no_match_and_bad_pattern() {
        let f = LineFilter::new(Some("(?i)ERROR"), None, None, None).unwrap();
        assert_eq!(
            f.apply("x.log", LOG),
            "x.log: 7 lines, 0 matching /(?i)ERROR/; nothing to show"
        );
        assert!(LineFilter::new(Some("("), None, None, None).is_err());
    }
}
