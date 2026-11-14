use axum::http::StatusCode;
use serde_json::{Value, json};

use super::test_support::{Fixture, Reply};
use crate::db::{SwimlanePatch, SwimlaneUpdateOutcome};

struct Frozen {
    fx: Fixture,
    board: String,
    columns: Vec<String>,
    open: String,
    lane: String,
}

impl Frozen {
    fn new() -> Self {
        let fx = Fixture::new();
        let (board, columns) = fx.board("Plans", &["Todo", "Doing"]);
        fx.db
            .create_swimlane(&board, "Second")
            .expect("second lane");
        let lanes = fx.db.list_swimlanes(&board).expect("lanes");
        Self {
            open: lanes[0].id.clone(),
            lane: lanes[1].id.clone(),
            fx,
            board,
            columns,
        }
    }

    async fn task_in(&self, column: &str, lane: &str, name: &str) -> String {
        let body = json!({"column_id": column, "name": name, "swimlane_id": lane});
        let reply = self.fx.send("POST", "/api/tasks", Some(body)).await;
        assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.body);
        reply.body["id"].as_str().expect("id").to_string()
    }

    async fn archive(&self) {
        let uri = format!("/api/swimlanes/{}", self.lane);
        let reply = self
            .fx
            .send("PATCH", &uri, Some(json!({"archived": true})))
            .await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    }

    async fn archive_under_timer(&self, task: &str) {
        let start = json!({"task_id": task, "mode": "stopwatch"});
        let started = self.fx.send("POST", "/api/timer/start", Some(start)).await;
        assert_eq!(started.status, StatusCode::OK, "{}", started.body);
        let account = &self.fx.admin;
        let timer = self
            .fx
            .db
            .active_timer(account)
            .expect("timer")
            .expect("running");
        self.fx.db.clear_active_timer(account).expect("clear");
        let patch = SwimlanePatch {
            archived: Some(true),
            ..SwimlanePatch::default()
        };
        let outcome = self
            .fx
            .db
            .update_swimlane(&self.lane, &patch)
            .expect("archive");
        assert!(matches!(outcome, SwimlaneUpdateOutcome::Updated));
        self.fx
            .db
            .set_active_timer(account, &timer)
            .expect("restore");
    }
}

fn assert_frozen(reply: &Reply, what: &str) {
    assert_eq!(reply.status, StatusCode::CONFLICT, "{what}: {}", reply.body);
    assert_eq!(
        reply.body["code"], "lane_archived",
        "{what}: {}",
        reply.body
    );
}

#[tokio::test]
async fn create_should_be_refused_in_an_archived_lane_and_create_nothing() {
    let frozen = Frozen::new();
    frozen.archive().await;
    let before = frozen
        .fx
        .db
        .board_tasks(&frozen.board)
        .expect("tasks")
        .len();

    let body = json!({"column_id": frozen.columns[0], "name": "New", "swimlane_id": frozen.lane});
    let reply = frozen.fx.send("POST", "/api/tasks", Some(body)).await;

    assert_frozen(&reply, "create");
    let after = frozen
        .fx
        .db
        .board_tasks(&frozen.board)
        .expect("tasks")
        .len();
    assert_eq!(after, before);
}

#[tokio::test]
async fn move_should_be_refused_into_an_archived_lane() {
    let frozen = Frozen::new();
    let task = frozen
        .task_in(&frozen.columns[0], &frozen.open, "open")
        .await;
    frozen.archive().await;

    let body = json!({"column_id": frozen.columns[1], "swimlane_id": frozen.lane});
    let reply = frozen
        .fx
        .send("POST", &format!("/api/tasks/{task}/move"), Some(body))
        .await;

    assert_frozen(&reply, "move in");
    let row = frozen.fx.db.task(&task).expect("read").expect("task");
    assert_eq!(row.column_id, frozen.columns[0]);
}

#[tokio::test]
async fn move_should_be_refused_out_of_an_archived_lane() {
    let frozen = Frozen::new();
    let task = frozen
        .task_in(&frozen.columns[0], &frozen.lane, "frozen")
        .await;
    frozen.archive().await;

    for body in [
        json!({"column_id": frozen.columns[1]}),
        json!({"column_id": frozen.columns[1], "swimlane_id": frozen.open}),
        json!({"column_id": frozen.columns[0], "position": 0}),
    ] {
        let reply = frozen
            .fx
            .send("POST", &format!("/api/tasks/{task}/move"), Some(body))
            .await;
        assert_frozen(&reply, "move out");
    }
    let row = frozen.fx.db.task(&task).expect("read").expect("task");
    assert_eq!(row.column_id, frozen.columns[0]);
    assert_eq!(row.swimlane_id.as_deref(), Some(frozen.lane.as_str()));
}

#[tokio::test]
async fn every_task_write_route_should_be_refused_in_an_archived_lane() {
    let frozen = Frozen::new();
    let fx = &frozen.fx;
    let task = frozen
        .task_in(&frozen.columns[0], &frozen.lane, "frozen")
        .await;
    let other = frozen
        .task_in(&frozen.columns[0], &frozen.open, "open")
        .await;
    let manual = json!({"task_id": task, "date": "2020-01-02", "from": "09:00", "to": "09:30"});
    let entry = fx.send("POST", "/api/time/manual", Some(manual)).await;
    assert_eq!(entry.status, StatusCode::OK, "{}", entry.body);
    let entry = entry.body["id"].as_str().expect("entry id").to_string();
    let link = json!({"kind": "related", "target_id": other});
    let link = fx
        .send("POST", &format!("/api/tasks/{task}/links"), Some(link))
        .await;
    assert_eq!(link.status, StatusCode::OK, "{}", link.body);
    let link = link.body["id"].as_str().expect("link id").to_string();
    let timer = json!({"task_id": other, "mode": "stopwatch"});
    let started = fx.send("POST", "/api/timer/start", Some(timer)).await;
    assert_eq!(started.status, StatusCode::OK, "{}", started.body);
    frozen.archive().await;

    let path = format!("/api/tasks/{task}");
    let range = json!({"task_id": task, "date": "2020-01-03", "from": "10:00", "to": "10:30"});
    let cases: Vec<(&str, String, Option<Value>)> = vec![
        ("PATCH", path.clone(), Some(json!({"name": "Renamed"}))),
        ("PATCH", path.clone(), Some(json!({"labels_add": ["x"]}))),
        ("PATCH", path.clone(), Some(json!({"size": 3}))),
        ("DELETE", path.clone(), None),
        (
            "POST",
            format!("{path}/watch"),
            Some(json!({"watched": true})),
        ),
        (
            "POST",
            format!("{path}/checklist/0"),
            Some(json!({"checked": true, "description_hash": "x"})),
        ),
        ("POST", format!("{path}/time"), Some(json!({"minutes": 5}))),
        (
            "POST",
            format!("{path}/subtasks"),
            Some(json!({"name": "S"})),
        ),
        (
            "PUT",
            format!("{path}/subtasks/order"),
            Some(json!({"order": []})),
        ),
        (
            "PATCH",
            format!("{path}/subtasks/nope"),
            Some(json!({"done": true})),
        ),
        ("DELETE", format!("{path}/subtasks/nope"), None),
        ("DELETE", format!("{path}/subtasks"), None),
        (
            "POST",
            format!("{path}/subtasks/from-template"),
            Some(json!({"template_id": "nope"})),
        ),
        (
            "POST",
            format!("{path}/subtasks/nope/convert"),
            Some(json!({})),
        ),
        (
            "POST",
            format!("{path}/comments"),
            Some(json!({"body": "hi", "kind": "agent"})),
        ),
        (
            "PATCH",
            format!("{path}/comments/nope"),
            Some(json!({"body": "hi"})),
        ),
        ("DELETE", format!("{path}/comments/nope"), None),
        (
            "POST",
            format!("{path}/attachments/link"),
            Some(json!({"name": "Spec", "url": "https://example.com/spec"})),
        ),
        (
            "PATCH",
            format!("{path}/attachments/nope"),
            Some(json!({"name": "b.txt"})),
        ),
        ("DELETE", format!("{path}/attachments/nope"), None),
        (
            "POST",
            format!("{path}/links"),
            Some(json!({"kind": "related", "target_id": other})),
        ),
        ("DELETE", format!("{path}/links/{link}"), None),
        (
            "POST",
            format!("/api/tasks/{other}/links"),
            Some(json!({"kind": "related", "target_id": task})),
        ),
        (
            "POST",
            format!("/api/tasks/{other}/links"),
            Some(json!({"kind": "blocks", "target_id": task})),
        ),
        (
            "PUT",
            format!("/api/tasks/{other}/umbrella"),
            Some(json!({"target_id": task})),
        ),
        (
            "POST",
            format!("{path}/claim"),
            Some(json!({"claimant": "agent", "ttl_seconds": 60})),
        ),
        (
            "POST",
            format!("{path}/claim/heartbeat"),
            Some(json!({"fence": 1, "ttl_seconds": 60})),
        ),
        (
            "POST",
            format!("{path}/claim/release"),
            Some(json!({"fence": 1})),
        ),
        (
            "POST",
            format!("{path}/owner"),
            Some(json!({"name": "agent", "ttl_seconds": 60})),
        ),
        ("POST", "/api/time/manual".to_string(), Some(range.clone())),
        (
            "PUT",
            format!("/api/time/entries/{entry}"),
            Some(range.clone()),
        ),
        ("DELETE", format!("/api/time/entries/{entry}"), None),
        (
            "POST",
            "/api/timer/start".to_string(),
            Some(json!({"task_id": task, "mode": "stopwatch"})),
        ),
        (
            "POST",
            "/api/timer/retarget".to_string(),
            Some(json!({"task_id": task})),
        ),
        (
            "POST",
            format!("/api/boards/{}/tasks/bulk", frozen.board),
            Some(json!({"task_ids": [task], "action": "unassign_members"})),
        ),
    ];
    for (method, uri, body) in cases {
        let reply = fx.send(method, &uri, body).await;
        assert_frozen(&reply, &format!("{method} {uri}"));
    }
}

#[tokio::test]
async fn bulk_action_should_be_refused_as_a_whole_when_it_touches_an_archived_card() {
    let frozen = Frozen::new();
    let kept = frozen
        .task_in(&frozen.columns[0], &frozen.open, "kept")
        .await;
    let held = frozen
        .task_in(&frozen.columns[0], &frozen.lane, "held")
        .await;
    frozen.archive().await;

    let moved = frozen
        .fx
        .bulk(
            &frozen.board,
            json!({"task_ids": [kept, held], "action": "move", "column_id": frozen.columns[1]}),
        )
        .await;
    let labelled = frozen
        .fx
        .bulk(
            &frozen.board,
            json!({"task_ids": [kept, held], "action": "labels", "add_labels": ["x"]}),
        )
        .await;
    let into = frozen
        .fx
        .bulk(
            &frozen.board,
            json!({
                "task_ids": [kept], "action": "move",
                "column_id": frozen.columns[1], "swimlane_id": frozen.lane,
            }),
        )
        .await;

    assert_frozen(&moved, "bulk move");
    assert_frozen(&labelled, "bulk labels");
    assert_frozen(&into, "bulk move into the lane");
    let row = frozen.fx.db.task(&kept).expect("read").expect("task");
    assert_eq!(row.column_id, frozen.columns[0]);
    assert!(row.labels.is_empty());
}

#[tokio::test]
async fn get_should_still_read_a_task_in_an_archived_lane() {
    let frozen = Frozen::new();
    let task = frozen
        .task_in(&frozen.columns[0], &frozen.lane, "frozen")
        .await;
    frozen.archive().await;

    let reply = frozen
        .fx
        .send("GET", &format!("/api/tasks/{task}"), None)
        .await;

    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(reply.body["id"], task.as_str());
    assert_eq!(reply.body["name"], "frozen");
}

#[tokio::test]
async fn wip_count_and_limit_refusal_should_ignore_cards_in_an_archived_lane() {
    let frozen = Frozen::new();
    let column = frozen.columns[1].clone();
    frozen.task_in(&column, &frozen.lane, "frozen").await;
    frozen
        .fx
        .db
        .set_column_wip(&column, Some(1))
        .expect("limit");
    let body = json!({"column_id": column, "name": "extra", "swimlane_id": frozen.open});
    let full = frozen
        .fx
        .send("POST", "/api/tasks", Some(body.clone()))
        .await;
    assert_eq!(full.status, StatusCode::CONFLICT, "{}", full.body);
    assert_eq!(full.body["code"], "wip_limit_reached", "{}", full.body);
    assert_eq!(
        frozen.fx.db.count_tasks_in_column(&column).expect("count"),
        1
    );

    frozen.archive().await;

    assert_eq!(
        frozen.fx.db.count_tasks_in_column(&column).expect("count"),
        0
    );
    let created = frozen
        .fx
        .send("POST", "/api/tasks", Some(body.clone()))
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    let refused = frozen.fx.send("POST", "/api/tasks", Some(body)).await;
    assert_eq!(refused.status, StatusCode::CONFLICT, "{}", refused.body);
    assert_eq!(
        refused.body["code"], "wip_limit_reached",
        "{}",
        refused.body
    );
}

#[tokio::test]
async fn timer_should_stop_and_switch_when_its_lane_is_archived_under_it() {
    let frozen = Frozen::new();
    let task = frozen
        .task_in(&frozen.columns[0], &frozen.lane, "frozen")
        .await;
    let other = frozen
        .task_in(&frozen.columns[0], &frozen.open, "open")
        .await;
    frozen.archive_under_timer(&task).await;

    let switch = json!({"task_id": other, "mode": "stopwatch"});
    let switched = frozen
        .fx
        .send("POST", "/api/timer/start", Some(switch))
        .await;
    assert_eq!(switched.status, StatusCode::OK, "{}", switched.body);
}

#[tokio::test]
async fn timer_should_stop_when_its_lane_is_archived_under_it() {
    let frozen = Frozen::new();
    let task = frozen
        .task_in(&frozen.columns[0], &frozen.lane, "frozen")
        .await;
    frozen.archive_under_timer(&task).await;

    let stopped = frozen
        .fx
        .send("POST", "/api/timer/stop", Some(json!({})))
        .await;

    assert_eq!(stopped.status, StatusCode::OK, "{}", stopped.body);
}
