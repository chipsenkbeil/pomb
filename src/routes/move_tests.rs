use axum::http::StatusCode;
use serde_json::json;

use super::test_support::Fixture;

#[tokio::test]
async fn single_move_without_a_position_should_follow_the_new_task_position_setting() {
    let fx = Fixture::new();
    let (board, columns) = fx.board("Plans", &["Todo", "Doing"]);
    fx.task(&columns[1], "d").await;
    let a = fx.task(&columns[0], "a").await;
    let b = fx.task(&columns[0], "b").await;
    let to_doing = json!({"column_id": columns[1]});

    let at_bottom = fx
        .send(
            "POST",
            &format!("/api/tasks/{a}/move"),
            Some(to_doing.clone()),
        )
        .await;
    fx.top_placement(&board).await;
    let at_top = fx
        .send("POST", &format!("/api/tasks/{b}/move"), Some(to_doing))
        .await;

    assert_eq!(at_bottom.status, StatusCode::OK, "{}", at_bottom.body);
    assert_eq!(at_top.status, StatusCode::OK, "{}", at_top.body);
    assert_eq!(fx.cell(&board, &columns[1]), ["b", "d", "a"]);
}

#[tokio::test]
async fn single_move_with_a_position_should_ignore_the_new_task_position_setting() {
    let fx = Fixture::new();
    let (board, columns) = fx.board("Plans", &["Todo", "Doing"]);
    fx.top_placement(&board).await;
    fx.task(&columns[1], "d").await;
    let a = fx.task(&columns[0], "a").await;
    let body = json!({"column_id": columns[1], "position": 1});

    let reply = fx
        .send("POST", &format!("/api/tasks/{a}/move"), Some(body))
        .await;

    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(fx.cell(&board, &columns[1]), ["d", "a"]);
}
