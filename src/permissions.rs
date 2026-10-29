//! The catalog of token permissions and the presets that group them.
//!
//! A [`Permission`] names one capability by a stable `group.action` wire name. A [`Preset`] is the
//! set of permissions a scope-like label stands for: each preset holds everything the one below it
//! holds, and `admin` holds every permission.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

macro_rules! catalog {
    ($($variant:ident, $name:literal, $group:literal, $preset:ident, $description:literal;)*) => {
        /// One capability a token can hold.
        #[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        pub enum Permission {
            $(
                #[doc = $description]
                #[serde(rename = $name)]
                $variant,
            )*
        }

        impl Permission {
            /// Every permission, in catalog order.
            pub const ALL: &'static [Permission] = &[$(Permission::$variant),*];

            /// Returns the stable wire name, `group.action`.
            #[must_use]
            pub const fn name(self) -> &'static str {
                match self {
                    $(Permission::$variant => $name,)*
                }
            }

            /// Returns the group, the part of the wire name before the dot.
            #[must_use]
            pub const fn group(self) -> &'static str {
                match self {
                    $(Permission::$variant => $group,)*
                }
            }

            /// Returns a one-line description of what the permission allows.
            #[must_use]
            pub const fn description(self) -> &'static str {
                match self {
                    $(Permission::$variant => $description,)*
                }
            }

            /// Returns the smallest preset that holds this permission.
            #[must_use]
            pub const fn lowest_preset(self) -> Preset {
                match self {
                    $(Permission::$variant => Preset::$preset,)*
                }
            }
        }
    };
}

catalog! {
    BoardsRead, "boards.read", "boards", Read, "List and get boards, columns, swimlanes, palette, labels, fields and templates, and export board data.";
    BoardsCreate, "boards.create", "boards", Lead, "Create a board, blank or from a template.";
    BoardsConfigure, "boards.configure", "boards", Admin, "Replace a board's config and change its settings.";
    BoardsDelete, "boards.delete", "boards", Admin, "Delete a board.";
    ColumnsCreate, "columns.create", "columns", Lead, "Create a column, optionally marked as the done column.";
    ColumnsEdit, "columns.edit", "columns", Lead, "Rename a column and change its WIP limit, description, collapse state, config or done flag.";
    ColumnsReorder, "columns.reorder", "columns", Lead, "Reorder a board's columns.";
    ColumnsDelete, "columns.delete", "columns", Admin, "Delete a column.";
    SwimlanesCreate, "swimlanes.create", "swimlanes", Lead, "Create a swimlane.";
    SwimlanesEdit, "swimlanes.edit", "swimlanes", Lead, "Rename and reorder swimlanes.";
    SwimlanesDelete, "swimlanes.delete", "swimlanes", Admin, "Delete a swimlane.";
    ColorsCreate, "colors.create", "colors", Lead, "Create a palette color.";
    ColorsEdit, "colors.edit", "colors", Lead, "Change a color's label, description, enabled state and default, and reorder the palette.";
    ColorsCopy, "colors.copy", "colors", Admin, "Copy the palette from another board.";
    ColorsDelete, "colors.delete", "colors", Admin, "Delete a palette color.";
    FieldsCreate, "fields.create", "fields", Lead, "Create a custom field.";
    FieldsEdit, "fields.edit", "fields", Lead, "Add options and optional table columns, and change a field's label, required flag, default and position.";
    FieldsRemoveOption, "fields.remove_option", "fields", Admin, "Remove or rename a field's options.";
    FieldsDelete, "fields.delete", "fields", Admin, "Delete a custom field.";
    TemplatesSave, "templates.save", "templates", Lead, "Save a board as a template.";
    TemplatesDelete, "templates.delete", "templates", Admin, "Delete a saved template.";
    TasksRead, "tasks.read", "tasks", Read, "Read tasks, subtasks and links.";
    TasksWrite, "tasks.write", "tasks", Work, "Create, update, move, watch, claim, heartbeat and release tasks.";
    TasksDelete, "tasks.delete", "tasks", Admin, "Delete a task.";
    SubtasksWrite, "subtasks.write", "subtasks", Work, "Create, update and reorder subtasks.";
    SubtasksDelete, "subtasks.delete", "subtasks", Admin, "Delete a subtask.";
    LinksWrite, "links.write", "links", Work, "Create task links.";
    LinksDelete, "links.delete", "links", Admin, "Delete a task link.";
    CommentsRead, "comments.read", "comments", Read, "Read comments.";
    CommentsWrite, "comments.write", "comments", Work, "Create comments, and edit or delete your own.";
    CommentsEditAny, "comments.edit_any", "comments", Admin, "Edit any comment, whoever wrote it.";
    CommentsDelete, "comments.delete", "comments", Admin, "Delete any comment, whoever wrote it.";
    AttachmentsRead, "attachments.read", "attachments", Read, "Download attachments.";
    AttachmentsWrite, "attachments.write", "attachments", Work, "Upload and rename attachments.";
    AttachmentsDelete, "attachments.delete", "attachments", Admin, "Delete an attachment.";
    TimeRead, "time.read", "time", Read, "Read time entries and the timer status.";
    TimeWrite, "time.write", "time", Work, "Add a manual entry, log time on a task, and start, stop or retarget the timer.";
    TimeEdit, "time.edit", "time", Admin, "Edit a time entry.";
    TimeDelete, "time.delete", "time", Admin, "Delete a time entry.";
    SettingsRead, "settings.read", "settings", Read, "Read the account settings.";
    SettingsWrite, "settings.write", "settings", Admin, "Change the account settings.";
    FeedsManage, "feeds.manage", "feeds", Admin, "Get, create and reset a board's calendar feed link.";
}

/// A named set of permissions, each holding everything the one before it holds.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Preset {
    /// Every `.read` permission.
    Read,
    /// Read, plus writing tasks, subtasks, links, comments, attachments and time.
    Work,
    /// Work, plus creating and editing board structure.
    Lead,
    /// Every permission.
    Admin,
}

impl Preset {
    /// Every preset, from the smallest set to the largest.
    pub const ALL: &'static [Preset] = &[Preset::Read, Preset::Work, Preset::Lead, Preset::Admin];

    /// Returns the preset's wire name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Preset::Read => "read",
            Preset::Work => "work",
            Preset::Lead => "lead",
            Preset::Admin => "admin",
        }
    }

    /// Returns every permission this preset holds.
    #[must_use]
    pub fn permissions(self) -> BTreeSet<Permission> {
        Permission::ALL
            .iter()
            .copied()
            .filter(|permission| self.contains(*permission))
            .collect()
    }

    /// Returns whether this preset holds `permission`.
    #[must_use]
    pub fn contains(self, permission: Permission) -> bool {
        self >= permission.lowest_preset()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    const NAMES: [&str; 42] = [
        "boards.read",
        "boards.create",
        "boards.configure",
        "boards.delete",
        "columns.create",
        "columns.edit",
        "columns.reorder",
        "columns.delete",
        "swimlanes.create",
        "swimlanes.edit",
        "swimlanes.delete",
        "colors.create",
        "colors.edit",
        "colors.copy",
        "colors.delete",
        "fields.create",
        "fields.edit",
        "fields.remove_option",
        "fields.delete",
        "templates.save",
        "templates.delete",
        "tasks.read",
        "tasks.write",
        "tasks.delete",
        "subtasks.write",
        "subtasks.delete",
        "links.write",
        "links.delete",
        "comments.read",
        "comments.write",
        "comments.edit_any",
        "comments.delete",
        "attachments.read",
        "attachments.write",
        "attachments.delete",
        "time.read",
        "time.write",
        "time.edit",
        "time.delete",
        "settings.read",
        "settings.write",
        "feeds.manage",
    ];

    fn held(preset: Preset) -> HashSet<&'static str> {
        Permission::ALL
            .iter()
            .filter(|permission| preset.contains(**permission))
            .map(|permission| permission.name())
            .collect()
    }

    #[test]
    fn catalog_should_match_the_name_snapshot() {
        let names: Vec<_> = Permission::ALL.iter().map(|p| p.name()).collect();
        assert_eq!(names, NAMES);
    }

    #[test]
    fn names_should_be_unique_and_prefixed_by_their_group() {
        let unique: HashSet<_> = Permission::ALL.iter().map(|p| p.name()).collect();
        assert_eq!(unique.len(), Permission::ALL.len());
        for permission in Permission::ALL {
            let (group, action) = permission.name().split_once('.').expect("group.action");
            assert_eq!(group, permission.group());
            assert!(!action.is_empty());
            assert!(!permission.description().is_empty());
        }
    }

    #[test]
    fn admin_should_hold_every_permission() {
        assert_eq!(held(Preset::Admin).len(), Permission::ALL.len());
    }

    #[test]
    fn presets_should_nest_from_read_to_admin() {
        let sets: Vec<_> = Preset::ALL.iter().map(|preset| held(*preset)).collect();
        for pair in sets.windows(2) {
            assert!(pair[0].is_subset(&pair[1]));
            assert!(pair[0].len() < pair[1].len());
        }
    }

    #[test]
    fn read_should_hold_exactly_the_read_permissions() {
        let reads: HashSet<_> = Permission::ALL
            .iter()
            .map(|p| p.name())
            .filter(|name| name.ends_with(".read"))
            .collect();
        assert_eq!(held(Preset::Read), reads);
        assert_eq!(reads.len(), 6);
    }

    #[test]
    fn work_should_add_exactly_the_write_permissions() {
        let added: HashSet<_> = held(Preset::Work)
            .difference(&held(Preset::Read))
            .copied()
            .collect();
        let expected = HashSet::from([
            "tasks.write",
            "subtasks.write",
            "links.write",
            "comments.write",
            "attachments.write",
            "time.write",
        ]);
        assert_eq!(added, expected);
    }

    #[test]
    fn lead_should_add_exactly_the_structure_permissions() {
        let added: HashSet<_> = held(Preset::Lead)
            .difference(&held(Preset::Work))
            .copied()
            .collect();
        let expected = HashSet::from([
            "boards.create",
            "columns.create",
            "columns.edit",
            "columns.reorder",
            "swimlanes.create",
            "swimlanes.edit",
            "colors.create",
            "colors.edit",
            "fields.create",
            "fields.edit",
            "templates.save",
        ]);
        assert_eq!(added, expected);
    }

    #[test]
    fn only_admin_should_hold_a_delete_permission() {
        for preset in [Preset::Read, Preset::Work, Preset::Lead] {
            assert!(
                held(preset).iter().all(|name| !name.ends_with(".delete")),
                "{}",
                preset.name()
            );
        }
        for permission in Permission::ALL {
            if permission.name().ends_with(".delete") {
                assert_eq!(permission.lowest_preset(), Preset::Admin);
            }
        }
    }

    #[test]
    fn admin_alone_should_hold_the_sensitive_permissions() {
        for name in [
            "boards.configure",
            "colors.copy",
            "fields.remove_option",
            "comments.edit_any",
            "time.edit",
            "settings.write",
            "feeds.manage",
        ] {
            assert!(held(Preset::Admin).contains(name), "{name}");
            assert!(!held(Preset::Lead).contains(name), "{name}");
        }
    }

    #[test]
    fn permission_should_round_trip_through_its_wire_name() {
        for permission in Permission::ALL {
            let json = serde_json::to_string(permission).unwrap();
            assert_eq!(json, format!("\"{}\"", permission.name()));
            let back: Permission = serde_json::from_str(&json).unwrap();
            assert_eq!(back, *permission);
        }
        assert!(serde_json::from_str::<Permission>("\"boards.nope\"").is_err());
    }

    #[test]
    fn preset_permissions_should_match_the_contains_filter() {
        for preset in Preset::ALL {
            let filtered: BTreeSet<Permission> = Permission::ALL
                .iter()
                .copied()
                .filter(|permission| preset.contains(*permission))
                .collect();
            assert_eq!(preset.permissions(), filtered);
        }
    }

    #[test]
    fn preset_names_should_be_stable() {
        let names: Vec<_> = Preset::ALL.iter().map(|preset| preset.name()).collect();
        assert_eq!(names, ["read", "work", "lead", "admin"]);
    }
}
