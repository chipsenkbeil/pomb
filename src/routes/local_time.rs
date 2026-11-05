use std::fmt;

use chrono::{DateTime, Duration, SecondsFormat, Utc};

use crate::models::TimeFormat;

/// How a rendered time reads.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Shape {
    /// A time of day: "5:00 PM".
    Clock,
    /// A date and a time of day: "Sep 28, 2026 5:00 PM".
    DateTime,
    /// A comment's time: "Today 5:00 PM", "Yesterday 5:00 PM" or "Sep 27 5:00 PM".
    Comment,
    /// A card's due text: "Today 5:00 PM", "Friday 5:00 PM" or "25 September 5:00 PM".
    DueCard,
    /// A modal's due text: like the card's, except other days read "Oct 2 at 5:00 PM".
    DueModal,
    /// A span of one day's time: "5:00 PM - 5:25 PM".
    Range,
}

impl Shape {
    /// Returns the name the browser formats the time by.
    #[must_use]
    fn name(self) -> &'static str {
        match self {
            Self::Clock => "clock",
            Self::DateTime => "datetime",
            Self::Comment => "comment",
            Self::DueCard => "due-card",
            Self::DueModal => "due-modal",
            Self::Range => "range",
        }
    }
}

/// A time that renders as an element carrying its UTC instant, whose text reads as UTC until the
/// browser rewrites it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LocalTime {
    /// The text the element holds without JavaScript.
    text: String,
    /// The instant, or [`None`] when the stored text did not parse.
    at: Option<DateTime<Utc>>,
    /// The end of a [`Shape::Range`].
    end: Option<DateTime<Utc>>,
    /// How the time reads.
    shape: Shape,
    /// The account's clock.
    clock: TimeFormat,
}

impl LocalTime {
    /// Returns the time `rfc3339` names as `shape` reads it, or the text itself when it is not a
    /// time.
    #[must_use]
    pub(crate) fn of(shape: Shape, rfc3339: &str) -> Self {
        Self::build(shape, rfc3339, Utc::now(), TimeFormat::current())
    }

    /// Returns the due text of `shape` for `rfc3339`, or [`None`] when it is not a time.
    #[must_use]
    pub(crate) fn due(shape: Shape, rfc3339: &str) -> Option<Self> {
        let time = Self::of(shape, rfc3339);
        time.at.map(|_| time)
    }

    /// Returns the span that starts at `rfc3339` and lasts `seconds`.
    #[must_use]
    pub(crate) fn range(rfc3339: &str, seconds: i64) -> Self {
        Self::range_with(rfc3339, seconds, Utc::now(), TimeFormat::current())
    }

    /// Returns the attributes that make an element's `title` follow the browser's zone.
    #[must_use]
    pub(crate) fn title_attrs(&self) -> String {
        let Some(at) = self.at else {
            return format!(" title=\"{}\"", escape(&self.text));
        };
        format!(
            " title=\"{}\" data-at=\"{}\" data-fmt=\"{}\" data-clock=\"{}\" data-attr=\"title\"",
            escape(&self.text),
            stamp(at),
            self.shape.name(),
            self.clock.as_str()
        )
    }

    /// Returns [`Self::of`] with the present at `now` and the account's `clock`.
    fn build(shape: Shape, rfc3339: &str, now: DateTime<Utc>, clock: TimeFormat) -> Self {
        let at = DateTime::parse_from_rfc3339(rfc3339)
            .ok()
            .map(|at| at.with_timezone(&Utc));
        let text = at.map_or_else(
            || rfc3339.to_string(),
            |at| format!("{} UTC", written(shape, at, now, clock)),
        );
        Self {
            text,
            at,
            end: None,
            shape,
            clock,
        }
    }

    /// Returns [`Self::range`] with the present at `now` and the account's `clock`.
    fn range_with(rfc3339: &str, seconds: i64, now: DateTime<Utc>, clock: TimeFormat) -> Self {
        let mut time = Self::build(Shape::Range, rfc3339, now, clock);
        if let Some(start) = time.at {
            let end = start + Duration::seconds(seconds);
            time.text = format!(
                "{} - {} UTC",
                written(Shape::Clock, start, now, time.clock),
                written(Shape::Clock, end, now, time.clock)
            );
            time.end = Some(end);
        }
        time
    }
}

impl fmt::Display for LocalTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Some(at) = self.at else {
            return f.write_str(&escape(&self.text));
        };
        write!(
            f,
            "<time class=\"local-time\" datetime=\"{at}\" data-at=\"{at}\" data-fmt=\"{}\" data-clock=\"{}\"",
            self.shape.name(),
            self.clock.as_str(),
            at = stamp(at)
        )?;
        if let Some(end) = self.end {
            write!(f, " data-end=\"{}\"", stamp(end))?;
        }
        write!(f, ">{}</time>", escape(&self.text))
    }
}

/// Returns `at` as RFC 3339 in UTC.
#[must_use]
fn stamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Returns `text` safe to put in HTML text or in a double-quoted attribute.
#[must_use]
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// Returns `at` as `shape` writes it in UTC, with the present at `now`.
#[must_use]
fn written(shape: Shape, at: DateTime<Utc>, now: DateTime<Utc>, clock: TimeFormat) -> String {
    let clock = clock.clock();
    let (day, today) = (at.date_naive(), now.date_naive());
    let due_day = || {
        if day == today {
            Some("Today")
        } else if at > now && day < today + Duration::days(7) {
            Some("%A")
        } else {
            None
        }
    };
    let pattern = match shape {
        Shape::Clock | Shape::Range => clock.to_string(),
        Shape::DateTime => format!("%b %d, %Y {clock}"),
        Shape::Comment => {
            if day == today {
                format!("Today {clock}")
            } else if day == today - Duration::days(1) {
                format!("Yesterday {clock}")
            } else {
                format!("%b %-d {clock}")
            }
        }
        Shape::DueCard => due_day().map_or_else(
            || format!("%-d %B {clock}"),
            |label| format!("{label} {clock}"),
        ),
        Shape::DueModal => due_day().map_or_else(
            || format!("%b %-d at {clock}"),
            |label| format!("{label} {clock}"),
        ),
    };
    at.format(&pattern).to_string()
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone as _;

    use super::*;

    /// Wednesday 2026-09-30 15:22 UTC.
    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 30, 15, 22, 0)
            .single()
            .expect("a time")
    }

    fn at(days: i64, hour: u32, minute: u32) -> String {
        let day = now().date_naive() + Duration::days(days);
        let naive = day.and_hms_opt(hour, minute, 0).expect("a time");
        Utc.from_utc_datetime(&naive).to_rfc3339()
    }

    fn text(shape: Shape, days: i64, hour: u32) -> String {
        LocalTime::build(shape, &at(days, hour, 0), now(), TimeFormat::Hours12).text
    }

    #[test]
    fn due_card_should_read_today_weekday_and_absolute_in_utc() {
        assert_eq!(text(Shape::DueCard, 0, 17), "Today 5:00 PM UTC");
        assert_eq!(text(Shape::DueCard, 0, 9), "Today 9:00 AM UTC");
        assert_eq!(text(Shape::DueCard, 2, 17), "Friday 5:00 PM UTC");
        assert_eq!(text(Shape::DueCard, 6, 17), "Tuesday 5:00 PM UTC");
        assert_eq!(text(Shape::DueCard, 7, 17), "7 October 5:00 PM UTC");
        assert_eq!(text(Shape::DueCard, -5, 17), "25 September 5:00 PM UTC");
        assert_eq!(text(Shape::DueCard, -1, 23), "29 September 11:00 PM UTC");
    }

    #[test]
    fn due_modal_should_read_other_days_with_at() {
        assert_eq!(text(Shape::DueModal, 0, 17), "Today 5:00 PM UTC");
        assert_eq!(text(Shape::DueModal, 2, 23), "Friday 11:00 PM UTC");
        assert_eq!(text(Shape::DueModal, -16, 9), "Sep 14 at 9:00 AM UTC");
        assert_eq!(text(Shape::DueModal, 14, 9), "Oct 14 at 9:00 AM UTC");
    }

    #[test]
    fn comment_and_datetime_should_read_their_day() {
        assert_eq!(text(Shape::Comment, 0, 15), "Today 3:00 PM UTC");
        assert_eq!(text(Shape::Comment, -1, 15), "Yesterday 3:00 PM UTC");
        assert_eq!(text(Shape::Comment, -3, 15), "Sep 27 3:00 PM UTC");
        assert_eq!(text(Shape::DateTime, -22, 9), "Sep 08, 2026 9:00 AM UTC");
        assert_eq!(text(Shape::Clock, 0, 0), "12:00 AM UTC");
    }

    #[test]
    fn a_range_should_carry_both_ends_and_the_24_hour_clock() {
        let start = at(0, 23, 50);
        let time = LocalTime::range_with(&start, 1200, now(), TimeFormat::Hours24);
        assert_eq!(time.text, "23:50 - 00:10 UTC");
        let html = time.to_string();
        assert!(html.contains("data-fmt=\"range\""), "{html}");
        assert!(html.contains("data-clock=\"24h\""), "{html}");
        assert!(html.contains("data-at=\"2026-09-30T23:50:00Z\""), "{html}");
        assert!(html.contains("data-end=\"2026-10-01T00:10:00Z\""), "{html}");
    }

    #[test]
    fn a_stored_text_that_is_no_time_should_render_escaped_and_bare() {
        let time = LocalTime::build(
            Shape::DateTime,
            "<b>not a time</b>",
            now(),
            TimeFormat::Hours12,
        );
        assert_eq!(time.to_string(), "&lt;b&gt;not a time&lt;/b&gt;");
        assert!(LocalTime::due(Shape::DueCard, "not-a-date").is_none());
    }
}
