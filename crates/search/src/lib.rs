//! Search query parser and FTS bindings.
//!
//! UI, MCP, and shortcuts talk to Core. This crate does not import GTK.
//!
//! This crate has **no dependency on any other workspace crate** and no
//! external dependency at all (see `Cargo.toml`). That means it cannot know
//! the caller's timezone, the signed-in user's own address, or how the
//! database layer names its tables. Those decisions are pushed onto the
//! caller on purpose (see the doc comments on [`Addressee`], [`Date`], and
//! [`SearchPlan`]).
//!
//! # What this crate does
//!
//! [`Query::parse`] turns a raw search string (as typed by a human into the
//! GTK search field, or as sent verbatim by an MCP client) into a structured
//! [`Query`]. [`Query::to_search_plan`] then turns that structure into a
//! [`SearchPlan`]: an FTS5 `MATCH` argument (already escaped, safe to splice
//! into SQL literally) plus a list of [`Predicate`]s the caller must apply
//! itself (unread/starred/read flags, attachment presence, recipient, date
//! range). This crate never touches SQLite, never executes anything, and
//! never formats anything for a human to read — see D54: "один парсер для
//! UI и MCP".
//!
//! # Query language (D54)
//!
//! - Bare words and `"quoted phrases"` search across all indexed columns.
//! - `from:`, `to:`, `subject:` scope a value to one field. The value may
//!   itself be a bare word or a `"quoted phrase"`.
//! - `is:unread`, `is:starred`, `is:read` filter on message state.
//! - `has:attachment` filters on attachment presence.
//! - `after:YYYY-MM-DD`, `before:YYYY-MM-DD` filter on date.
//! - Unknown operators (e.g. `foo:bar`) and malformed/incomplete operator
//!   values (e.g. `after:2026-13-45`, typed mid-keystroke as `after:2`) are
//!   **not errors** — the whole token is treated as a literal bare-word
//!   search term instead. A user typing character by character must never
//!   see the parser panic or silently drop a filter.
//! - Multiple tokens combine with **AND**, including repeated operators:
//!   `from:a from:b` requires both, it does not turn into OR.
//! - Negation (`-term`), boolean `OR`, and parentheses are **out of scope**
//!   for D54 and are deliberately not implemented; such input is treated as
//!   literal text (see the tests in this module).
//! - An empty query (or one that is only whitespace) means "show
//!   everything", not "match nothing" — `to_search_plan` returns
//!   `fts_match: None` for it, since `MATCH ''` is an FTS5 syntax error.
//!
//! # Index/query normalization (T-155)
//!
//! [`fts_text`] is the one normalization the index and the query must
//! agree on before FTS5's `unicode61` tokenizer sees either of them: it
//! folds Cyrillic `ё` to `е` and puts a space at every boundary touching a
//! CJK character. It lives here, next to the parser, precisely so there is
//! a single definition for both sides — see its doc comment.

/// Workspace probe so `cargo test -p feathermail-search` has a test even
/// before the parser existed. Kept for continuity with earlier scaffolding.
pub fn crate_name() -> &'static str {
    env!("CARGO_PKG_NAME")
}

/// Normalize text so that the FTS5 `unicode61` tokenizer produces the
/// tokens a human expects (T-155).
///
/// **This is one half of a pair.** The exact same function must be applied
/// to the text that goes *into* `messages_fts` and to the text of every
/// term/phrase that comes *out* of [`Query::to_search_plan`]. Applying it
/// on one side only silently breaks search rather than erroring: the index
/// holds one spelling and the query asks for another, and FTS5 answers
/// "no rows". Core calls it in its indexer (`crates/core/src/search.rs`,
/// `index_one`); this crate calls it in `escape_fts_literal`. There is no
/// third caller, and a new writer of `messages_fts` must use it too.
///
/// It fixes exactly two things `unicode61` gets wrong for this mailbox:
///
/// 1. **Cyrillic `ё`.** `unicode61` folds case (so `СЧЁТ` is findable as
///    `счёт`) but `ё` and `е` stay two different letters, so a user typing
///    `счет` never finds `СЧЁТ`. Russian writing treats the two as
///    interchangeable, so both sides fold `ё`/`Ё` to `е`/`Е`.
/// 2. **CJK runs.** `unicode61` splits on non-alphanumeric characters, and
///    every Han ideograph and kana sign is "alphanumeric" to it, so a whole
///    Japanese sentence written without spaces is *one* token. D54 searches
///    for a literal phrase (no `*`), so `東京` inside
///    `今日の会議は東京で行われました` could never be found. A space is
///    inserted at every boundary that touches a CJK character, which makes
///    each such character a token of its own; the query `東京` then becomes
///    the two-token phrase `"東 京"` and matches consecutive tokens.
///
/// Nothing else is touched: Latin, Cyrillic, Hangul, Arabic and the rest
/// come back byte-for-byte unchanged, and the function is idempotent (a
/// boundary that already has whitespace does not get another space).
///
/// The result is *index-only* text. It is not what the user typed and must
/// never be shown back to a human — the UI's snippets come from
/// `messages.snippet`, never from an FTS column, and nothing in the
/// codebase reads a column of `messages_fts` back out.
#[must_use]
pub fn fts_text(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut prev: Option<char> = None;
    for ch in raw.chars() {
        let ch = match ch {
            'ё' => 'е',
            'Ё' => 'Е',
            other => other,
        };
        // A boundary needs a space when at least one of its two sides is a
        // CJK character. Between two of them that is what turns a run into
        // one-token-per-character; on the edge of a run it is what keeps a
        // mixed word like `東京Tokyo` from indexing as `東 京Tokyo` while
        // the query for `Tokyo` alone asks for the bare token `tokyo`.
        // Whitespace on either side means the boundary is already split,
        // which is also what makes this idempotent.
        if let Some(prev) = prev {
            if (is_cjk(prev) || is_cjk(ch)) && !prev.is_whitespace() && !ch.is_whitespace() {
                out.push(' ');
            }
        }
        out.push(ch);
        prev = Some(ch);
    }
    out
}

/// Han ideographs and kana — the scripts that write words without spaces
/// and that `unicode61` therefore glues into a single token.
///
/// Hangul is deliberately **not** here: Korean is written with spaces
/// between words, so `unicode61` already tokenizes it the way a reader
/// expects, and splitting every syllable block would only make phrases
/// longer for no gain. Neither is CJK punctuation (U+3000–U+303F), which
/// `unicode61` already treats as a separator.
fn is_cjk(ch: char) -> bool {
    matches!(
        u32::from(ch),
        0x3040..=0x30FF        // Hiragana + Katakana
            | 0x3400..=0x4DBF  // CJK Unified Ideographs Extension A
            | 0x4E00..=0x9FFF  // CJK Unified Ideographs
            | 0xF900..=0xFAFF  // CJK Compatibility Ideographs
            | 0x2_0000..=0x2_FA1F // SIP: Extensions B-F + Compatibility Supplement
    )
}

/// A parsed search query: an ordered list of filters, all combined with AND.
///
/// Order is preserved from the input for reproducibility, but callers should
/// not rely on it for anything beyond that: repeating a filter does not
/// change its meaning, it just repeats the AND condition.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Query {
    /// Every token the user typed, one [`Filter`] each, left to right.
    pub filters: Vec<Filter>,
}

/// One token of a parsed [`Query`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Filter {
    /// A bare word, searched literally across every indexed column.
    Term(String),
    /// A `"quoted phrase"`, searched literally (as a phrase) across every
    /// indexed column.
    Phrase(String),
    /// `from:VALUE` — sender field must contain this text.
    From(String),
    /// `subject:VALUE` — subject field must contain this text.
    Subject(String),
    /// `to:VALUE` — recipient must match. See [`Addressee`]: `to:me` is
    /// never resolved by this crate.
    To(Addressee),
    /// `is:unread` / `is:starred` / `is:read`.
    Is(IsFlag),
    /// `has:attachment`.
    HasAttachment,
    /// `after:YYYY-MM-DD`, inclusive lower bound. See [`Date`].
    After(Date),
    /// `before:YYYY-MM-DD`, exclusive upper bound. See [`Date`].
    Before(Date),
}

/// The target of a `to:` filter.
///
/// `to:me` cannot be resolved by this crate: it has no idea which account
/// is signed in, let alone its address. The [`Addressee::Me`] variant exists
/// precisely so the caller **cannot silently ignore this and search for the
/// literal word "me"** — a `String` field would make that mistake
/// undetectable at compile time, so this is an enum instead, and the caller
/// must match both arms to build a query. Whoever calls
/// [`Query::to_search_plan`] (Core, today) owns resolving `Addressee::Me` to
/// the signed-in account's own address before running the search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Addressee {
    /// `to:me` — the caller must substitute the signed-in account's address.
    Me,
    /// `to:SOMEONE` — a literal address or address fragment as typed.
    Address(String),
}

/// `is:` values from D54.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsFlag {
    /// `is:unread`.
    Unread,
    /// `is:starred`.
    Starred,
    /// `is:read`.
    Read,
}

/// A calendar date parsed from `after:`/`before:`, e.g. `2026-08-01`.
///
/// This is deliberately **not** a timestamp. This crate has no timezone and
/// no clock, so turning "2026-08-01" into a moment in time would require
/// guessing a timezone (UTC would be wrong for any user not on UTC, shifting
/// results by a day). The caller — which knows the local timezone — is
/// responsible for turning this into whatever bound its storage layer needs.
///
/// Range semantics (also the caller's responsibility to honor): `after:X`
/// is an **inclusive** lower bound (day X itself matches), `before:X` is an
/// **exclusive** upper bound (day X itself does not match). That makes
/// `after:2026-08-01 before:2026-09-01` mean exactly the month of August:
/// August 1st is included, September 1st is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Date {
    /// Full year, e.g. 2026.
    pub year: u16,
    /// Month, 1-12.
    pub month: u8,
    /// Day of month, 1-31 (validated against `year`/`month` at parse time).
    pub day: u8,
}

/// A filter that cannot be expressed inside an FTS5 `MATCH` string and must
/// be applied by the caller as a separate SQL condition.
///
/// This is data, not SQL text: no column names, no placeholders, no
/// comparison operators. The caller binds each variant to its own schema
/// (parameterized, never string-interpolated) — this crate has no
/// dependency on `rusqlite` or any other SQL layer and cannot do that
/// itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Predicate {
    /// `is:unread` / `is:starred` / `is:read`.
    Is(IsFlag),
    /// `has:attachment`.
    HasAttachment,
    /// `to:` — see [`Addressee`] for why `me` is never resolved here.
    To(Addressee),
    /// `after:` — inclusive lower bound, see [`Date`].
    After(Date),
    /// `before:` — exclusive upper bound, see [`Date`].
    Before(Date),
}

/// The result of [`Query::to_search_plan`]: data the caller can execute, not
/// a query this crate runs itself.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SearchPlan {
    /// An FTS5 `MATCH` argument built from every `Term`/`Phrase`/`From`/
    /// `Subject` filter, already escaped and safe to bind as-is. `None`
    /// when the query has no free-text component at all (including the
    /// empty query) — an empty string is a syntax error in FTS5, so the
    /// caller must skip the `MATCH` clause entirely rather than pass `""`.
    ///
    /// The virtual table this is matched against must expose a `sender`
    /// column (for `from:`) and a `subject` column (for `subject:`);
    /// unscoped terms search across all columns of the table by FTS5's
    /// default behavior. Wiring up that table is T-048's job.
    pub fts_match: Option<String>,
    /// Every filter that is not full-text, in input order. See
    /// [`Predicate`].
    pub predicates: Vec<Predicate>,
}

impl Query {
    /// Parse a raw search string as typed by a user (or sent verbatim by an
    /// MCP client) into a structured [`Query`].
    ///
    /// This never fails and never panics: an empty string parses to a query
    /// with no filters ("show everything"), and any token this crate does
    /// not recognize as a well-formed operator — including one a user is
    /// still in the middle of typing, like `after:2026-0` — becomes a
    /// literal [`Filter::Term`] rather than an error or a dropped filter.
    #[must_use]
    pub fn parse(input: &str) -> Query {
        let filters = split_tokens(input)
            .into_iter()
            .map(|t| parse_token(&t))
            .collect();
        Query { filters }
    }

    /// Turn this query into an [`SearchPlan`] the caller can execute: an
    /// escaped FTS5 `MATCH` string plus a list of [`Predicate`]s to apply
    /// as ordinary SQL `WHERE` conditions.
    #[must_use]
    pub fn to_search_plan(&self) -> SearchPlan {
        let mut fts_terms: Vec<String> = Vec::new();
        let mut predicates: Vec<Predicate> = Vec::new();

        for filter in &self.filters {
            match filter {
                Filter::Term(text) | Filter::Phrase(text) => {
                    fts_terms.push(escape_fts_literal(text));
                }
                Filter::From(text) => {
                    fts_terms.push(format!("sender:{}", escape_fts_literal(text)));
                }
                Filter::Subject(text) => {
                    fts_terms.push(format!("subject:{}", escape_fts_literal(text)));
                }
                Filter::To(addressee) => predicates.push(Predicate::To(addressee.clone())),
                Filter::Is(flag) => predicates.push(Predicate::Is(*flag)),
                Filter::HasAttachment => predicates.push(Predicate::HasAttachment),
                Filter::After(date) => predicates.push(Predicate::After(*date)),
                Filter::Before(date) => predicates.push(Predicate::Before(*date)),
            }
        }

        let fts_match = if fts_terms.is_empty() {
            None
        } else {
            Some(fts_terms.join(" "))
        };
        SearchPlan {
            fts_match,
            predicates,
        }
    }
}

/// Split `input` into whitespace-separated tokens, except that whitespace
/// inside a `"..."` span never splits a token — the span is read verbatim,
/// including an unterminated one that runs to the end of the string. This
/// is what lets `subject:"quarterly report"` stay one token, and what lets
/// a user mid-keystroke (`subject:"still typ`) keep typing without the
/// parser losing track of where the phrase starts.
fn split_tokens(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;

    for ch in input.chars() {
        if ch == '"' {
            in_quotes = !in_quotes;
            current.push(ch);
        } else if ch.is_whitespace() && !in_quotes {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
        } else {
            current.push(ch);
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Strip a leading `"` and, if present, a matching trailing `"`. An
/// unterminated phrase (no trailing quote) keeps everything after the
/// opening quote — "read to end of line", per D54's incremental-typing
/// requirement.
fn strip_quotes(value: &str) -> String {
    match value.strip_prefix('"') {
        Some(rest) => match rest.strip_suffix('"') {
            Some(inner) => inner.to_string(),
            None => rest.to_string(),
        },
        None => value.to_string(),
    }
}

/// Parse one whitespace-delimited raw token into a [`Filter`].
fn parse_token(raw: &str) -> Filter {
    if raw.starts_with('"') {
        return Filter::Phrase(strip_quotes(raw));
    }

    if let Some(value) = raw.strip_prefix("from:") {
        return operator_or_term(raw, value, |v| Filter::From(strip_quotes(v)));
    }
    if let Some(value) = raw.strip_prefix("to:") {
        return operator_or_term(raw, value, |v| {
            let addressee = strip_quotes(v);
            if addressee == "me" {
                Filter::To(Addressee::Me)
            } else {
                Filter::To(Addressee::Address(addressee))
            }
        });
    }
    if let Some(value) = raw.strip_prefix("subject:") {
        return operator_or_term(raw, value, |v| Filter::Subject(strip_quotes(v)));
    }
    if let Some(value) = raw.strip_prefix("is:") {
        return match value {
            "unread" => Filter::Is(IsFlag::Unread),
            "starred" => Filter::Is(IsFlag::Starred),
            "read" => Filter::Is(IsFlag::Read),
            _ => Filter::Term(raw.to_string()),
        };
    }
    if let Some(value) = raw.strip_prefix("has:") {
        return match value {
            "attachment" => Filter::HasAttachment,
            _ => Filter::Term(raw.to_string()),
        };
    }
    if let Some(value) = raw.strip_prefix("after:") {
        return match parse_date(value) {
            Some(date) => Filter::After(date),
            None => Filter::Term(raw.to_string()),
        };
    }
    if let Some(value) = raw.strip_prefix("before:") {
        return match parse_date(value) {
            Some(date) => Filter::Before(date),
            None => Filter::Term(raw.to_string()),
        };
    }

    Filter::Term(raw.to_string())
}

/// Shared "empty value falls back to the whole raw token as a literal term"
/// rule for `from:`/`to:`/`subject:` — an operator with nothing after the
/// colon (`from:` with no value, e.g. because the user hasn't typed it yet)
/// is not a usable filter, so per D54's "unknown operator = term" spirit it
/// degrades to a literal term instead of a filter with an empty value.
fn operator_or_term(raw: &str, value: &str, build: impl FnOnce(&str) -> Filter) -> Filter {
    if value.is_empty() {
        Filter::Term(raw.to_string())
    } else {
        build(value)
    }
}

/// Escape `raw` for safe use as a literal inside an FTS5 `MATCH` string:
/// wrap it in double quotes and double any double quote it contains. This
/// is what stops user input like `OR`, `NEAR(a b)`, `*`, or a literal `"`
/// from being interpreted as FTS5 syntax instead of being searched for
/// literally.
///
/// The text is first put through [`fts_text`], the same normalization the
/// indexer applies before writing `messages_fts` (T-155) — the two must
/// agree or nothing matches. Normalizing *inside* the quotes is what makes
/// a CJK word searchable: `東京` becomes the phrase `"東 京"`, two tokens
/// that must appear next to each other, which is exactly what the user
/// asked for and still not a wildcard (D54 stays a literal search). The
/// quoting and quote-doubling are unchanged and still applied to the
/// normalized text, so no input can escape the literal.
fn escape_fts_literal(raw: &str) -> String {
    let normalized = fts_text(raw);
    let mut out = String::with_capacity(normalized.len() + 2);
    out.push('"');
    for ch in normalized.chars() {
        if ch == '"' {
            out.push('"');
        }
        out.push(ch);
    }
    out.push('"');
    out
}

/// Parse a strict `YYYY-MM-DD` date, validating month/day ranges (including
/// leap years). Anything short of a fully-typed, calendar-valid date
/// returns `None` so the caller falls back to a literal term instead of
/// erroring or silently dropping the filter — see D54's "still typing"
/// requirement.
fn parse_date(value: &str) -> Option<Date> {
    let bytes = value.as_bytes();
    if bytes.len() != 10 {
        return None;
    }
    if bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    for &i in &[0usize, 1, 2, 3, 5, 6, 8, 9] {
        if !bytes[i].is_ascii_digit() {
            return None;
        }
    }

    let year: u16 = value[0..4].parse().ok()?;
    let month: u8 = value[5..7].parse().ok()?;
    let day: u8 = value[8..10].parse().ok()?;
    let date = Date { year, month, day };
    if date.is_valid() {
        Some(date)
    } else {
        None
    }
}

impl Date {
    fn is_valid(self) -> bool {
        if self.month == 0 || self.month > 12 {
            return false;
        }
        self.day != 0 && self.day <= days_in_month(self.year, self.month)
    }
}

fn is_leap_year(year: u16) -> bool {
    (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400)
}

fn days_in_month(year: u16, month: u8) -> u8 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap_year(year) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::{crate_name, fts_text, Addressee, Date, Filter, IsFlag, Predicate, Query};

    #[test]
    fn crate_compiles() {
        assert!(crate_name().starts_with("feathermail-"));
    }

    // -- §15 example table: one test per row -------------------------------

    #[test]
    fn bare_word_searches_every_indexed_column() {
        let plan = Query::parse("invoice").to_search_plan();
        assert_eq!(plan.fts_match.as_deref(), Some("\"invoice\""));
        assert!(plan.predicates.is_empty());
    }

    #[test]
    fn from_domain_filters_sender_column() {
        let plan = Query::parse("from:github.com").to_search_plan();
        assert_eq!(plan.fts_match.as_deref(), Some("sender:\"github.com\""));
    }

    #[test]
    fn from_full_address_filters_sender_column() {
        let plan = Query::parse("from:john@example.com").to_search_plan();
        assert_eq!(
            plan.fts_match.as_deref(),
            Some("sender:\"john@example.com\"")
        );
    }

    #[test]
    fn to_me_is_never_resolved_to_the_literal_word_me() {
        let query = Query::parse("to:me");
        assert_eq!(query.filters, vec![Filter::To(Addressee::Me)]);
        let plan = query.to_search_plan();
        assert_eq!(plan.predicates, vec![Predicate::To(Addressee::Me)]);
        // Must not leak into the free-text FTS match as the literal word "me".
        assert_eq!(plan.fts_match, None);
    }

    #[test]
    fn to_address_becomes_a_predicate_for_the_caller_to_match() {
        let plan = Query::parse("to:jane@example.com").to_search_plan();
        assert_eq!(
            plan.predicates,
            vec![Predicate::To(Addressee::Address(
                "jane@example.com".to_string()
            ))]
        );
    }

    #[test]
    fn subject_filters_subject_column() {
        let plan = Query::parse("subject:invoice").to_search_plan();
        assert_eq!(plan.fts_match.as_deref(), Some("subject:\"invoice\""));
    }

    #[test]
    fn is_unread_is_a_predicate_not_a_text_search() {
        let plan = Query::parse("is:unread").to_search_plan();
        assert_eq!(plan.predicates, vec![Predicate::Is(IsFlag::Unread)]);
        assert_eq!(plan.fts_match, None);
    }

    #[test]
    fn is_starred_is_a_predicate_not_a_text_search() {
        let plan = Query::parse("is:starred").to_search_plan();
        assert_eq!(plan.predicates, vec![Predicate::Is(IsFlag::Starred)]);
    }

    #[test]
    fn is_read_is_a_predicate_not_a_text_search() {
        let plan = Query::parse("is:read").to_search_plan();
        assert_eq!(plan.predicates, vec![Predicate::Is(IsFlag::Read)]);
    }

    #[test]
    fn has_attachment_is_a_predicate_not_a_text_search() {
        let plan = Query::parse("has:attachment").to_search_plan();
        assert_eq!(plan.predicates, vec![Predicate::HasAttachment]);
        assert_eq!(plan.fts_match, None);
    }

    #[test]
    fn after_date_is_parsed_structurally_not_as_an_epoch() {
        let plan = Query::parse("after:2026-08-01").to_search_plan();
        assert_eq!(
            plan.predicates,
            vec![Predicate::After(Date {
                year: 2026,
                month: 8,
                day: 1
            })]
        );
    }

    #[test]
    fn before_date_is_parsed_structurally_not_as_an_epoch() {
        let plan = Query::parse("before:2026-09-01").to_search_plan();
        assert_eq!(
            plan.predicates,
            vec![Predicate::Before(Date {
                year: 2026,
                month: 9,
                day: 1
            })]
        );
    }

    // -- Escaping (fork point 1) --------------------------------------------

    #[test]
    fn fts_injection_via_bare_or_is_searched_literally() {
        let plan = Query::parse("OR").to_search_plan();
        assert_eq!(plan.fts_match.as_deref(), Some("\"OR\""));
    }

    #[test]
    fn fts_injection_via_wildcard_star_is_searched_literally() {
        let plan = Query::parse("*").to_search_plan();
        assert_eq!(plan.fts_match.as_deref(), Some("\"*\""));
    }

    #[test]
    fn fts_injection_via_near_operator_is_searched_literally() {
        // Quoted so it stays one token/phrase, exactly as a user would have
        // to type it to mean "search for this exact text".
        let plan = Query::parse("\"NEAR(a b)\"").to_search_plan();
        assert_eq!(plan.fts_match.as_deref(), Some("\"NEAR(a b)\""));
    }

    #[test]
    fn embedded_double_quote_is_doubled_not_left_to_break_out_of_match_string() {
        let query = Query {
            filters: vec![Filter::Term("embedded \" quote".to_string())],
        };
        let plan = query.to_search_plan();
        assert_eq!(plan.fts_match.as_deref(), Some("\"embedded \"\" quote\""));
    }

    // -- Empty query (fork point 2) ------------------------------------------

    #[test]
    fn empty_query_means_show_everything_not_match_nothing() {
        let plan = Query::parse("").to_search_plan();
        assert_eq!(plan.fts_match, None);
        assert!(plan.predicates.is_empty());
    }

    #[test]
    fn whitespace_only_query_means_show_everything() {
        let plan = Query::parse("   \t  ").to_search_plan();
        assert_eq!(plan.fts_match, None);
        assert!(plan.predicates.is_empty());
    }

    // -- Dates (fork point 4): inclusive/exclusive boundary ------------------

    #[test]
    fn after_bound_is_not_shifted_to_the_next_day() {
        // If a mutation nudged `after:` to be exclusive by adding a day to
        // stored date, this would drift to day 2.
        let plan = Query::parse("after:2026-08-01").to_search_plan();
        assert_eq!(
            plan.predicates.first(),
            Some(&Predicate::After(Date {
                year: 2026,
                month: 8,
                day: 1
            }))
        );
    }

    #[test]
    fn before_bound_is_not_shifted_to_the_previous_day() {
        // If a mutation nudged `before:` to be inclusive by subtracting a
        // day, this would drift to day 31 of August.
        let plan = Query::parse("before:2026-09-01").to_search_plan();
        assert_eq!(
            plan.predicates.first(),
            Some(&Predicate::Before(Date {
                year: 2026,
                month: 9,
                day: 1
            }))
        );
    }

    #[test]
    fn after_and_before_together_bound_exactly_one_calendar_month() {
        let plan = Query::parse("after:2026-08-01 before:2026-09-01").to_search_plan();
        assert_eq!(
            plan.predicates,
            vec![
                Predicate::After(Date {
                    year: 2026,
                    month: 8,
                    day: 1
                }),
                Predicate::Before(Date {
                    year: 2026,
                    month: 9,
                    day: 1
                }),
            ]
        );
    }

    // -- Malformed/incomplete dates (fork point 5) ---------------------------

    #[test]
    fn single_digit_after_date_mid_typing_falls_back_to_literal_term() {
        let query = Query::parse("after:2");
        assert_eq!(query.filters, vec![Filter::Term("after:2".to_string())]);
    }

    #[test]
    fn truncated_after_date_mid_typing_falls_back_to_literal_term() {
        let query = Query::parse("after:2026-0");
        assert_eq!(
            query.filters,
            vec![Filter::Term("after:2026-0".to_string())]
        );
    }

    #[test]
    fn out_of_range_after_date_falls_back_to_literal_term_not_dropped_filter() {
        let query = Query::parse("after:2026-13-45");
        assert_eq!(
            query.filters,
            vec![Filter::Term("after:2026-13-45".to_string())]
        );
        // Critically: it must not silently vanish as a broken filter, nor
        // panic. It stays present, as a literal search term.
        let plan = query.to_search_plan();
        assert!(plan.predicates.is_empty());
        assert_eq!(plan.fts_match.as_deref(), Some("\"after:2026-13-45\""));
    }

    #[test]
    fn before_date_with_invalid_calendar_day_falls_back_to_literal_term() {
        let query = Query::parse("before:2026-02-30");
        assert_eq!(
            query.filters,
            vec![Filter::Term("before:2026-02-30".to_string())]
        );
    }

    #[test]
    fn february_29_is_valid_only_on_a_leap_year() {
        assert_eq!(
            Query::parse("after:2024-02-29").filters,
            vec![Filter::After(Date {
                year: 2024,
                month: 2,
                day: 29
            })]
        );
        assert_eq!(
            Query::parse("after:2026-02-29").filters,
            vec![Filter::Term("after:2026-02-29".to_string())]
        );
    }

    // -- AND semantics / repeated operators (fork point 6) -------------------

    #[test]
    fn multiple_bare_terms_combine_with_and() {
        let plan = Query::parse("invoice project").to_search_plan();
        assert_eq!(plan.fts_match.as_deref(), Some("\"invoice\" \"project\""));
    }

    #[test]
    fn repeated_operator_intersects_instead_of_replacing_the_earlier_one() {
        let query = Query::parse("from:a from:b");
        assert_eq!(
            query.filters,
            vec![Filter::From("a".to_string()), Filter::From("b".to_string())]
        );
        let plan = query.to_search_plan();
        assert_eq!(plan.fts_match.as_deref(), Some("sender:\"a\" sender:\"b\""));
    }

    // -- Phrases (fork point 7) ----------------------------------------------

    #[test]
    fn quoted_phrase_is_searched_as_one_unit_not_split_on_spaces() {
        let query = Query::parse("\"quarterly report\"");
        assert_eq!(
            query.filters,
            vec![Filter::Phrase("quarterly report".to_string())]
        );
    }

    #[test]
    fn quoted_phrase_works_inside_an_operator_value() {
        let plan = Query::parse("subject:\"quarterly report\"").to_search_plan();
        assert_eq!(
            plan.fts_match.as_deref(),
            Some("subject:\"quarterly report\"")
        );
    }

    #[test]
    fn unterminated_quote_in_operator_value_is_not_an_error() {
        let query = Query::parse("subject:\"quarter");
        assert_eq!(query.filters, vec![Filter::Subject("quarter".to_string())]);
    }

    #[test]
    fn unterminated_bare_quote_reads_to_end_of_line() {
        let query = Query::parse("\"still typing a phrase");
        assert_eq!(
            query.filters,
            vec![Filter::Phrase("still typing a phrase".to_string())]
        );
    }

    // -- Explicitly out of scope for D54 (fork point 8) -----------------------

    #[test]
    fn minus_prefixed_term_is_literal_text_not_negation() {
        let query = Query::parse("-invoice");
        assert_eq!(query.filters, vec![Filter::Term("-invoice".to_string())]);
    }

    #[test]
    fn bare_or_between_terms_is_literal_text_not_boolean_or() {
        let plan = Query::parse("apple OR banana").to_search_plan();
        assert_eq!(
            plan.fts_match.as_deref(),
            Some("\"apple\" \"OR\" \"banana\"")
        );
    }

    // -- Unknown operators (D54: "неизвестные операторы = обычный term") ----

    #[test]
    fn unknown_operator_falls_back_to_literal_term() {
        let query = Query::parse("foo:bar");
        assert_eq!(query.filters, vec![Filter::Term("foo:bar".to_string())]);
    }

    #[test]
    fn unrecognized_is_value_falls_back_to_literal_term() {
        let query = Query::parse("is:archived");
        assert_eq!(query.filters, vec![Filter::Term("is:archived".to_string())]);
    }

    #[test]
    fn unrecognized_has_value_falls_back_to_literal_term() {
        let query = Query::parse("has:calendar");
        assert_eq!(
            query.filters,
            vec![Filter::Term("has:calendar".to_string())]
        );
    }

    // -- T-155: index/query normalization ------------------------------------

    #[test]
    fn normalization_folds_cyrillic_yo_to_ye_in_both_cases() {
        assert_eq!(fts_text("Оплатите СЧЁТ"), "Оплатите СЧЕТ");
        assert_eq!(fts_text("ёжик"), "ежик");
        assert_eq!(fts_text("Ёлка"), "Елка");
    }

    #[test]
    fn normalization_splits_a_cjk_run_into_one_token_per_character() {
        assert_eq!(
            fts_text("今日の会議は東京で行われました"),
            "今 日 の 会 議 は 東 京 で 行 わ れ ま し た"
        );
    }

    #[test]
    fn normalization_turns_a_cjk_query_word_into_a_two_token_phrase() {
        // This is the whole point: the query and the indexed body are put
        // through the same function, so `東 京` in the query lines up with
        // `... 東 京 で ...` in the index.
        let plan = Query::parse("東京").to_search_plan();
        assert_eq!(plan.fts_match.as_deref(), Some("\"東 京\""));
    }

    #[test]
    fn normalization_leaves_latin_text_byte_for_byte_alone() {
        let latin = "Quarterly report from john@example.com (Q3, 2026) -- 100% done";
        assert_eq!(fts_text(latin), latin);
    }

    #[test]
    fn normalization_leaves_cyrillic_without_yo_and_hangul_alone() {
        assert_eq!(fts_text("Оплатите счет"), "Оплатите счет");
        // Korean is written with spaces already; splitting syllable blocks
        // would be churn, so Hangul is not in the CJK set.
        assert_eq!(fts_text("안녕하세요 여러분"), "안녕하세요 여러분");
    }

    #[test]
    fn normalization_separates_cjk_from_the_latin_glued_to_it() {
        // Without the boundary space the index would hold the token
        // `京tokyo`, and a search for either half alone would miss it.
        assert_eq!(fts_text("東京Tokyo"), "東 京 Tokyo");
        assert_eq!(fts_text("Tokyo東京"), "Tokyo 東 京");
    }

    #[test]
    fn normalization_does_not_add_a_second_space_where_one_exists() {
        assert_eq!(
            fts_text("Meeting in 東京 tomorrow"),
            "Meeting in 東 京 tomorrow"
        );
    }

    #[test]
    fn normalization_is_idempotent() {
        for raw in [
            "今日の会議は東京で行われました",
            "Оплатите СЧЁТ пожалуйста",
            "東京Tokyo ёлка",
            "plain latin text",
            "",
        ] {
            let once = fts_text(raw);
            assert_eq!(fts_text(&once), once, "not idempotent for {raw:?}");
        }
    }

    #[test]
    fn normalization_applies_inside_a_phrase_without_losing_the_escaping() {
        let plan = Query::parse("\"счёт 東京\"").to_search_plan();
        assert_eq!(plan.fts_match.as_deref(), Some("\"счет 東 京\""));
    }

    #[test]
    fn normalization_applies_to_from_and_subject_values() {
        let plan = Query::parse("from:Королёв subject:東京").to_search_plan();
        assert_eq!(
            plan.fts_match.as_deref(),
            Some("sender:\"Королев\" subject:\"東 京\"")
        );
    }

    #[test]
    fn normalization_never_lets_a_quote_break_out_of_the_literal() {
        let query = Query {
            filters: vec![Filter::Term("счёт \" 東京".to_string())],
        };
        assert_eq!(
            query.to_search_plan().fts_match.as_deref(),
            Some("\"счет \"\" 東 京\"")
        );
    }
}
