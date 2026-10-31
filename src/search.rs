//! The card finder: parsing a query, matching it against a card, and ranking the matches.
//!
//! A query is a list of terms separated by spaces. A plain word matches fuzzily, a quoted phrase
//! matches as an exact case-insensitive substring, and `key:value` terms (`in:`, `board:`,
//! `column:`, `label:`, `color:` and `is:`) filter. A leading `-` turns any term into an
//! exclusion. Every term must hold for a card to match.
//!
//! # Fuzzy words
//!
//! A word matches a text when its characters appear in the text in order, case-insensitively, and
//! the first one sits at the start of a word of the text. A match with fewer skipped characters
//! scores better, and a match earlier in the text breaks a tie.
//!
//! # Ranking
//!
//! Matches sort by status (open, then done, then archived), then by the best field that matched
//! (title, description, subtasks, comments), then by score, then by name and id.

use std::collections::HashMap;
use std::mem::{Discriminant, discriminant};
use std::time::Instant;

use serde::Serialize;
use utoipa::ToSchema;

use crate::models::{BoardRow, ColorRow, ColumnRow, TaskRow};

/// Longest query accepted, in characters.
pub const MAX_QUERY_CHARS: usize = 500;

/// Most terms a query may hold.
pub const MAX_TERMS: usize = 20;

/// Names a part of a card a query can search, in the order a match in it ranks.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Field {
    /// The task name.
    Title,
    /// The task description.
    Description,
    /// The names of the task's subtasks.
    Subtasks,
    /// The bodies of the task's comments.
    Comments,
}

impl Field {
    /// Every field, in ranking order.
    const ALL: [Self; 4] = [
        Self::Title,
        Self::Description,
        Self::Subtasks,
        Self::Comments,
    ];

    /// Returns the field named by the folded `value`, or [`None`].
    fn parse(value: &str) -> Option<Self> {
        match value {
            "title" => Some(Self::Title),
            "description" => Some(Self::Description),
            "subtasks" => Some(Self::Subtasks),
            "comments" => Some(Self::Comments),
            _ => None,
        }
    }
}

/// Names where a card stands, in the order cards rank.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// The card sits in a column that is not a done column, on a board that is not archived.
    Open,
    /// The card sits in a done column of a board that is not archived.
    Done,
    /// The card sits on an archived board.
    Archived,
}

impl Status {
    /// Returns the status named by the folded `value`, or [`None`].
    fn parse(value: &str) -> Option<Self> {
        match value {
            "open" => Some(Self::Open),
            "done" => Some(Self::Done),
            "archived" => Some(Self::Archived),
            _ => None,
        }
    }
}

/// A run of characters in a text, as offsets counted in characters from its start.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, ToSchema)]
pub struct Range {
    /// Offset of the first matched character.
    pub start: usize,
    /// Offset one past the last matched character.
    pub end: usize,
}

/// A card that matched a query.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct Hit {
    /// The task id.
    pub task_id: String,
    /// The task name.
    pub name: String,
    /// The board the task sits on.
    pub board_id: String,
    /// The name of that board.
    pub board_name: String,
    /// The column the task sits in.
    pub column_id: String,
    /// The name of that column.
    pub column_name: String,
    /// Where the card stands: `open`, `done` or `archived`.
    pub status: Status,
    /// The best field the query matched: `title`, `description`, `subtasks` or `comments`.
    /// A query with no text to match reports `title`.
    pub matched_field: Field,
    /// For `subtasks` and `comments`, the position (from 0) of the matched subtask or comment in
    /// the task's own list, as the task detail returns it. Null for the other fields.
    pub matched_index: Option<usize>,
    /// The matched character ranges, counted in characters (not bytes) from the start of the
    /// matched text: the title, the description, the matched subtask's name or the matched
    /// comment's body. Sorted, without overlaps, and empty for a query with no text to match.
    pub ranges: Vec<Range>,
    /// How far into the text and how scattered the matches are, which ranks equal fields.
    #[serde(skip)]
    #[schema(ignore)]
    score: usize,
}

impl Hit {
    /// Sorts `hits` best first: open before done before archived, then by matched field, then by
    /// score, then by name and id.
    pub fn rank(hits: &mut [Self]) {
        hits.sort_by(|a, b| {
            a.status
                .cmp(&b.status)
                .then_with(|| a.matched_field.cmp(&b.matched_field))
                .then_with(|| a.score.cmp(&b.score))
                .then_with(|| a.name.cmp(&b.name))
                .then_with(|| a.task_id.cmp(&b.task_id))
        });
    }
}

/// A board with the lookups a card needs to be placed: its columns and colors.
#[derive(Debug)]
pub struct BoardView {
    /// The board's id.
    id: String,
    /// The board's name.
    name: String,
    /// Whether the board is archived.
    archived: bool,
    /// The columns by id.
    columns: HashMap<String, ColumnRow>,
    /// The colors by id.
    colors: HashMap<String, ColorRow>,
}

impl BoardView {
    /// Creates the view of `board` with its `columns` and `colors`.
    #[must_use]
    pub fn new(board: &BoardRow, columns: Vec<ColumnRow>, colors: Vec<ColorRow>) -> Self {
        Self {
            id: board.id.clone(),
            name: board.name.clone(),
            archived: board.config_bool("archived"),
            columns: columns
                .into_iter()
                .map(|column| (column.id.clone(), column))
                .collect(),
            colors: colors
                .into_iter()
                .map(|color| (color.id.clone(), color))
                .collect(),
        }
    }
}

/// How a term filters, with its value folded to lowercase.
#[derive(Clone, Debug)]
enum Filter {
    /// The board's id or name.
    Board(String),
    /// The column's id or name.
    Column(String),
    /// One of the task's labels.
    Label(String),
    /// The task color's id or label.
    Color(String),
    /// The card's status.
    Is(Status),
}

/// A filter, kept or inverted.
#[derive(Clone, Debug)]
struct Condition {
    /// Whether the filter excludes instead of keeps.
    negated: bool,
    /// The filter to apply.
    filter: Filter,
}

/// What a text term looks for, folded to lowercase.
#[derive(Clone, Debug)]
enum Needle {
    /// Characters that must appear in order, the first at the start of a word.
    Fuzzy(Vec<char>),
    /// Characters that must appear as one run.
    Phrase(Vec<char>),
}

/// A text term, kept or inverted.
#[derive(Clone, Debug)]
struct Word {
    /// Whether the term excludes instead of requires.
    negated: bool,
    /// What the term looks for.
    needle: Needle,
}

/// A parsed query.
#[derive(Clone, Debug)]
pub struct Query {
    /// The fields the text terms search.
    fields: Vec<Field>,
    /// The filters, all of which apply.
    conditions: Vec<Condition>,
    /// The text terms, all of which apply.
    words: Vec<Word>,
}

/// Folds a character to lowercase, one character for one, so offsets stay aligned.
fn fold(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// Folds every character of `text` to lowercase.
fn fold_str(text: &str) -> String {
    text.chars().map(fold).collect()
}

/// Folds every character of `text` to lowercase, as characters.
fn fold_chars(text: &str) -> Vec<char> {
    text.chars().map(fold).collect()
}

/// Splits `query` at whitespace outside quotes.
fn split_terms(query: &str) -> Result<Vec<String>, String> {
    let mut terms = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for c in query.chars() {
        if c == '"' {
            quoted = !quoted;
            current.push(c);
        } else if c.is_whitespace() && !quoted {
            if !current.is_empty() {
                terms.push(std::mem::take(&mut current));
            }
        } else {
            current.push(c);
        }
    }
    if quoted {
        return Err(format!("unclosed quote in {current}"));
    }
    if !current.is_empty() {
        terms.push(current);
    }
    Ok(terms)
}

/// Returns the text between the quotes of `quoted`, or [`None`] when it is not one quoted run.
fn unquote(quoted: &str) -> Option<&str> {
    let inner = quoted.strip_prefix('"')?.strip_suffix('"')?;
    (!inner.contains('"')).then_some(inner)
}

/// Returns whether `key` can name a filter: a letter, then letters, digits, `_` and `-`.
fn is_key(key: &str) -> bool {
    let mut chars = key.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

impl Query {
    /// Parses `text` into a query.
    ///
    /// # Errors
    ///
    /// Returns a message naming the term when the query is blank or too long, holds an unknown
    /// `key:value` key or a value the key does not accept, or breaks the quoting.
    pub fn parse(text: &str) -> Result<Self, String> {
        if text.chars().count() > MAX_QUERY_CHARS {
            return Err(format!("q is longer than {MAX_QUERY_CHARS} characters"));
        }
        let terms = split_terms(text)?;
        if terms.is_empty() {
            return Err("q has no terms".to_string());
        }
        if terms.len() > MAX_TERMS {
            return Err(format!("q has more than {MAX_TERMS} terms"));
        }
        let mut query = Self {
            fields: Vec::new(),
            conditions: Vec::new(),
            words: Vec::new(),
        };
        for term in &terms {
            query.add_term(term)?;
        }
        if query.fields.is_empty() {
            query.fields = Field::ALL.to_vec();
        }
        Ok(query)
    }

    /// Adds one whitespace-separated `term`, a word, a phrase or a filter.
    fn add_term(&mut self, term: &str) -> Result<(), String> {
        let (negated, body) = match term.strip_prefix('-') {
            Some(rest) if !rest.is_empty() => (true, rest),
            _ => (false, term),
        };
        if body.starts_with('"') {
            let phrase = unquote(body)
                .filter(|inner| !inner.trim().is_empty())
                .ok_or_else(|| format!("bad quoted phrase {term}"))?;
            self.words.push(Word {
                negated,
                needle: Needle::Phrase(fold_chars(phrase)),
            });
        } else if let Some((key, value)) = body.split_once(':')
            && is_key(key)
        {
            self.add_filter(term, negated, key, value)?;
        } else if body.contains('"') {
            return Err(format!("stray quote in {term}"));
        } else {
            self.words.push(Word {
                negated,
                needle: Needle::Fuzzy(fold_chars(body)),
            });
        }
        Ok(())
    }

    /// Adds the `key:value` filter or field choice `term` names.
    fn add_filter(
        &mut self,
        term: &str,
        negated: bool,
        key: &str,
        value: &str,
    ) -> Result<(), String> {
        let value = if value.starts_with('"') {
            unquote(value).ok_or_else(|| format!("bad quoted value in {term}"))?
        } else if value.contains('"') {
            return Err(format!("stray quote in {term}"));
        } else {
            value
        };
        let value = value.trim();
        if value.is_empty() {
            return Err(format!("{term} has no value"));
        }
        let folded = fold_str(value);
        let filter = match key.to_ascii_lowercase().as_str() {
            "in" => {
                if negated {
                    return Err(format!("{term} cannot be excluded"));
                }
                let field = Field::parse(&folded).ok_or_else(|| {
                    format!("{term} names no field: use title, description, subtasks or comments")
                })?;
                if !self.fields.contains(&field) {
                    self.fields.push(field);
                }
                return Ok(());
            }
            "board" => Filter::Board(folded),
            "column" => Filter::Column(folded),
            "label" => Filter::Label(folded),
            "color" => Filter::Color(folded),
            "is" => Filter::Is(
                Status::parse(&folded)
                    .ok_or_else(|| format!("{term} is not a status: use open, done or archived"))?,
            ),
            _ => {
                return Err(format!(
                    "unknown term {term}: use in:, board:, column:, label:, color: or is:"
                ));
            }
        };
        self.conditions.push(Condition { negated, filter });
        Ok(())
    }

    /// Returns the [`Hit`] for `task` on `board` when the task matches, or [`None`].
    ///
    /// # Errors
    ///
    /// Returns [`TimedOut`] when `deadline` passes before the card's texts are read.
    pub fn find(
        &self,
        board: &BoardView,
        task: &TaskRow,
        deadline: Instant,
    ) -> Result<Option<Hit>, TimedOut> {
        let Some(column) = board.columns.get(&task.column_id) else {
            return Ok(None);
        };
        let status = if board.archived {
            Status::Archived
        } else if column.is_done {
            Status::Done
        } else {
            Status::Open
        };
        if !self.passes_filters(board, column, status, task) {
            return Ok(None);
        }
        let Some(found) = self.match_text(task, deadline)? else {
            return Ok(None);
        };
        Ok(Some(Hit {
            task_id: task.id.clone(),
            name: task.name.clone(),
            board_id: board.id.clone(),
            board_name: board.name.clone(),
            column_id: column.id.clone(),
            column_name: column.name.clone(),
            status,
            matched_field: found.field,
            matched_index: found.index,
            ranges: found.ranges,
            score: found.score,
        }))
    }

    /// Returns whether the card passes every filter.
    fn passes_filters(
        &self,
        board: &BoardView,
        column: &ColumnRow,
        status: Status,
        task: &TaskRow,
    ) -> bool {
        let is_named =
            |wanted: &str, id: &str, name: &str| wanted == fold_str(id) || wanted == fold_str(name);
        let holds = |filter: &Filter| match filter {
            Filter::Board(wanted) => is_named(wanted, &board.id, &board.name),
            Filter::Column(wanted) => is_named(wanted, &column.id, &column.name),
            Filter::Label(wanted) => task.labels.iter().any(|label| fold_str(label) == *wanted),
            Filter::Color(wanted) => task
                .color_id
                .as_ref()
                .and_then(|id| board.colors.get(id))
                .is_some_and(|color| is_named(wanted, &color.id, &color.label)),
            Filter::Is(wanted) => *wanted == status,
        };
        let mut alternatives: HashMap<Discriminant<Filter>, bool> = HashMap::new();
        for condition in &self.conditions {
            let kept = holds(&condition.filter);
            if condition.negated {
                if kept {
                    return false;
                }
            } else if matches!(condition.filter, Filter::Label(_)) {
                if !kept {
                    return false;
                }
            } else {
                *alternatives
                    .entry(discriminant(&condition.filter))
                    .or_insert(false) |= kept;
            }
        }
        alternatives.values().all(|&any| any)
    }

    /// Matches the text terms against the card's texts, or returns [`None`] when they fail.
    fn match_text(&self, task: &TaskRow, deadline: Instant) -> Result<Option<Found>, TimedOut> {
        if self.words.is_empty() {
            return Ok(Some(Found::none()));
        }
        let positives: Vec<&Word> = self.words.iter().filter(|word| !word.negated).collect();
        let segments = self.segments(task, deadline)?;
        for word in self.words.iter().filter(|word| word.negated) {
            if segments
                .iter()
                .any(|segment| locate(&word.needle, &segment.chars).is_some())
            {
                return Ok(None);
            }
        }
        if positives.is_empty() {
            return Ok(Some(Found::none()));
        }
        let mut bests = Vec::with_capacity(positives.len());
        for word in &positives {
            let best = segments
                .iter()
                .filter_map(|segment| {
                    locate(&word.needle, &segment.chars).map(|located| (segment, located))
                })
                .min_by_key(|(segment, located)| (segment.field, located.gaps, located.start));
            let Some(best) = best else {
                return Ok(None);
            };
            bests.push(best);
        }
        let Some((segment, _)) = bests
            .iter()
            .min_by_key(|(segment, located)| (segment.field, located.gaps, located.start))
        else {
            return Ok(None);
        };
        let mut ranges = Vec::new();
        for word in &positives {
            if let Some(located) = locate(&word.needle, &segment.chars) {
                ranges.extend(located.ranges);
            }
        }
        Ok(Some(Found {
            field: segment.field,
            index: segment.index,
            ranges: merge_ranges(ranges),
            score: bests
                .iter()
                .map(|(_, located)| located.gaps + located.start)
                .sum(),
        }))
    }

    /// Returns the folded texts of the searched fields, or [`TimedOut`] past `deadline`.
    fn segments(&self, task: &TaskRow, deadline: Instant) -> Result<Vec<Segment>, TimedOut> {
        let mut segments = Vec::new();
        for &field in &Field::ALL {
            if !self.fields.contains(&field) {
                continue;
            }
            match field {
                Field::Title => segments.push(Segment::new(field, None, &task.name)),
                Field::Description => {
                    segments.push(Segment::new(field, None, &task.description));
                }
                Field::Subtasks => {
                    for (index, subtask) in task.subtasks.iter().enumerate() {
                        check(deadline)?;
                        segments.push(Segment::new(field, Some(index), &subtask.name));
                    }
                }
                Field::Comments => {
                    for (index, comment) in task.comments.iter().enumerate() {
                        check(deadline)?;
                        segments.push(Segment::new(field, Some(index), &comment.body));
                    }
                }
            }
        }
        Ok(segments)
    }
}

/// The time budget ran out before a card was read.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct TimedOut;

/// Returns [`TimedOut`] once `deadline` has passed.
fn check(deadline: Instant) -> Result<(), TimedOut> {
    if Instant::now() >= deadline {
        return Err(TimedOut);
    }
    Ok(())
}

/// One searchable text of a card, folded.
struct Segment {
    /// The field the text belongs to.
    field: Field,
    /// The position of the subtask or comment in the task's list, or none.
    index: Option<usize>,
    /// The folded text.
    chars: Vec<char>,
}

impl Segment {
    /// Folds `text` into a segment of `field`.
    fn new(field: Field, index: Option<usize>, text: &str) -> Self {
        Self {
            field,
            index,
            chars: fold_chars(text),
        }
    }
}

/// What a card's text matched.
struct Found {
    /// The best field matched.
    field: Field,
    /// The position of the matched subtask or comment, or none.
    index: Option<usize>,
    /// The matched ranges in the matched text.
    ranges: Vec<Range>,
    /// The sum of the matches' gaps and starts, lower is better.
    score: usize,
}

impl Found {
    /// Returns the result for a query with no text to match.
    fn none() -> Self {
        Self {
            field: Field::Title,
            index: None,
            ranges: Vec::new(),
            score: 0,
        }
    }
}

/// Where a needle sits in a text.
struct Located {
    /// Characters skipped between the first and last matched character.
    gaps: usize,
    /// Offset of the first matched character.
    start: usize,
    /// The matched runs, sorted.
    ranges: Vec<Range>,
}

/// Finds the best place of `needle` in the folded `text`.
fn locate(needle: &Needle, text: &[char]) -> Option<Located> {
    match needle {
        Needle::Phrase(phrase) => {
            let start = text
                .windows(phrase.len())
                .position(|window| window == phrase.as_slice())?;
            Some(Located {
                gaps: 0,
                start,
                ranges: vec![Range {
                    start,
                    end: start + phrase.len(),
                }],
            })
        }
        Needle::Fuzzy(word) => locate_fuzzy(word, text),
    }
}

/// Finds the match of `word` in `text` that skips the fewest characters, trying each word start
/// where the first character matches.
fn locate_fuzzy(word: &[char], text: &[char]) -> Option<Located> {
    let (&first, rest) = word.split_first()?;
    let mut best: Option<Located> = None;
    for start in 0..text.len() {
        let at_word_start = start == 0 || !text[start - 1].is_alphanumeric();
        if text[start] != first || !at_word_start {
            continue;
        }
        let mut positions = vec![start];
        let mut previous = start;
        for &wanted in rest {
            let Some(offset) = text[previous + 1..].iter().position(|&c| c == wanted) else {
                return best;
            };
            previous += 1 + offset;
            positions.push(previous);
        }
        let gaps = previous + 1 - start - word.len();
        if best.as_ref().is_none_or(|found| gaps < found.gaps) {
            best = Some(Located {
                gaps,
                start,
                ranges: runs(&positions),
            });
        }
        if gaps == 0 {
            break;
        }
    }
    best
}

/// Turns ascending matched offsets into the ranges of their consecutive runs.
fn runs(positions: &[usize]) -> Vec<Range> {
    let mut ranges: Vec<Range> = Vec::new();
    for &position in positions {
        match ranges.last_mut() {
            Some(last) if last.end == position => last.end += 1,
            _ => ranges.push(Range {
                start: position,
                end: position + 1,
            }),
        }
    }
    ranges
}

/// Sorts `ranges` and joins the ones that overlap or touch.
fn merge_ranges(mut ranges: Vec<Range>) -> Vec<Range> {
    ranges.sort_by_key(|range| (range.start, range.end));
    let mut merged: Vec<Range> = Vec::new();
    for range in ranges {
        match merged.last_mut() {
            Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
            _ => merged.push(range),
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    type LocatedTuple = (usize, usize, Vec<(usize, usize)>);

    fn located(word: &str, text: &str) -> Option<LocatedTuple> {
        locate(&Needle::Fuzzy(fold_chars(word)), &fold_chars(text)).map(|found| {
            (
                found.gaps,
                found.start,
                found.ranges.iter().map(|r| (r.start, r.end)).collect(),
            )
        })
    }

    fn error_of(text: &str) -> String {
        Query::parse(text).expect_err("a refused query")
    }

    #[test]
    fn fuzzy_should_match_the_first_letter_of_a_word() {
        assert!(located("dep", "Deploy to staging").is_some());
        assert!(located("dpl", "Deploy to staging").is_some());
        assert!(located("tst", "Run the tests").is_some());
    }

    #[test]
    fn fuzzy_should_refuse_a_match_that_starts_inside_a_word() {
        assert!(located("ploy", "Deploy to staging").is_none());
        assert!(located("zz", "Deploy").is_none());
    }

    #[test]
    fn fuzzy_should_match_a_later_word_start() {
        assert_eq!(
            located("st", "Deploy to staging"),
            Some((0, 10, vec![(10, 12)]))
        );
    }

    #[test]
    fn fuzzy_should_prefer_the_contiguous_match() {
        let (gaps, start, ranges) = located("ab", "a xb ab").expect("a match");
        assert_eq!((gaps, start), (0, 5));
        assert_eq!(ranges, vec![(5, 7)]);
    }

    #[test]
    fn fuzzy_should_report_runs_of_matched_characters() {
        let (gaps, _, ranges) = located("dpl", "deploy").expect("a match");
        assert_eq!(gaps, 1);
        assert_eq!(ranges, vec![(0, 1), (2, 4)]);
    }

    #[test]
    fn fuzzy_should_count_offsets_in_characters() {
        let (_, start, ranges) = located("b", "éé b").expect("a match");
        assert_eq!(start, 3);
        assert_eq!(ranges, vec![(3, 4)]);
    }

    #[test]
    fn parse_should_refuse_a_blank_query() {
        assert!(error_of("").contains("no terms"));
        assert!(error_of("   ").contains("no terms"));
    }

    #[test]
    fn parse_should_name_an_unknown_key() {
        assert!(error_of("foo:bar").contains("foo:bar"));
        assert!(error_of("-foo:bar").contains("-foo:bar"));
    }

    #[test]
    fn parse_should_name_a_bad_value() {
        assert!(error_of("is:foo").contains("is:foo"));
        assert!(error_of("in:foo").contains("in:foo"));
        assert!(error_of("label:").contains("label:"));
        assert!(error_of("-in:title").contains("-in:title"));
    }

    #[test]
    fn parse_should_refuse_broken_quotes() {
        assert!(error_of("\"open").contains("unclosed"));
        assert!(error_of("a\"b").contains("a\"b"));
        assert!(error_of("\"\"").contains("\"\""));
    }

    #[test]
    fn parse_should_take_a_quoted_value() {
        let query = Query::parse("board:\"My Board\" label:\"two words\"").expect("a query");
        assert_eq!(query.conditions.len(), 2);
    }

    #[test]
    fn parse_should_read_a_time_as_a_word() {
        let query = Query::parse("10:30").expect("a query");
        assert_eq!(query.words.len(), 1);
    }

    #[test]
    fn parse_should_cap_the_terms() {
        let many = "a ".repeat(MAX_TERMS + 1);
        assert!(error_of(&many).contains("terms"));
    }

    #[test]
    fn parse_should_search_every_field_unless_in_narrows_it() {
        assert_eq!(Query::parse("x").expect("a query").fields, Field::ALL);
        let query = Query::parse("x in:comments in:title in:title").expect("a query");
        assert_eq!(query.fields, vec![Field::Comments, Field::Title]);
    }

    #[test]
    fn merge_should_join_touching_ranges() {
        let ranges = merge_ranges(vec![
            Range { start: 4, end: 6 },
            Range { start: 0, end: 2 },
            Range { start: 2, end: 3 },
            Range { start: 5, end: 8 },
        ]);
        assert_eq!(
            ranges,
            vec![Range { start: 0, end: 3 }, Range { start: 4, end: 8 }]
        );
    }

    #[test]
    fn phrase_should_match_as_a_case_insensitive_substring() {
        let needle = Needle::Phrase(fold_chars("Ploy To"));
        let found = locate(&needle, &fold_chars("Deploy to staging")).expect("a match");
        assert_eq!(found.ranges, vec![Range { start: 2, end: 9 }]);
    }
}
