use crate::db::Db;

/// The category of the templates an account saved.
pub const YOUR_BOARDS: &str = "Your boards";

/// A group of built-in layouts on the Create board page.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Category {
    General,
    Marketing,
    Sales,
    Operations,
    Support,
    PeopleAndHr,
    Finance,
    SoftwareDevelopment,
    PersonalProductivity,
}

impl Category {
    /// Every category in the order the page lists them after `"Your boards"`.
    pub const ALL: [Self; 9] = [
        Self::General,
        Self::Marketing,
        Self::Sales,
        Self::Operations,
        Self::Support,
        Self::PeopleAndHr,
        Self::Finance,
        Self::SoftwareDevelopment,
        Self::PersonalProductivity,
    ];

    /// Returns the name the page shows for the category.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Marketing => "Marketing",
            Self::Sales => "Sales",
            Self::Operations => "Operations",
            Self::Support => "Support",
            Self::PeopleAndHr => "People and HR",
            Self::Finance => "Finance",
            Self::SoftwareDevelopment => "Software development",
            Self::PersonalProductivity => "Personal productivity",
        }
    }
}

/// Returns the labels of the filter chips after `"All"`, in page order.
#[must_use]
pub fn chip_labels() -> Vec<&'static str> {
    std::iter::once(YOUR_BOARDS)
        .chain(Category::ALL.map(Category::label))
        .collect()
}

/// One built-in board layout.
#[derive(Debug)]
pub struct Layout {
    /// The name shown on the Create board page.
    pub name: &'static str,
    /// The group the layout appears under.
    pub category: Category,
    /// One sentence on what the layout is for.
    pub description: &'static str,
    /// The column names from left to right. The last column is the Done column.
    pub columns: &'static [&'static str],
}

/// Every built-in layout that is only columns on the simple palette.
pub const LAYOUTS: [Layout; 20] = [
    Layout {
        name: "Project management",
        category: Category::General,
        description: "Plan work from a backlog through review to done.",
        columns: &["Backlog", "To-do", "In progress", "Review", "Done"],
    },
    Layout {
        name: "Weekly planning",
        category: Category::General,
        description: "A column for each weekday, with a backlog beside them.",
        columns: &[
            "Backlog",
            "Monday",
            "Tuesday",
            "Wednesday",
            "Thursday",
            "Friday",
            "Done",
        ],
    },
    Layout {
        name: "Blank",
        category: Category::General,
        description: "Two columns to start from and shape yourself.",
        columns: &["To-do", "Done"],
    },
    Layout {
        name: "Campaign management",
        category: Category::Marketing,
        description: "Follow a campaign from idea to published and measured.",
        columns: &["Ideas", "Planning", "Creating", "Review", "Published"],
    },
    Layout {
        name: "Content calendar",
        category: Category::Marketing,
        description: "Move posts and articles from draft to scheduled and live.",
        columns: &["Ideas", "Drafting", "Editing", "Scheduled", "Published"],
    },
    Layout {
        name: "Sales pipeline",
        category: Category::Sales,
        description: "Track each deal from lead to won.",
        columns: &["Leads", "Contacted", "Proposal sent", "Negotiation", "Won"],
    },
    Layout {
        name: "Account follow-up",
        category: Category::Sales,
        description: "Keep track of who to contact and who owes a reply.",
        columns: &[
            "To contact",
            "Waiting for reply",
            "Meeting booked",
            "Closed",
        ],
    },
    Layout {
        name: "Order fulfillment",
        category: Category::Operations,
        description: "Take an order from received to delivered.",
        columns: &["New orders", "Picking", "Packed", "Shipped", "Delivered"],
    },
    Layout {
        name: "Maintenance requests",
        category: Category::Operations,
        description: "Log, schedule and close repair and upkeep jobs.",
        columns: &["Reported", "Scheduled", "In progress", "Done"],
    },
    Layout {
        name: "Customer support",
        category: Category::Support,
        description: "Work tickets from new to resolved.",
        columns: &["New", "Investigating", "Waiting on customer", "Resolved"],
    },
    Layout {
        name: "Feature requests",
        category: Category::Support,
        description: "Collect requests and show what is planned and shipped.",
        columns: &["Requested", "Under review", "Planned", "Shipped"],
    },
    Layout {
        name: "Hiring",
        category: Category::PeopleAndHr,
        description: "Move candidates from application to hired.",
        columns: &["Applied", "Screening", "Interview", "Offer", "Hired"],
    },
    Layout {
        name: "Onboarding",
        category: Category::PeopleAndHr,
        description: "Prepare and track a new hire's first weeks.",
        columns: &["To prepare", "Week one", "First month", "Complete"],
    },
    Layout {
        name: "Supplier invoices",
        category: Category::Finance,
        description: "Approve and pay the invoices you receive.",
        columns: &["Received", "Approved", "Scheduled", "Paid"],
    },
    Layout {
        name: "Expense reports",
        category: Category::Finance,
        description: "Review and reimburse submitted expenses.",
        columns: &["Submitted", "Under review", "Approved", "Reimbursed"],
    },
    Layout {
        name: "Software development",
        category: Category::SoftwareDevelopment,
        description: "Take work from the backlog through code review and testing to release.",
        columns: &[
            "Backlog",
            "Selected",
            "In progress",
            "Code review",
            "Testing",
            "Released",
        ],
    },
    Layout {
        name: "Bug tracking",
        category: Category::SoftwareDevelopment,
        description: "Confirm, fix and verify reported bugs.",
        columns: &["Reported", "Confirmed", "Fixing", "Verifying", "Closed"],
    },
    Layout {
        name: "Today and this week",
        category: Category::PersonalProductivity,
        description: "Pull work from later into this week and today.",
        columns: &["Later", "This week", "Today", "Doing", "Done"],
    },
    Layout {
        name: "Reading list",
        category: Category::PersonalProductivity,
        description: "Books and articles to read, being read and finished.",
        columns: &["To read", "Reading", "Finished"],
    },
    Layout {
        name: "Meal planning",
        category: Category::PersonalProductivity,
        description: "Collect recipes and plan the week's meals.",
        columns: &["Recipes", "This week", "Cooked"],
    },
];

/// Returns the category label of the built-in template called `name`, or [`YOUR_BOARDS`] when it is
/// not one.
#[must_use]
pub fn category_of(name: &str) -> &'static str {
    let category = match name {
        Db::BUILTIN_TEMPLATE_NAME => Category::PersonalProductivity,
        Db::KANBAN_BASICS_TEMPLATE_NAME => Category::General,
        _ => match LAYOUTS.iter().find(|layout| layout.name == name) {
            Some(layout) => layout.category,
            None => return YOUR_BOARDS,
        },
    };
    category.label()
}
