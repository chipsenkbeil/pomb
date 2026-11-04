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

            /// Returns the permission with wire name `name`.
            #[must_use]
            pub fn named(name: &str) -> Option<Self> {
                Self::ALL.iter().copied().find(|held| held.name() == name)
            }

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
    BoardsConfigure, "boards.configure", "boards", Admin, "Replace a board's config, change its settings and manage its members.";
    BoardsDelete, "boards.delete", "boards", Admin, "Delete a board.";
    ColumnsCreate, "columns.create", "columns", Lead, "Create a column, optionally marked as the done column.";
    ColumnsEdit, "columns.edit", "columns", Lead, "Rename a column and change its WIP limit, description, collapse state, config, done flag or Blocked flag.";
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
    LabelsManage, "labels.manage", "labels", Lead, "Highlight labels on a board, color them and add labels no task carries yet.";
    EpicsManage, "epics.manage", "epics", Lead, "Close, reopen or turn an epic back into a plain task.";
    TasksRead, "tasks.read", "tasks", Read, "Read tasks, subtasks and links.";
    TasksCreate, "tasks.create", "tasks", Work, "Create a task or an epic, and turn a subtask into a task.";
    TasksEdit, "tasks.edit", "tasks", Work, "Update a task, tick the boxes in its description, close and reopen an epic, bulk update and restore tasks.";
    TasksMove, "tasks.move", "tasks", Work, "Move a task to another column or position.";
    TasksClaim, "tasks.claim", "tasks", Work, "Claim a task, renew or release the claim, and watch the task.";
    TasksBin, "tasks.bin", "tasks", Lead, "Move a task to the recycle bin.";
    TasksDelete, "tasks.delete", "tasks", Admin, "Delete a task from the recycle bin for good.";
    SubtasksEdit, "subtasks.edit", "subtasks", Work, "Create, rename, assign, reorder and set the due date of subtasks, and delete the ones you created.";
    SubtasksCheck, "subtasks.check", "subtasks", Work, "Tick and untick subtasks.";
    SubtasksDelete, "subtasks.delete", "subtasks", Admin, "Delete any subtask, whoever created it.";
    LinksWrite, "links.write", "links", Work, "Create task links, and delete the ones you created.";
    LinksDelete, "links.delete", "links", Admin, "Delete any task link, whoever created it, and change a task's umbrella.";
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
    FeedsManage, "feeds.manage", "feeds", Admin, "List, create and delete a board's calendar feeds.";
    WebhooksManage, "webhooks.manage", "webhooks", Admin, "List, create, change and delete a board's webhooks, which send its events to other servers.";
    InboundManage, "inbound.manage", "inbound", Admin, "List, create, rotate and delete a board's inbound addresses, which turn forwarded mail into tasks.";
    TokensIssue, "tokens.issue", "tokens", Lead, "Create child tokens that hold no more than this token and expire within hours.";
    RolesGrant, "roles.grant", "roles", Lead, "Create the missing standard roles, and grant, revoke or end a role's tokens on boards this token holds.";
}

/// A named set of permissions, each holding everything the one before it holds.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Preset {
    /// Every `.read` permission.
    Read,
    /// Read, plus writing tasks, subtasks, links, comments, attachments and time.
    Work,
    /// Work, plus moving tasks to the recycle bin and creating and editing board structure.
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

    /// Returns the preset with wire name `name`.
    #[must_use]
    pub fn named(name: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|preset| preset.name() == name)
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

/// One of the six agent roles every account starts from.
#[derive(Copy, Clone, Debug)]
pub struct StandardRole {
    /// The role's name, its key.
    pub name: &'static str,
    /// The permissions the role holds beyond [`STANDARD_BASE`].
    pub extra: &'static [Permission],
}

impl StandardRole {
    /// Returns [`STANDARD_BASE`] plus the role's `extra` permissions.
    #[must_use]
    pub fn permissions(&self) -> BTreeSet<Permission> {
        STANDARD_BASE.iter().chain(self.extra).copied().collect()
    }
}

/// The permissions every standard role holds: reading tasks, comments and attachments, and
/// commenting.
pub const STANDARD_BASE: &[Permission] = &[
    Permission::TasksRead,
    Permission::CommentsRead,
    Permission::AttachmentsRead,
    Permission::CommentsWrite,
];

/// The six standard roles, from the planner who shapes a card to the lead who holds every part.
pub const STANDARD_ROLES: &[StandardRole] = &[
    StandardRole {
        name: "planner",
        extra: &[
            Permission::TasksCreate,
            Permission::TasksEdit,
            Permission::TasksMove,
            Permission::SubtasksEdit,
            Permission::LinksWrite,
            Permission::LabelsManage,
        ],
    },
    StandardRole {
        name: "implementer",
        extra: &[
            Permission::TasksClaim,
            Permission::TasksMove,
            Permission::AttachmentsWrite,
            Permission::TimeWrite,
        ],
    },
    StandardRole {
        name: "reviewer",
        extra: &[Permission::TasksClaim, Permission::TasksMove],
    },
    StandardRole {
        name: "verifier",
        extra: &[
            Permission::TasksClaim,
            Permission::TasksMove,
            Permission::SubtasksCheck,
        ],
    },
    StandardRole {
        name: "release",
        extra: &[Permission::TasksClaim, Permission::TasksMove],
    },
    StandardRole {
        name: "lead",
        extra: &[
            Permission::TasksCreate,
            Permission::TasksEdit,
            Permission::TasksMove,
            Permission::TasksClaim,
            Permission::SubtasksEdit,
            Permission::SubtasksCheck,
            Permission::LinksWrite,
            Permission::LabelsManage,
            Permission::AttachmentsWrite,
            Permission::TimeWrite,
            Permission::TokensIssue,
            Permission::RolesGrant,
        ],
    },
];

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    const NAMES: [&str; 53] = [
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
        "labels.manage",
        "epics.manage",
        "tasks.read",
        "tasks.create",
        "tasks.edit",
        "tasks.move",
        "tasks.claim",
        "tasks.bin",
        "tasks.delete",
        "subtasks.edit",
        "subtasks.check",
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
        "webhooks.manage",
        "inbound.manage",
        "tokens.issue",
        "roles.grant",
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
        let names: Vec<_> = Permission::ALL
            .iter()
            .map(|permission| permission.name())
            .collect();
        assert_eq!(names, NAMES);
    }

    #[test]
    fn names_should_be_unique_and_prefixed_by_their_group() {
        let unique: HashSet<_> = Permission::ALL
            .iter()
            .map(|permission| permission.name())
            .collect();
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
            "tasks.create",
            "tasks.edit",
            "tasks.move",
            "tasks.claim",
            "subtasks.edit",
            "subtasks.check",
            "links.write",
            "comments.write",
            "attachments.write",
            "time.write",
        ]);
        assert_eq!(added, expected);
    }

    #[test]
    fn lead_should_add_exactly_the_bin_and_the_structure_permissions() {
        let added: HashSet<_> = held(Preset::Lead)
            .difference(&held(Preset::Work))
            .copied()
            .collect();
        let expected = HashSet::from([
            "tasks.bin",
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
            "labels.manage",
            "epics.manage",
            "tokens.issue",
            "roles.grant",
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
            "webhooks.manage",
            "inbound.manage",
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

    #[test]
    fn each_standard_role_should_hold_exactly_its_listed_permissions_in_the_table() {
        let base = [
            "tasks.read",
            "comments.read",
            "attachments.read",
            "comments.write",
        ];
        let listed: [(&str, &[&str]); 6] = [
            (
                "planner",
                &[
                    "tasks.create",
                    "tasks.edit",
                    "tasks.move",
                    "subtasks.edit",
                    "links.write",
                    "labels.manage",
                ],
            ),
            (
                "implementer",
                &[
                    "tasks.claim",
                    "tasks.move",
                    "attachments.write",
                    "time.write",
                ],
            ),
            ("reviewer", &["tasks.claim", "tasks.move"]),
            ("verifier", &["tasks.claim", "tasks.move", "subtasks.check"]),
            ("release", &["tasks.claim", "tasks.move"]),
            (
                "lead",
                &[
                    "tasks.create",
                    "tasks.edit",
                    "tasks.move",
                    "tasks.claim",
                    "subtasks.edit",
                    "subtasks.check",
                    "links.write",
                    "labels.manage",
                    "attachments.write",
                    "time.write",
                    "tokens.issue",
                    "roles.grant",
                ],
            ),
        ];
        let names: Vec<_> = STANDARD_ROLES.iter().map(|role| role.name).collect();
        assert_eq!(names, listed.map(|(name, _)| name));
        for (role, (name, extra)) in STANDARD_ROLES.iter().zip(listed) {
            let expected: HashSet<_> = base.iter().chain(extra).copied().collect();
            let held: HashSet<_> = role
                .permissions()
                .iter()
                .map(|permission| permission.name())
                .collect();
            assert_eq!(held, expected, "{name}");
        }
    }
}
