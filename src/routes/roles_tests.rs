use axum::http::{Method, StatusCode};
use serde_json::{Value, json};

use super::passkey_tests::{Reply, call};
use super::server::{passkey_mode, unthrottled_limits};
use super::{ROUTE_ACCESS, RouteClass, router};
use crate::AppState;
use crate::auth::{new_api_token_record, required_role};
use crate::db::{Db, LinkAddOutcome, NewTimeEntry};
use crate::models::{
    AccountId, ActiveTimer, Actor, AuthScope, BoardRole, EntryKind, HistoryKind, LinkKind,
    TimerMode,
};

/// A board of alice with a column and a task, a member bob, a viewer carol and a stranger dave.
struct Fixture {
    _dir: tempfile::TempDir,
    app: axum::Router,
    db: Db,
    alice: AccountId,
    bob: AccountId,
    carol: AccountId,
    tokens: [String; 4],
    board: String,
    column: String,
    task: String,
}

const OWNER: usize = 0;
const MEMBER: usize = 1;
const VIEWER: usize = 2;
const STRANGER: usize = 3;

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("roles.redb");
        let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
        let alice = db.create_user("alice").expect("alice");
        let bob = db.create_user("bob").expect("bob");
        let carol = db.create_user("carol").expect("carol");
        let dave = db.create_user("dave").expect("dave");
        let board = db.create_board(&alice, "Shared").expect("board");
        let column = db.create_column(&board, "Todo", None).expect("column");
        let task = db
            .create_task(&column, None, "Card", 1, None)
            .expect("task");
        db.set_board_member(&board, "bob", BoardRole::Member)
            .expect("bob");
        db.set_board_member(&board, "carol", BoardRole::Viewer)
            .expect("carol");
        let token = |account: &AccountId| {
            let (record, raw) = new_api_token_record(account, "agent", AuthScope::Admin, None);
            db.create_api_token(&record).expect("token");
            raw
        };
        let tokens = [token(&alice), token(&bob), token(&carol), token(&dave)];
        let app = router(
            AppState::new(db.clone(), None, passkey_mode()).with_limits(unthrottled_limits()),
        );
        Self {
            _dir: dir,
            app,
            db,
            alice,
            bob,
            carol,
            tokens,
            board,
            column,
            task,
        }
    }

    async fn send(&self, who: usize, method: &str, uri: &str, body: Option<Value>) -> Reply {
        let bearer = format!("Bearer {}", self.tokens[who]);
        call(&self.app, method, uri, &[("authorization", &bearer)], body).await
    }

    fn fill(&self, uri: &str) -> String {
        uri.replace("{board}", &self.board)
            .replace("{task}", &self.task)
            .replace("{column}", &self.column)
    }
}

fn entry(task: &str) -> NewTimeEntry<'_> {
    NewTimeEntry {
        task_id: Some(task),
        started_at: "2026-10-29T10:00:00+00:00",
        minutes: 5,
        seconds: 300,
        note: "",
        kind: EntryKind::Manual,
        interrupted: false,
        interrupt_reason: None,
        labels: &[],
        created_by: "",
        activity_id: None,
        role: None,
        token_name: None,
        role_id: None,
        role_name: None,
    }
}

#[test]
fn required_role_should_hold_every_board_task_and_entity_write_at_member_or_above() {
    for (method, pattern, class, _) in ROUTE_ACCESS {
        let shares = matches!(
            class,
            RouteClass::Board | RouteClass::Task | RouteClass::Entity
        );
        let leaves = (*method == Method::DELETE && *pattern == "/api/boards/:id/members")
            || (*method == Method::PATCH && *pattern == "/api/columns/:id");
        if shares && *method != Method::GET && !leaves {
            assert!(
                required_role(method, pattern) >= BoardRole::Member,
                "{method} {pattern}"
            );
        }
    }
}

#[test]
fn required_role_should_hold_the_owner_settings_pages_and_the_calendar_secret_at_owner() {
    for pattern in [
        "/b/:board_id/settings",
        "/b/:board_id/settings/rules",
        "/b/:board_id/settings/delete",
        "/api/boards/:id/calendar-feeds",
        "/api/boards/:id/inbound-addresses",
    ] {
        assert_eq!(
            required_role(&Method::GET, pattern),
            BoardRole::Owner,
            "{pattern}"
        );
    }
    assert_eq!(
        required_role(&Method::GET, "/b/:board_id"),
        BoardRole::Viewer
    );
}

#[test]
fn required_role_should_fail_closed_for_a_route_the_table_does_not_list() {
    assert_eq!(
        required_role(&Method::GET, "/api/unlisted"),
        BoardRole::Owner
    );
}

#[tokio::test]
async fn routes_should_let_each_role_do_only_what_it_may() {
    let fx = Fixture::new();
    let rows: [(&str, &str, Option<Value>, BoardRole); 10] = [
        ("GET", "/api/tasks/{task}", None, BoardRole::Viewer),
        ("GET", "/api/boards/{board}/tasks", None, BoardRole::Viewer),
        (
            "GET",
            "/api/boards/{board}/members",
            None,
            BoardRole::Viewer,
        ),
        (
            "PATCH",
            "/api/tasks/{task}",
            Some(json!({"name": "Renamed"})),
            BoardRole::Member,
        ),
        (
            "POST",
            "/api/tasks/{task}/comments",
            Some(json!({"body": "hello"})),
            BoardRole::Member,
        ),
        (
            "POST",
            "/api/tasks/{task}/time",
            Some(json!({"minutes": 5})),
            BoardRole::Member,
        ),
        (
            "PUT",
            "/api/boards/{board}/config",
            Some(json!({"legend_visible": true})),
            BoardRole::Owner,
        ),
        (
            "PATCH",
            "/api/columns/{column}",
            Some(json!({"name": "Renamed"})),
            BoardRole::Owner,
        ),
        (
            "GET",
            "/api/boards/{board}/calendar-feeds",
            None,
            BoardRole::Owner,
        ),
        (
            "GET",
            "/api/boards/{board}/inbound-addresses",
            None,
            BoardRole::Owner,
        ),
    ];
    for (method, uri, body, lowest) in rows {
        let uri = fx.fill(uri);
        for (who, role) in [(VIEWER, BoardRole::Viewer), (MEMBER, BoardRole::Member)] {
            let reply = fx.send(who, method, &uri, body.clone()).await;
            if role >= lowest {
                assert!(
                    reply.status.is_success(),
                    "{role:?} {method} {uri}: {} {}",
                    reply.status,
                    reply.body
                );
            } else {
                assert_eq!(
                    reply.status,
                    StatusCode::FORBIDDEN,
                    "{role:?} {method} {uri}: {}",
                    reply.body
                );
            }
        }
        let stranger = fx.send(STRANGER, method, &uri, body.clone()).await;
        assert_eq!(
            stranger.status,
            StatusCode::NOT_FOUND,
            "stranger {method} {uri}: {}",
            stranger.body
        );
        let owner = fx.send(OWNER, method, &uri, body).await;
        assert!(
            owner.status.is_success(),
            "owner {method} {uri}: {} {}",
            owner.status,
            owner.body
        );
    }
}

#[tokio::test]
async fn a_viewer_write_should_refuse_with_the_role_and_leave_the_task_unchanged() {
    let fx = Fixture::new();
    let name = |db: &Db, id: &str| db.task(id).expect("read").expect("task").name;
    let before = name(&fx.db, &fx.task);
    let uri = fx.fill("/api/tasks/{task}");
    let reply = fx
        .send(VIEWER, "PATCH", &uri, Some(json!({"name": "Renamed"})))
        .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN, "{}", reply.body);
    assert!(
        reply.body.to_string().contains("role"),
        "names the role: {}",
        reply.body
    );
    assert_eq!(name(&fx.db, &fx.task), before);
}

#[tokio::test]
async fn a_member_should_leave_a_board_but_not_remove_another_account() {
    let fx = Fixture::new();
    let uri = fx.fill("/api/boards/{board}/members?username=carol");
    let refused = fx.send(MEMBER, "DELETE", &uri, None).await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.body);
    let own = fx.fill("/api/boards/{board}/members?username=bob");
    let left = fx.send(MEMBER, "DELETE", &own, None).await;
    assert_eq!(left.status, StatusCode::OK, "{}", left.body);
    let after = fx
        .send(MEMBER, "GET", &fx.fill("/api/tasks/{task}"), None)
        .await;
    assert_eq!(after.status, StatusCode::NOT_FOUND, "{}", after.body);
    let viewer_leaves = fx
        .send(
            VIEWER,
            "DELETE",
            &fx.fill("/api/boards/{board}/members?username=carol"),
            None,
        )
        .await;
    assert_eq!(
        viewer_leaves.status,
        StatusCode::OK,
        "{}",
        viewer_leaves.body
    );
}

#[tokio::test]
async fn a_member_should_see_only_the_time_it_logged_itself() {
    let fx = Fixture::new();
    let uri = fx.fill("/api/tasks/{task}/time");
    for (who, minutes) in [(OWNER, 7), (MEMBER, 5)] {
        let logged = fx
            .send(who, "POST", &uri, Some(json!({"minutes": minutes})))
            .await;
        assert!(logged.status.is_success(), "{}", logged.body);
    }
    let minutes_of = |account: &AccountId| {
        fx.db
            .account(account)
            .needing(BoardRole::Viewer)
            .list_entries(&fx.task)
            .expect("entries")
            .iter()
            .map(|entry| entry.minutes)
            .collect::<Vec<_>>()
    };
    assert_eq!(minutes_of(&fx.alice), [7]);
    assert_eq!(minutes_of(&fx.bob), [5]);
    let total = |account: &AccountId| {
        fx.db
            .account(account)
            .needing(BoardRole::Viewer)
            .board_task(&fx.task)
            .expect("task")
            .expect("reachable")
            .total_minutes
    };
    assert_eq!((total(&fx.alice), total(&fx.bob)), (7, 5));
}

#[test]
fn account_db_should_refuse_a_time_entry_by_a_viewer_and_a_stranger() {
    let fx = Fixture::new();
    let stranger = fx.db.account(&AccountId::new("nobody"));
    assert!(
        stranger
            .create_entry_full(entry(&fx.task))
            .expect("write")
            .is_none()
    );
    let member = fx.db.account(&fx.bob).needing(BoardRole::Member);
    assert!(
        member
            .create_entry_full(entry(&fx.task))
            .expect("write")
            .is_some()
    );
    let viewer = fx.db.account(&fx.carol).needing(BoardRole::Member);
    assert!(viewer.create_entry_full(entry(&fx.task)).is_err());
}

#[test]
fn deleting_a_task_should_stop_the_timer_of_each_member_running_on_it() {
    let fx = Fixture::new();
    let timer = ActiveTimer {
        task_id: Some(fx.task.clone()),
        mode: TimerMode::Stopwatch,
        started_at: 0,
        duration_secs: None,
    };
    fx.db.set_active_timer(&fx.bob, &timer).expect("timer");
    assert!(fx.db.delete_task(&fx.task, Actor::SYSTEM).expect("delete"));
    assert!(fx.db.active_timer(&fx.bob).expect("read").is_none());
}

#[test]
fn demoting_a_member_to_viewer_should_stop_its_timer() {
    let fx = Fixture::new();
    let timer = ActiveTimer {
        task_id: Some(fx.task.clone()),
        mode: TimerMode::Stopwatch,
        started_at: 0,
        duration_secs: None,
    };
    fx.db.set_active_timer(&fx.bob, &timer).expect("timer");
    fx.db
        .set_board_member(&fx.board, "bob", BoardRole::Viewer)
        .expect("demote");
    assert!(fx.db.active_timer(&fx.bob).expect("read").is_none());
}

#[tokio::test]
async fn task_listings_should_include_the_cards_of_a_joined_board() {
    let fx = Fixture::new();
    let listed = fx.send(MEMBER, "GET", "/api/tasks", None).await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.body);
    assert!(
        listed.body.to_string().contains(&fx.task),
        "{}",
        listed.body
    );
    let search = fx.send(VIEWER, "GET", "/api/search?q=Card", None).await;
    assert_eq!(search.status, StatusCode::OK, "{}", search.body);
    assert!(search.body.to_string().contains("Card"), "{}", search.body);
}

#[test]
fn moving_a_task_to_a_board_a_member_cannot_reach_should_stop_that_members_timer() {
    let fx = Fixture::new();
    let timer = ActiveTimer {
        task_id: Some(fx.task.clone()),
        mode: TimerMode::Stopwatch,
        started_at: 0,
        duration_secs: None,
    };
    fx.db.set_active_timer(&fx.bob, &timer).expect("timer");
    let elsewhere = fx.db.create_board(&fx.alice, "Private").expect("board");
    let column = fx
        .db
        .create_column(&elsewhere, "Todo", None)
        .expect("column");
    fx.db.move_task(&fx.task, &column, None, 0.0).expect("move");
    assert!(fx.db.active_timer(&fx.bob).expect("read").is_none());
}

#[tokio::test]
async fn a_member_should_not_delete_a_task_or_link_that_rewrites_a_board_it_only_reads() {
    let fx = Fixture::new();
    let reading = fx.db.create_board(&fx.alice, "Reading").expect("board");
    let column = fx.db.create_column(&reading, "Todo", None).expect("column");
    let other = fx
        .db
        .create_task(&column, None, "Other", 1, None)
        .expect("task");
    fx.db
        .set_board_member(&reading, "bob", BoardRole::Viewer)
        .expect("viewer");
    let added = fx
        .db
        .add_task_link(&fx.task, LinkKind::Related, &other, None, Actor::SYSTEM)
        .expect("link");
    assert!(matches!(added, LinkAddOutcome::Added(_)));
    let uri = fx.fill("/api/tasks/{task}");
    let refused = fx.send(MEMBER, "DELETE", &uri, None).await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.body);
    assert!(fx.db.task(&fx.task).expect("read").is_some());
}

#[tokio::test]
async fn a_locked_card_should_refuse_a_member_token_as_locked_and_a_viewer_as_role_denied() {
    let fx = Fixture::new();
    let uri = fx.fill("/api/tasks/{task}");
    let label = Some(json!({"labels": ["needs:human"]}));
    let locked = fx.send(OWNER, "PATCH", &uri, label).await;
    assert_eq!(locked.status, StatusCode::OK, "{}", locked.body);
    let patch = Some(json!({"name": "Renamed"}));
    let member = fx.send(MEMBER, "PATCH", &uri, patch.clone()).await;
    assert_eq!(member.status, StatusCode::FORBIDDEN, "{}", member.body);
    assert_eq!(member.code.as_deref(), Some("card_locked"));
    let viewer = fx.send(VIEWER, "PATCH", &uri, patch).await;
    assert_eq!(viewer.status, StatusCode::FORBIDDEN, "{}", viewer.body);
    assert_ne!(viewer.code.as_deref(), Some("card_locked"));
}

#[test]
fn a_removed_member_should_not_edit_or_delete_its_old_time_entries() {
    let fx = Fixture::new();
    let id = fx
        .db
        .account(&fx.bob)
        .needing(BoardRole::Member)
        .create_entry_full(entry(&fx.task))
        .expect("write")
        .expect("created");
    assert!(
        fx.db
            .account(&fx.bob)
            .needing(BoardRole::Member)
            .entry(&id)
            .expect("read")
            .is_some()
    );
    fx.db
        .remove_board_member(&fx.board, &fx.bob)
        .expect("remove");
    assert!(
        fx.db
            .account(&fx.bob)
            .needing(BoardRole::Member)
            .entry(&id)
            .expect("read")
            .is_none()
    );
}

#[test]
fn moving_a_task_to_a_board_a_member_only_reads_should_stop_that_members_timer() {
    let fx = Fixture::new();
    let timer = ActiveTimer {
        task_id: Some(fx.task.clone()),
        mode: TimerMode::Stopwatch,
        started_at: 0,
        duration_secs: None,
    };
    fx.db.set_active_timer(&fx.bob, &timer).expect("timer");
    let reading = fx.db.create_board(&fx.alice, "Reading").expect("board");
    fx.db
        .set_board_member(&reading, "bob", BoardRole::Viewer)
        .expect("viewer");
    let column = fx.db.create_column(&reading, "Todo", None).expect("column");
    fx.db.move_task(&fx.task, &column, None, 0.0).expect("move");
    assert!(fx.db.active_timer(&fx.bob).expect("read").is_none());
}

#[test]
fn changing_an_umbrella_should_refuse_a_member_without_a_role_on_the_old_parents_board() {
    let fx = Fixture::new();
    let elsewhere = fx.db.create_board(&fx.alice, "Private").expect("board");
    let far = fx
        .db
        .create_column(&elsewhere, "Todo", None)
        .expect("column");
    let parent = fx
        .db
        .create_task(&far, None, "Parent", 1, None)
        .expect("task");
    let added = fx
        .db
        .add_task_link(&fx.task, LinkKind::Parent, &parent, None, Actor::SYSTEM)
        .expect("link");
    assert!(matches!(added, LinkAddOutcome::Added(_)));
    let umbrella = fx
        .db
        .create_task(&fx.column, None, "Umbrella", 2, None)
        .expect("task");
    let account = fx.db.account(&fx.bob).needing(BoardRole::Member);
    let task = account.task(&fx.task).expect("read").expect("task");
    let target = account.task(&umbrella).expect("read").expect("umbrella");
    let refused = account.change_umbrella(&task, &target, Actor::SYSTEM);
    assert!(refused.is_err());
}

#[test]
fn set_active_timer_should_refuse_a_viewer_on_the_task_board() {
    let fx = Fixture::new();
    let timer = ActiveTimer {
        task_id: Some(fx.task.clone()),
        mode: TimerMode::Stopwatch,
        started_at: 0,
        duration_secs: None,
    };
    assert!(fx.db.set_active_timer(&fx.carol, &timer).is_err());
    assert!(fx.db.active_timer(&fx.carol).expect("read").is_none());
}

#[tokio::test]
async fn board_page_should_carry_the_role_of_the_signed_in_account() {
    let fx = Fixture::new();
    for (who, role) in [
        (OWNER, BoardRole::Owner),
        (MEMBER, BoardRole::Member),
        (VIEWER, BoardRole::Viewer),
    ] {
        let page = fx.send(who, "GET", &fx.fill("/b/{board}"), None).await;
        assert_eq!(page.status, StatusCode::OK, "{}", page.body);
        let marker = format!("data-role=\"{}\"", role.as_str());
        let html = page.body.as_str().expect("html");
        assert!(html.contains(&marker), "{}", role.as_str());
    }
}

#[test]
fn task_history_should_hide_a_link_event_naming_a_task_the_account_cannot_reach() {
    let fx = Fixture::new();
    let private = fx.db.create_board(&fx.alice, "Private").expect("board");
    let column = fx.db.create_column(&private, "Todo", None).expect("column");
    let hidden = fx
        .db
        .create_task(&column, None, "Hidden", 1, None)
        .expect("task");
    let added = fx
        .db
        .add_task_link(&fx.task, LinkKind::Related, &hidden, None, Actor::SYSTEM)
        .expect("link");
    assert!(matches!(added, LinkAddOutcome::Added(_)));
    let history = fx.db.task(&fx.task).expect("read").expect("task").history;
    let seen = |account: &AccountId| {
        fx.db
            .account(account)
            .needing(BoardRole::Viewer)
            .visible_history(&history)
            .expect("history")
            .iter()
            .filter(|event| event.detail.contains("Hidden"))
            .count()
    };
    assert_eq!((seen(&fx.alice), seen(&fx.bob)), (1, 0));
}

#[test]
fn listing_links_should_hide_a_task_on_a_board_the_account_holds_no_role_on() {
    let fx = Fixture::new();
    let private = fx.db.create_board(&fx.bob, "Private").expect("board");
    let column = fx.db.create_column(&private, "Todo", None).expect("column");
    let hidden = fx
        .db
        .create_task(&column, None, "Hidden", 1, None)
        .expect("task");
    let added = fx
        .db
        .add_task_link(&fx.task, LinkKind::Related, &hidden, None, Actor::SYSTEM)
        .expect("link");
    assert!(matches!(added, LinkAddOutcome::Added(_)));
    let seen = |account: &AccountId| {
        fx.db
            .account(account)
            .needing(BoardRole::Viewer)
            .list_task_links(&fx.task)
            .expect("read")
            .expect("task")
            .len()
    };
    assert_eq!((seen(&fx.alice), seen(&fx.bob)), (0, 1));
}

#[tokio::test]
async fn board_page_should_hide_the_write_controls_a_role_cannot_use() {
    let fx = Fixture::new();
    let controls = ["data-add-task-for", "data-bm=\"recycle\""];
    let owner_only = ["id=\"edit-layout-btn\"", "data-bm=\"layout\""];
    for (who, writes, settings) in [
        (OWNER, true, true),
        (MEMBER, true, false),
        (VIEWER, false, false),
    ] {
        let page = fx.send(who, "GET", &fx.fill("/b/{board}"), None).await;
        let html = page.body.as_str().expect("html");
        for control in controls {
            assert_eq!(html.contains(control), writes, "{control} for {who}");
        }
        for control in owner_only {
            assert_eq!(html.contains(control), settings, "{control} for {who}");
        }
        assert_eq!(html.contains("data-bm=\"settings\""), settings, "{who}");
    }
}

fn menu_actions(html: &str, menu_id: &str, attribute: &str) -> Vec<String> {
    let start = html
        .find(&format!("id=\"{menu_id}\""))
        .unwrap_or_else(|| panic!("{menu_id} is on the page"));
    let menu = &html[start..];
    let menu = &menu[..menu.find("</div>").expect("menu end")];
    menu.split(&format!("{attribute}=\""))
        .skip(1)
        .map(|rest| rest[..rest.find('"').expect("attribute end")].to_string())
        .collect()
}

const WRITE_CARD_ACTIONS: [&str; 10] = [
    "timer",
    "move",
    "color",
    "assign-members",
    "make-epic",
    "add-to-epic",
    "grouping-date",
    "copy-here",
    "copy",
    "delete",
];

#[tokio::test]
async fn board_page_should_offer_a_viewer_only_watch_and_details_in_the_column_menu() {
    let fx = Fixture::new();
    let page = fx.send(VIEWER, "GET", &fx.fill("/b/{board}"), None).await;
    let html = page.body.as_str().expect("html");
    assert_eq!(
        menu_actions(html, "column-menu", "data-col-act"),
        ["details", "watch"]
    );
}

#[tokio::test]
async fn board_page_should_offer_a_viewer_no_write_entry_in_the_card_menu() {
    let fx = Fixture::new();
    let page = fx.send(VIEWER, "GET", &fx.fill("/b/{board}"), None).await;
    let html = page.body.as_str().expect("html");
    assert_eq!(
        menu_actions(html, "card-ctx-menu", "data-card-act"),
        ["task-url"]
    );
}

#[tokio::test]
async fn board_page_should_keep_a_member_the_card_write_entries_but_not_the_column_edit() {
    let fx = Fixture::new();
    let page = fx.send(MEMBER, "GET", &fx.fill("/b/{board}"), None).await;
    let html = page.body.as_str().expect("html");
    let cards = menu_actions(html, "card-ctx-menu", "data-card-act");
    for action in WRITE_CARD_ACTIONS.into_iter().chain(["task-url"]) {
        assert!(cards.iter().any(|found| found == action), "{action}");
    }
    assert_eq!(
        menu_actions(html, "column-menu", "data-col-act"),
        ["details", "watch"]
    );
}

#[tokio::test]
async fn board_page_should_keep_the_owner_every_column_and_card_menu_entry() {
    let fx = Fixture::new();
    let page = fx.send(OWNER, "GET", &fx.fill("/b/{board}"), None).await;
    let html = page.body.as_str().expect("html");
    assert_eq!(
        menu_actions(html, "column-menu", "data-col-act"),
        ["edit", "collapse", "details", "watch"]
    );
    let cards = menu_actions(html, "card-ctx-menu", "data-card-act");
    for action in WRITE_CARD_ACTIONS.into_iter().chain(["task-url"]) {
        assert!(cards.iter().any(|found| found == action), "{action}");
    }
}

const MODAL_WRITE_CONTROLS: [&str; 8] = [
    "data-claim-take",
    "tm-add-menu",
    "data-tm-act=\"move\"",
    "tm-timer-menu",
    "data-tm-act=\"delete\"",
    "tm-claim-ttl",
    "data-tm-act=\"make-epic\"",
    "data-tm-act=\"add-to-epic\"",
];

const EPIC_MODAL_WRITE_CONTROLS: [&str; 6] = [
    "data-tm-act=\"close-epic\"",
    "data-tm-act=\"make-card\"",
    "data-tm-act=\"delete\"",
    "data-link-open=\"child\"",
    "data-new-child-open",
    "data-unlink-child",
];

#[tokio::test]
async fn epic_modal_should_hide_the_epic_write_controls_from_a_viewer_alone() {
    let fx = Fixture::new();
    let child = fx
        .db
        .create_task(&fx.column, None, "Child", 2, None)
        .expect("task");
    let linked = fx
        .db
        .add_task_link(&child, LinkKind::Parent, &fx.task, None, Actor::SYSTEM)
        .expect("link");
    assert!(matches!(linked, LinkAddOutcome::Added(_)));
    let made = fx
        .send(
            OWNER,
            "PATCH",
            &fx.fill("/api/tasks/{task}"),
            Some(json!({"kind": "epic"})),
        )
        .await;
    assert_eq!(made.status, StatusCode::OK, "{}", made.body);
    for who in [VIEWER, MEMBER, OWNER] {
        let page = fx
            .send(who, "GET", &fx.fill("/api/tasks/{task}/modal"), None)
            .await;
        let html = page.body.as_str().expect("html");
        for control in EPIC_MODAL_WRITE_CONTROLS {
            assert_eq!(html.contains(control), who != VIEWER, "{control} for {who}");
        }
    }
}

#[tokio::test]
async fn task_modal_should_hide_the_write_controls_from_a_viewer() {
    let fx = Fixture::new();
    let page = fx
        .send(VIEWER, "GET", &fx.fill("/api/tasks/{task}/modal"), None)
        .await;
    let html = page.body.as_str().expect("html");
    for control in MODAL_WRITE_CONTROLS {
        assert!(!html.contains(control), "{control}");
    }
    assert!(html.contains("data-tm-act=\"time-log\""));
}

#[tokio::test]
async fn task_modal_should_keep_the_write_controls_for_a_member_and_the_owner() {
    let fx = Fixture::new();
    for who in [MEMBER, OWNER] {
        let page = fx
            .send(who, "GET", &fx.fill("/api/tasks/{task}/modal"), None)
            .await;
        let html = page.body.as_str().expect("html");
        for control in MODAL_WRITE_CONTROLS {
            assert!(html.contains(control), "{control} for {who}");
        }
    }
}

#[tokio::test]
async fn board_page_should_name_each_member_with_a_role_in_its_avatar_popover() {
    let fx = Fixture::new();
    let page = fx.send(VIEWER, "GET", &fx.fill("/b/{board}"), None).await;
    let html = page.body.as_str().expect("html");
    for (name, role) in [("alice", "owner"), ("bob", "member"), ("carol", "viewer")] {
        let avatar = format!("data-member-name=\"{name}\" data-member-role=\"{role}\"");
        assert!(html.contains(&avatar), "{name}");
    }
    assert!(html.contains("id=\"member-popover-filter\""));
    assert!(!html.contains("dave"));
}

#[tokio::test]
async fn changing_the_board_owner_should_swap_owner_and_member_and_refuse_everyone_else() {
    let fx = Fixture::new();
    let uri = fx.fill("/api/boards/{board}/owner");
    for who in [MEMBER, VIEWER, STRANGER] {
        let reply = fx
            .send(who, "PUT", &uri, Some(json!({ "username": "bob" })))
            .await;
        let expected = if who == STRANGER {
            StatusCode::NOT_FOUND
        } else {
            StatusCode::FORBIDDEN
        };
        assert_eq!(reply.status, expected, "{who}: {}", reply.body);
    }
    let stranger = fx
        .send(OWNER, "PUT", &uri, Some(json!({ "username": "dave" })))
        .await;
    assert_eq!(stranger.status, StatusCode::NOT_FOUND);
    let own = fx
        .send(OWNER, "PUT", &uri, Some(json!({ "username": "alice" })))
        .await;
    assert_eq!(own.status, StatusCode::BAD_REQUEST);
    let changed = fx
        .send(OWNER, "PUT", &uri, Some(json!({ "username": "bob" })))
        .await;
    assert_eq!(changed.status, StatusCode::OK, "{}", changed.body);
    let roles = fx
        .send(MEMBER, "GET", &fx.fill("/api/boards/{board}/members"), None)
        .await;
    let listed: Vec<(String, String)> = roles
        .body
        .as_array()
        .expect("members")
        .iter()
        .map(|member| {
            let text = |key: &str| member[key].as_str().expect("text").to_string();
            assert_eq!(member.as_object().expect("object").len(), 3);
            (text("username"), text("role"))
        })
        .collect();
    assert_eq!(
        listed,
        [
            ("bob".to_string(), "owner".to_string()),
            ("alice".to_string(), "member".to_string()),
            ("carol".to_string(), "viewer".to_string()),
        ]
    );
    let back = fx
        .send(OWNER, "PUT", &uri, Some(json!({ "username": "alice" })))
        .await;
    assert_eq!(back.status, StatusCode::FORBIDDEN);
}

#[test]
fn required_role_should_hold_every_webhook_route_at_owner() {
    for (method, pattern, _, _) in ROUTE_ACCESS {
        if pattern.contains("webhooks") {
            assert_eq!(
                required_role(method, pattern),
                BoardRole::Owner,
                "{method} {pattern}"
            );
        }
    }
}

#[test]
fn required_role_should_let_a_viewer_watch_a_column_and_read_a_short_link() {
    assert_eq!(
        required_role(&Method::PATCH, "/api/columns/:id"),
        BoardRole::Viewer
    );
    assert_eq!(required_role(&Method::GET, "/t/:id"), BoardRole::Viewer);
    assert_eq!(
        required_role(&Method::GET, "/api/boards/:id/history"),
        BoardRole::Viewer
    );
}

#[test]
fn every_board_task_and_entity_route_should_be_listed_so_none_falls_back_to_owner() {
    for (method, pattern, class, _) in ROUTE_ACCESS {
        if matches!(
            class,
            RouteClass::Board | RouteClass::Task | RouteClass::Entity
        ) {
            assert!(
                crate::auth::ROUTE_PERMISSIONS
                    .iter()
                    .any(|(listed, path, _)| listed == method && path == pattern),
                "{method} {pattern}"
            );
        }
    }
}

#[test]
fn required_role_should_let_a_member_bulk_edit_tasks() {
    assert_eq!(
        required_role(&Method::POST, "/api/boards/:id/tasks/bulk"),
        BoardRole::Member
    );
}

#[tokio::test]
async fn a_member_should_bulk_edit_tasks_and_a_viewer_should_be_refused() {
    let fx = Fixture::new();
    let uri = fx.fill("/api/boards/{board}/tasks/bulk");
    let body = json!({"task_ids": [fx.task], "action": "unassign_members"});
    let viewer = fx.send(VIEWER, "POST", &uri, Some(body.clone())).await;
    assert_eq!(viewer.status, StatusCode::FORBIDDEN, "{}", viewer.body);
    let member = fx.send(MEMBER, "POST", &uri, Some(body)).await;
    assert!(
        member.status.is_success(),
        "{} {}",
        member.status,
        member.body
    );
}

#[tokio::test]
async fn a_member_should_react_to_a_comment_and_a_viewer_should_be_refused() {
    let fx = Fixture::new();
    let comment = fx
        .send(
            OWNER,
            "POST",
            &fx.fill("/api/tasks/{task}/comments"),
            Some(json!({"body": "hello"})),
        )
        .await;
    let cid = comment.body["id"].as_str().expect("comment id");
    let uri = fx.fill(&format!("/api/tasks/{{task}}/comments/{cid}/reactions"));
    let body = json!({"emoji": "\u{1F44D}"});
    let viewer = fx.send(VIEWER, "POST", &uri, Some(body.clone())).await;
    assert_eq!(viewer.status, StatusCode::FORBIDDEN, "{}", viewer.body);
    let member = fx.send(MEMBER, "POST", &uri, Some(body)).await;
    assert!(
        member.status.is_success(),
        "{} {}",
        member.status,
        member.body
    );
    assert_eq!(member.body["reactions"][0]["handle"], "bob");
}

#[tokio::test]
async fn a_time_logged_event_should_carry_only_the_logging_account_time() {
    let fx = Fixture::new();
    let uri = fx.fill("/api/tasks/{task}/time");
    for (who, minutes) in [(OWNER, 7), (MEMBER, 5)] {
        let logged = fx
            .send(who, "POST", &uri, Some(json!({"minutes": minutes})))
            .await;
        assert!(logged.status.is_success(), "{}", logged.body);
    }
    let manual = json!({
        "task_id": fx.task,
        "date": "2020-01-01",
        "from": "09:00",
        "to": "10:00",
    });
    let logged = fx
        .send(MEMBER, "POST", "/api/time/manual", Some(manual))
        .await;
    assert!(logged.status.is_success(), "{}", logged.body);
    let history = fx.db.task(&fx.task).expect("read").expect("task").history;
    let values: Vec<_> = history
        .iter()
        .filter(|event| event.kind == HistoryKind::TimeLogged)
        .map(|event| (event.from.as_deref(), event.to.as_deref()))
        .collect();
    assert_eq!(
        values,
        [
            (Some("0m"), Some("7m")),
            (Some("0m"), Some("5m")),
            (Some("5m"), Some("1h 5m")),
        ]
    );
}

#[tokio::test]
async fn a_bulk_edit_should_refuse_a_link_to_a_board_the_member_only_views() {
    let fx = Fixture::new();
    let viewed = fx.db.create_board(&fx.alice, "Viewed").expect("board");
    let column = fx.db.create_column(&viewed, "Todo", None).expect("column");
    let other = fx
        .db
        .create_task(&column, None, "Other", 1, None)
        .expect("task");
    fx.db
        .set_board_member(&viewed, "bob", BoardRole::Viewer)
        .expect("bob");
    let added = fx
        .db
        .add_task_link(&fx.task, LinkKind::Related, &other, None, Actor::SYSTEM)
        .expect("link");
    assert!(matches!(added, LinkAddOutcome::Added(_)));
    let uri = fx.fill("/api/boards/{board}/tasks/bulk");
    for body in [
        json!({"task_ids": [fx.task], "action": "delete"}),
        json!({
            "task_ids": [fx.task],
            "action": "relations",
            "remove_links": [{"kind": "related", "target_id": other}],
        }),
    ] {
        let reply = fx.send(MEMBER, "POST", &uri, Some(body.clone())).await;
        assert_eq!(
            reply.status,
            StatusCode::FORBIDDEN,
            "{body}: {}",
            reply.body
        );
        let task = fx.db.task(&fx.task).expect("read").expect("task");
        assert_eq!(task.links.len(), 1, "{body}");
    }
}

#[tokio::test]
async fn a_viewer_should_watch_a_column_but_get_403_for_each_other_column_field() {
    let fx = Fixture::new();
    let uri = fx.fill("/api/columns/{column}");
    let before = serde_json::to_value(&fx.db.list_columns(&fx.board).expect("columns")[0])
        .expect("column json");
    let reshapes = [
        json!({"wip_limit": 2}),
        json!({"wip_limit": null}),
        json!({"is_done": true}),
        json!({"description": "x"}),
        json!({"collapsed": true}),
        json!({"config_json": "{}"}),
        json!({"watched": true, "collapsed": true}),
    ];
    for body in reshapes {
        let reply = fx.send(VIEWER, "PATCH", &uri, Some(body.clone())).await;
        assert_eq!(
            reply.status,
            StatusCode::FORBIDDEN,
            "{body}: {}",
            reply.body
        );
        let after = serde_json::to_value(&fx.db.list_columns(&fx.board).expect("columns")[0])
            .expect("column json");
        assert_eq!(after, before, "{body}");
    }
    let watch = fx
        .send(VIEWER, "PATCH", &uri, Some(json!({"watched": true})))
        .await;
    assert!(watch.status.is_success(), "{} {}", watch.status, watch.body);
    let column = fx.db.list_columns(&fx.board).expect("columns").remove(0);
    assert_eq!(column.watchers, [fx.carol.as_str()]);
}

#[tokio::test]
async fn a_member_should_get_403_changing_any_column_field() {
    let fx = Fixture::new();
    let uri = fx.fill("/api/columns/{column}");
    for body in [
        json!({"name": "Renamed"}),
        json!({"wip_limit": 2}),
        json!({"is_done": true}),
        json!({"description": "x"}),
        json!({"collapsed": true}),
        json!({"config_json": "{}"}),
    ] {
        let reply = fx.send(MEMBER, "PATCH", &uri, Some(body.clone())).await;
        assert_eq!(
            reply.status,
            StatusCode::FORBIDDEN,
            "{body}: {}",
            reply.body
        );
    }
}

#[tokio::test]
async fn a_member_should_be_refused_a_column_move() {
    let fx = Fixture::new();
    let uri = fx.fill("/api/columns/{column}/move");
    let before = fx.db.list_columns(&fx.board).expect("columns")[0].position;
    for who in [MEMBER, VIEWER] {
        let reply = fx
            .send(who, "POST", &uri, Some(json!({"position": 5})))
            .await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{who}: {}", reply.body);
    }
    let after = fx.db.list_columns(&fx.board).expect("columns")[0].position;
    assert_eq!(after, before);
}

#[tokio::test]
async fn an_owner_should_not_leave_or_be_removed_from_its_own_board() {
    let fx = Fixture::new();
    let uri = fx.fill("/api/boards/{board}/members?username=alice");
    let reply = fx.send(OWNER, "DELETE", &uri, None).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.body);
    let task = fx
        .send(OWNER, "GET", &fx.fill("/api/tasks/{task}"), None)
        .await;
    assert!(task.status.is_success(), "{}", task.body);
}

#[tokio::test]
async fn timeline_page_should_tell_a_viewer_nothing_about_adding_tasks() {
    let fx = Fixture::new();
    for (who, writes) in [(OWNER, true), (MEMBER, true), (VIEWER, false)] {
        let page = fx
            .send(who, "GET", &fx.fill("/b/{board}/timeline"), None)
            .await;
        assert_eq!(page.status, StatusCode::OK, "{}", page.body);
        let html = page.body.as_str().expect("html");
        assert!(html.contains("data-page-init=\"TimelinePage\""), "{who}");
        assert!(html.contains("id=\"tlp-canvas\""), "{who}");
        assert!(html.contains("Your timeline is empty."), "{who}");
        assert_eq!(html.contains("id=\"tlp-panel\""), writes, "{who}");
        assert_eq!(html.contains("id=\"tlp-search\""), writes, "{who}");
        assert!(html.contains("id=\"tlp-zoom\""), "{who}");
        assert!(html.contains("id=\"tlp-centre\""), "{who}");
        assert_eq!(
            html.contains("Click a task in the Tasks list on the left to add it."),
            writes,
            "{who}"
        );
    }
    let stranger = fx
        .send(STRANGER, "GET", &fx.fill("/b/{board}/timeline"), None)
        .await;
    assert_eq!(stranger.status, StatusCode::NOT_FOUND);
}
