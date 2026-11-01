use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// The board config bag key that holds the column rules.
pub const CONFIG_KEY: &str = "column_rules";

/// The label that locks a task when a board stores no list of its own.
const DEFAULT_LOCKED_LABEL: &str = "needs:human";

/// The most labels, sections or field keys one list may hold.
const MAX_ITEMS: usize = 20;

/// The most characters one label, section or field key may hold.
const MAX_ITEM_CHARS: usize = 100;

/// What a board enforces about its columns and its locked tasks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ColumnRules {
    /// Labels that lock a task: an API token cannot move, edit or otherwise write a task that
    /// carries one, and the owner's browser session can. Absent means `["needs:human"]`, and an
    /// empty list locks nothing.
    #[serde(default = "default_locked_labels")]
    pub locked_labels: Vec<String>,
    /// The entry rule of each column, by column id. A column with no entry takes any task.
    #[serde(default)]
    pub columns: BTreeMap<String, EntryRule>,
}

/// What a task must have to enter one column, or to be edited while it sits there.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct EntryRule {
    /// Markdown headings the description must hold, each with text under it. The match ignores
    /// case, heading level and a trailing colon.
    #[serde(default)]
    pub sections: Vec<String>,
    /// The fewest outcomes (subtasks) the task must hold.
    #[serde(default)]
    pub outcomes_min: Option<u32>,
    /// The most outcomes (subtasks) the task may hold.
    #[serde(default)]
    pub outcomes_max: Option<u32>,
    /// Keys of the board fields the task must have a value for.
    #[serde(default)]
    pub fields: Vec<String>,
}

/// Returns the locked labels of a board that stores none.
#[must_use]
fn default_locked_labels() -> Vec<String> {
    vec![DEFAULT_LOCKED_LABEL.to_string()]
}

impl Default for ColumnRules {
    fn default() -> Self {
        Self {
            locked_labels: default_locked_labels(),
            columns: BTreeMap::new(),
        }
    }
}

impl EntryRule {
    /// Reports whether the rule asks for nothing.
    fn is_empty(&self) -> bool {
        self.sections.is_empty()
            && self.outcomes_min.is_none()
            && self.outcomes_max.is_none()
            && self.fields.is_empty()
    }
}

impl ColumnRules {
    /// Reads the rules out of a board's config bag, falling back to the defaults when the bag
    /// holds none or holds something unreadable.
    #[must_use]
    pub fn from_config(config_json: &str) -> Self {
        serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(config_json)
            .ok()
            .and_then(|mut bag| bag.remove(CONFIG_KEY))
            .and_then(|value| serde_json::from_value(value).ok())
            .unwrap_or_default()
    }

    /// Removes `field_key` from every entry rule, dropping a rule that then asks for nothing.
    fn drop_field(&mut self, field_key: &str) {
        for rule in self.columns.values_mut() {
            rule.fields.retain(|key| key != field_key);
        }
        self.columns.retain(|_, rule| !rule.is_empty());
    }

    /// Returns the rules as the value to store under [`CONFIG_KEY`].
    #[must_use]
    pub fn to_config_value(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
    }

    /// Returns the entry rule of `column_id`, if the column has one.
    #[must_use]
    pub fn rule_for(&self, column_id: &str) -> Option<&EntryRule> {
        self.columns.get(column_id)
    }

    /// Reports whether any of `labels` locks a task. Labels match ignoring case and surrounding
    /// space.
    #[must_use]
    pub fn locks(&self, labels: &[String]) -> bool {
        labels.iter().any(|label| {
            let label = label.trim().to_lowercase();
            self.locked_labels
                .iter()
                .any(|locked| locked.trim().to_lowercase() == label)
        })
    }

    /// Returns the rules cleaned up: every list trimmed and deduplicated, and a column whose rule
    /// asks for nothing dropped.
    ///
    /// # Errors
    ///
    /// Returns the reason when a column id or a field key is not one of the board's, a list
    /// holds too many items or an item is too long, or the fewest outcomes exceed the most.
    pub fn validated(
        mut self,
        column_ids: &BTreeSet<&str>,
        field_keys: &BTreeSet<&str>,
    ) -> Result<Self, String> {
        self.locked_labels = clean_list("locked_labels", &self.locked_labels)?;
        let mut columns = BTreeMap::new();
        for (column_id, mut rule) in self.columns {
            if !column_ids.contains(column_id.as_str()) {
                return Err(format!("unknown column \"{column_id}\""));
            }
            rule.sections = clean_list("sections", &rule.sections)?;
            rule.fields = clean_list("fields", &rule.fields)?;
            if let Some(key) = rule
                .fields
                .iter()
                .find(|key| !field_keys.contains(key.as_str()))
            {
                return Err(format!("unknown field \"{key}\""));
            }
            if let (Some(min), Some(max)) = (rule.outcomes_min, rule.outcomes_max)
                && min > max
            {
                return Err(format!(
                    "outcomes_min {min} is above outcomes_max {max} for column \"{column_id}\""
                ));
            }
            if !rule.is_empty() {
                columns.insert(column_id, rule);
            }
        }
        self.columns = columns;
        Ok(self)
    }
}

/// Trims and dedupes `items`, refusing too many or an over-long one, naming the list `name`.
fn clean_list(name: &str, items: &[String]) -> Result<Vec<String>, String> {
    let mut clean: Vec<String> = Vec::new();
    for item in items {
        let item = item.trim();
        if item.is_empty() || clean.iter().any(|seen| seen == item) {
            continue;
        }
        if item.chars().count() > MAX_ITEM_CHARS {
            return Err(format!(
                "{name} entries are at most {MAX_ITEM_CHARS} characters"
            ));
        }
        clean.push(item.to_string());
    }
    if clean.len() > MAX_ITEMS {
        return Err(format!("{name} holds at most {MAX_ITEMS} entries"));
    }
    Ok(clean)
}

/// Returns `config_json` with `field_key` removed from every entry rule, or [`None`] when no rule
/// names the field.
#[must_use]
pub fn config_without_field(config_json: &str, field_key: &str) -> Option<String> {
    let mut bag: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(config_json).ok()?;
    let mut rules: ColumnRules = serde_json::from_value(bag.get(CONFIG_KEY)?.clone()).ok()?;
    if !rules
        .columns
        .values()
        .any(|rule| rule.fields.iter().any(|key| key == field_key))
    {
        return None;
    }
    rules.drop_field(field_key);
    bag.insert(CONFIG_KEY.to_string(), rules.to_config_value());
    serde_json::to_string(&bag).ok()
}

/// How a description holds one section.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Section {
    /// No heading carries the section's name.
    Missing,
    /// The heading is there with nothing under it.
    Empty,
    /// Text or a subheading follows the heading.
    Filled,
}

/// Lists, as sentences, what keeps a task from meeting `rule`: `description` is its markdown,
/// `outcomes` the number of subtasks and `fields` its custom field values.
#[must_use]
pub fn violations(
    rule: &EntryRule,
    description: &str,
    outcomes: usize,
    fields: &serde_json::Map<String, serde_json::Value>,
) -> Vec<String> {
    let mut found = Vec::new();
    for name in &rule.sections {
        match section_in(description, name) {
            Section::Missing => found.push(format!("the description needs a \"{name}\" section")),
            Section::Empty => found.push(format!("the \"{name}\" section is empty")),
            Section::Filled => {}
        }
    }
    if let Some(min) = rule.outcomes_min
        && outcomes < min as usize
    {
        found.push(format!(
            "the task needs at least {min} outcomes and has {outcomes}"
        ));
    }
    if let Some(max) = rule.outcomes_max
        && outcomes > max as usize
    {
        found.push(format!(
            "the task may have at most {max} outcomes and has {outcomes}"
        ));
    }
    for key in &rule.fields {
        if fields.get(key).is_none_or(is_blank) {
            found.push(format!("the field \"{key}\" needs a value"));
        }
    }
    found
}

/// Reports whether a field value holds nothing: null, blank text or an empty list.
#[must_use]
fn is_blank(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Null => true,
        serde_json::Value::String(text) => text.trim().is_empty(),
        serde_json::Value::Array(items) => items.is_empty(),
        _ => false,
    }
}

/// Reports how `description` holds the section `name`.
#[must_use]
fn section_in(description: &str, name: &str) -> Section {
    let wanted = name.trim().to_lowercase();
    let mut found = Section::Missing;
    let mut open_level = None;
    let mut fence = false;
    for line in description.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fence = !fence;
        }
        let heading = if fence { None } else { heading_of(trimmed) };
        match (heading, open_level) {
            (Some((level, text)), _) if open_level.is_none_or(|open| level <= open) => {
                open_level = (text.to_lowercase() == wanted).then_some(level);
                if open_level.is_some() && found == Section::Missing {
                    found = Section::Empty;
                }
            }
            (None, Some(_)) if !trimmed.is_empty() => return Section::Filled,
            (Some(_), Some(_)) => return Section::Filled,
            _ => {}
        }
    }
    found
}

/// Returns the level and text of a markdown heading line, [`None`] for any other line.
#[must_use]
fn heading_of(line: &str) -> Option<(usize, &str)> {
    let level = line.chars().take_while(|c| *c == '#').count();
    if !(1..=6).contains(&level) {
        return None;
    }
    let text = &line[level..];
    if !text.is_empty() && !text.starts_with(char::is_whitespace) {
        return None;
    }
    Some((
        level,
        text.trim()
            .trim_end_matches('#')
            .trim_end()
            .trim_end_matches(':'),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule() -> EntryRule {
        EntryRule {
            sections: vec!["Done when".to_string()],
            ..EntryRule::default()
        }
    }

    fn none() -> serde_json::Map<String, serde_json::Value> {
        serde_json::Map::new()
    }

    mod violations {
        use super::*;

        #[test]
        fn should_name_a_missing_section() {
            let found = violations(&rule(), "just text", 0, &none());
            assert_eq!(found, vec!["the description needs a \"Done when\" section"]);
        }

        #[test]
        fn should_accept_a_filled_section_at_any_level_and_case() {
            let found = violations(&rule(), "intro\n\n### done WHEN:\n- ships\n", 0, &none());
            assert!(found.is_empty(), "{found:?}");
        }

        #[test]
        fn should_refuse_an_empty_section() {
            let found = violations(&rule(), "## Done when\n\n## Notes\ntext", 0, &none());
            assert_eq!(found, vec!["the \"Done when\" section is empty"]);
        }

        #[test]
        fn should_count_a_subheading_as_part_of_its_section() {
            let found = violations(&rule(), "## Done when\n### Detail\ntext", 0, &none());
            assert!(found.is_empty(), "{found:?}");
        }

        #[test]
        fn should_ignore_a_heading_inside_a_fence() {
            let found = violations(&rule(), "```\n## Done when\ntext\n```", 0, &none());
            assert_eq!(found.len(), 1);
        }

        #[test]
        fn should_check_the_outcome_range() {
            let rule = EntryRule {
                outcomes_min: Some(2),
                outcomes_max: Some(3),
                ..EntryRule::default()
            };
            assert_eq!(violations(&rule, "", 1, &none()).len(), 1);
            assert!(violations(&rule, "", 2, &none()).is_empty());
            assert_eq!(violations(&rule, "", 4, &none()).len(), 1);
        }

        #[test]
        fn should_require_a_value_for_each_field() {
            let rule = EntryRule {
                fields: vec!["owner".to_string(), "points".to_string()],
                ..EntryRule::default()
            };
            let mut values = none();
            values.insert("owner".to_string(), serde_json::json!("  "));
            values.insert("points".to_string(), serde_json::json!(0));
            let found = violations(&rule, "", 0, &values);
            assert_eq!(found, vec!["the field \"owner\" needs a value"]);
        }
    }

    mod validated {
        use super::*;

        fn known<'a>(items: &[&'a str]) -> BTreeSet<&'a str> {
            items.iter().copied().collect()
        }

        fn rules_for(column: &str, rule: EntryRule) -> ColumnRules {
            ColumnRules {
                columns: BTreeMap::from([(column.to_string(), rule)]),
                ..ColumnRules::default()
            }
        }

        #[test]
        fn should_trim_dedupe_and_drop_an_empty_rule() {
            let rules = ColumnRules {
                locked_labels: vec![" hold ".to_string(), "hold".to_string()],
                columns: BTreeMap::from([("c1".to_string(), EntryRule::default())]),
            };
            let rules = rules.validated(&known(&["c1"]), &known(&[])).unwrap();
            assert_eq!(rules.locked_labels, vec!["hold"]);
            assert!(rules.columns.is_empty());
        }

        #[test]
        fn should_refuse_an_unknown_column() {
            let rules = rules_for("nope", rule());
            let error = rules.validated(&known(&["c1"]), &known(&[])).unwrap_err();
            assert!(error.contains("unknown column"), "{error}");
        }

        #[test]
        fn should_refuse_an_unknown_field() {
            let rules = rules_for(
                "c1",
                EntryRule {
                    fields: vec!["ghost".to_string()],
                    ..EntryRule::default()
                },
            );
            let error = rules.validated(&known(&["c1"]), &known(&[])).unwrap_err();
            assert!(error.contains("unknown field"), "{error}");
        }

        #[test]
        fn should_refuse_a_min_above_the_max() {
            let rules = rules_for(
                "c1",
                EntryRule {
                    outcomes_min: Some(3),
                    outcomes_max: Some(2),
                    ..EntryRule::default()
                },
            );
            assert!(rules.validated(&known(&["c1"]), &known(&[])).is_err());
        }

        #[test]
        fn should_refuse_too_many_or_too_long_entries() {
            let many = ColumnRules {
                locked_labels: (0..=MAX_ITEMS).map(|n| format!("l{n}")).collect(),
                ..ColumnRules::default()
            };
            assert!(many.validated(&known(&[]), &known(&[])).is_err());
            let long = ColumnRules {
                locked_labels: vec!["x".repeat(MAX_ITEM_CHARS + 1)],
                ..ColumnRules::default()
            };
            assert!(long.validated(&known(&[]), &known(&[])).is_err());
        }
    }

    mod from_config {
        use super::*;

        #[test]
        fn should_default_when_the_bag_holds_no_rules() {
            assert_eq!(ColumnRules::from_config("{}"), ColumnRules::default());
            assert_eq!(
                ColumnRules::default().locked_labels,
                vec!["needs:human".to_string()]
            );
        }

        #[test]
        fn should_round_trip_through_the_bag() {
            let rules = ColumnRules {
                locked_labels: Vec::new(),
                columns: BTreeMap::from([("c1".to_string(), rule())]),
            };
            let bag = serde_json::json!({ CONFIG_KEY: rules.to_config_value() });
            assert_eq!(ColumnRules::from_config(&bag.to_string()), rules);
        }
    }

    mod config_without_field {
        use super::*;

        #[test]
        fn should_drop_the_key_and_an_emptied_rule() {
            let rules = ColumnRules {
                locked_labels: Vec::new(),
                columns: BTreeMap::from([
                    (
                        "c1".to_string(),
                        EntryRule {
                            fields: vec!["owner".to_string()],
                            ..EntryRule::default()
                        },
                    ),
                    ("c2".to_string(), rule()),
                ]),
            };
            let bag = serde_json::json!({ CONFIG_KEY: rules.to_config_value() });
            let config = config_without_field(&bag.to_string(), "owner").expect("changed");
            let stored = ColumnRules::from_config(&config);
            assert_eq!(stored.columns, BTreeMap::from([("c2".to_string(), rule())]));
        }

        #[test]
        fn should_return_none_when_no_rule_names_the_key() {
            let bag = serde_json::json!({ CONFIG_KEY: ColumnRules::default().to_config_value() });
            assert_eq!(config_without_field(&bag.to_string(), "owner"), None);
            assert_eq!(config_without_field("{}", "owner"), None);
        }
    }
}
