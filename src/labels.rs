use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::models::TextCap;

/// The board config bag key that holds the label settings.
pub const CONFIG_KEY: &str = "label_settings";

/// The most highlighted labels one board holds.
pub const MAX_HIGHLIGHTS: usize = 30;

/// The most labels a board keeps with no card using them.
pub const MAX_ADDED: usize = 100;

/// One label a board marks as important, with the color its stripe, flag and count draw in.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Highlight {
    /// The label, matched exactly against a task's labels.
    pub label: String,
    /// The color as `#rrggbb`.
    pub color: String,
}

/// What a board stores about its labels: which ones it highlights, whether its cards show a chip
/// for every label, and the labels it keeps before any card uses them.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct LabelSettings {
    /// The highlighted labels, in the order their stripe colors win on a card.
    #[serde(default)]
    pub highlights: Vec<Highlight>,
    /// Whether each card lists all its labels as chips, with the highlighted ones filled in their
    /// color.
    #[serde(default)]
    pub show_all: bool,
    /// Labels the board keeps although no card carries them, which the filter and the label
    /// picker offer.
    #[serde(default)]
    pub added: Vec<String>,
}

impl LabelSettings {
    /// Reads the settings out of a board's config bag, falling back to none when the bag holds
    /// none or holds something unreadable.
    #[must_use]
    pub fn from_config(config_json: &str) -> Self {
        serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(config_json)
            .ok()
            .and_then(|mut bag| bag.remove(CONFIG_KEY))
            .and_then(|value| serde_json::from_value(value).ok())
            .unwrap_or_default()
    }

    /// Returns the settings as the value to store under [`CONFIG_KEY`].
    #[must_use]
    pub fn to_config_value(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
    }

    /// Returns the color `label` is highlighted in, if it is.
    #[must_use]
    pub fn color_of(&self, label: &str) -> Option<&str> {
        self.highlights
            .iter()
            .find(|highlight| highlight.label == label)
            .map(|highlight| highlight.color.as_str())
    }

    /// Returns the settings cleaned up: every label trimmed and deduplicated, every color
    /// lowercase, and a highlighted label dropped from the added ones.
    ///
    /// # Errors
    ///
    /// Returns the reason when a label is over [`TextCap::LABEL`], a color is not `#rrggbb`, or a
    /// list holds more than its most.
    pub fn validated(mut self) -> Result<Self, String> {
        let mut highlights: Vec<Highlight> = Vec::new();
        for highlight in self.highlights {
            let label = clean_label(&highlight.label)?;
            if label.is_empty() || highlights.iter().any(|seen| seen.label == label) {
                continue;
            }
            let color = highlight.color.trim().to_lowercase();
            if !is_hex_color(&color) {
                return Err(format!("color \"{}\" is not #rrggbb", highlight.color));
            }
            highlights.push(Highlight { label, color });
        }
        if highlights.len() > MAX_HIGHLIGHTS {
            return Err(format!("highlights hold at most {MAX_HIGHLIGHTS} labels"));
        }
        let mut added: Vec<String> = Vec::new();
        for label in &self.added {
            let label = clean_label(label)?;
            let highlighted = highlights.iter().any(|seen| seen.label == label);
            if !label.is_empty() && !highlighted && !added.contains(&label) {
                added.push(label);
            }
        }
        if added.len() > MAX_ADDED {
            return Err(format!("added labels hold at most {MAX_ADDED} labels"));
        }
        self.highlights = highlights;
        self.added = added;
        Ok(self)
    }
}

/// Returns `label` trimmed, or the reason it is too long.
fn clean_label(label: &str) -> Result<String, String> {
    let label = label.trim();
    TextCap::LABEL
        .check(label)
        .map_err(|error| error.to_string())?;
    Ok(label.to_string())
}

/// Reports whether `color` is `#` and six hex digits.
fn is_hex_color(color: &str) -> bool {
    color.strip_prefix('#').is_some_and(|digits| {
        digits.len() == 6 && digits.chars().all(|digit| digit.is_ascii_hexdigit())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn highlight(label: &str, color: &str) -> Highlight {
        Highlight {
            label: label.to_string(),
            color: color.to_string(),
        }
    }

    #[test]
    fn validated_should_trim_dedupe_and_lowercase_the_color() {
        let settings = LabelSettings {
            highlights: vec![
                highlight(" hold ", "#E5484D"),
                highlight("hold", "#000000"),
                highlight("", "#000000"),
            ],
            show_all: true,
            added: vec![" idea ".to_string(), "idea".to_string(), "hold".to_string()],
        }
        .validated()
        .unwrap();
        assert_eq!(settings.highlights, vec![highlight("hold", "#e5484d")]);
        assert_eq!(settings.added, vec!["idea"]);
        assert!(settings.show_all);
    }

    #[test]
    fn validated_should_refuse_a_color_that_is_not_six_hex_digits() {
        for color in ["red", "#fff", "e5484d", "#e5484g", "#e5484d00"] {
            let settings = LabelSettings {
                highlights: vec![highlight("hold", color)],
                ..LabelSettings::default()
            };
            assert!(settings.validated().is_err(), "{color}");
        }
    }

    #[test]
    fn validated_should_refuse_too_many_highlights_and_a_long_label() {
        let many = LabelSettings {
            highlights: (0..=MAX_HIGHLIGHTS)
                .map(|n| highlight(&format!("l{n}"), "#000000"))
                .collect(),
            ..LabelSettings::default()
        };
        assert!(many.validated().is_err());
        let long = LabelSettings {
            added: vec!["x".repeat(51)],
            ..LabelSettings::default()
        };
        assert!(long.validated().is_err());
    }

    #[test]
    fn from_config_should_read_nothing_from_a_bag_without_settings() {
        assert_eq!(LabelSettings::from_config("{}"), LabelSettings::default());
        assert_eq!(LabelSettings::from_config("nope"), LabelSettings::default());
    }

    #[test]
    fn settings_should_round_trip_through_the_config_bag() {
        let settings = LabelSettings {
            highlights: vec![highlight("hold", "#e5484d")],
            show_all: true,
            added: vec!["idea".to_string()],
        };
        let bag = serde_json::json!({ CONFIG_KEY: settings.to_config_value() });
        assert_eq!(LabelSettings::from_config(&bag.to_string()), settings);
        assert_eq!(settings.color_of("hold"), Some("#e5484d"));
        assert_eq!(settings.color_of("idea"), None);
    }
}
