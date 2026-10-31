use std::collections::HashMap;
use std::fs;
use std::future::IntoFuture;
use std::net::SocketAddr;
use std::path::Path;

use axum::Router;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::get;
use chrono::{DateTime, Duration, Utc};
use clap::{CommandFactory, Parser};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::{
    AddArgs, AttachArgs, BoardArgs, BoardVerb, Card, Error, LabelArgs, LinkArgs, LinkKind, LsArgs,
    MoveArgs, NoteArgs, OutcomeArgs, ShowArgs, resolve, run,
};
use crate::auth::{AuthChoice, SignIn, new_api_token_record};
use crate::client::Client;
use crate::db::{AccountDb, Db, FencedWrite, NewComment};
use crate::models::{AccountId, Actor, ActorKind, AuthScope};
use crate::{AppState, Args, Verb, routes};

const BOARD: &str = "Pomb";

struct Server {
    _dir: tempfile::TempDir,
    app: Router,
    url: String,
    token: String,
    admin: String,
    db: Db,
    board_id: String,
    backlog: String,
    doing: String,
    shipped: String,
}

struct Seeded {
    docs: String,
    client: String,
    triage: String,
    review: String,
    ship: String,
}

async fn serve(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("address"));
    tokio::spawn(
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .into_future(),
    );
    url
}

async fn start() -> Server {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("board-cli.redb");
    let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
    let owner: AccountId = db.create_user("admin").expect("create user");
    let (record, token) = new_api_token_record(&owner, "reader", AuthScope::Read, None);
    db.create_api_token(&record).expect("reader token");
    let (record, admin) = new_api_token_record(&owner, "writer", AuthScope::Admin, None);
    db.create_api_token(&record).expect("admin token");
    let board_id = db.create_board(&owner, BOARD).expect("board");
    for starter in db.list_boards().expect("boards") {
        if starter.id != board_id {
            db.delete_board(&starter.id).expect("delete starter board");
        }
    }
    let backlog = db
        .create_column(&board_id, "Backlog", None)
        .expect("column");
    let doing = db.create_column(&board_id, "Doing", None).expect("column");
    let shipped = db
        .create_column(&board_id, "Shipped", None)
        .expect("column");
    db.set_column_done(&shipped, true).expect("done column");
    let sign_in = SignIn::new(AuthChoice::Default, "127.0.0.1", None).expect("sign in");
    let app = routes::router(AppState::new(db.clone(), None, sign_in));
    let url = serve(app.clone()).await;
    let server = Server {
        _dir: dir,
        app,
        url,
        token,
        admin,
        db,
        board_id,
        backlog,
        doing,
        shipped,
    };
    server.name_sizes().await;
    server
}

impl Server {
    async fn api(&self, method: &str, uri: &str, body: Option<Value>) -> Value {
        let mut builder = axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", format!("Bearer {}", self.admin));
        let body = match body {
            Some(value) => {
                builder = builder.header("content-type", "application/json");
                axum::body::Body::from(value.to_string())
            }
            None => axum::body::Body::empty(),
        };
        let response = self
            .app
            .clone()
            .oneshot(builder.body(body).expect("request"))
            .await
            .expect("oneshot");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("body");
        let text = String::from_utf8_lossy(&bytes).into_owned();
        assert!(
            status.is_success(),
            "{method} {uri} answered {status}: {text}"
        );
        serde_json::from_str(&text).unwrap_or(Value::Null)
    }

    async fn name_sizes(&self) {
        let colors = self
            .api(
                "GET",
                &format!("/api/boards/{}/colors", self.board_id),
                None,
            )
            .await;
        for (value, label) in [
            ("yellow", "1 Pomodoro"),
            ("green", "2 Pomodori"),
            ("blue", "3 Pomodori"),
            ("red", ">3 Pomodori"),
        ] {
            let id = self.color(&colors, value);
            let uri = format!("/api/boards/{}/colors/{id}", self.board_id);
            self.api("PATCH", &uri, Some(json!({"label": label}))).await;
        }
    }

    fn color(&self, colors: &Value, value: &str) -> String {
        colors
            .as_array()
            .expect("colors")
            .iter()
            .find(|color| color["value"] == value)
            .and_then(|color| color["id"].as_str())
            .expect("color id")
            .to_string()
    }

    async fn color_id(&self, value: &str) -> String {
        let colors = self
            .api(
                "GET",
                &format!("/api/boards/{}/colors", self.board_id),
                None,
            )
            .await;
        self.color(&colors, value)
    }

    async fn card(&self, column: &str, name: &str, color: Option<&str>, labels: &[&str]) -> String {
        let color_id = match color {
            Some(value) => Some(self.color_id(value).await),
            None => None,
        };
        let id = self
            .db
            .create_task(column, None, name, 0, color_id.as_deref())
            .expect("task");
        let labels: Vec<String> = labels.iter().map(ToString::to_string).collect();
        self.db.set_task_labels(&id, &labels).expect("labels");
        id
    }

    async fn claim(&self, task: &str, claimant: &str) {
        let body = json!({"claimant": claimant, "ttl_seconds": 3600});
        self.api("POST", &format!("/api/tasks/{task}/claim"), Some(body))
            .await;
    }

    fn comment(&self, task: &str, new: NewComment<'_>) {
        let written = self
            .db
            .add_comment(task, new, Actor::SYSTEM, None)
            .expect("comment");
        assert!(matches!(written, FencedWrite::Written(_)));
    }

    async fn seed(&self) -> Seeded {
        let docs = self
            .card(&self.backlog, "Write the docs", Some("green"), &["docs"])
            .await;
        let client = self
            .card(
                &self.doing,
                "Build the client",
                Some("red"),
                &["cli", "rust"],
            )
            .await;
        let triage = self.card(&self.backlog, "Triage inbox", None, &[]).await;
        let review = self
            .card(&self.doing, "Review the diff", Some("blue"), &["review"])
            .await;
        let ship = self
            .card(&self.shipped, "Ship the thing", Some("yellow"), &[])
            .await;
        for (name, done) in [("Client reads the env", true), ("ls works", false)] {
            let subtask = self
                .db
                .add_subtask(&client, name)
                .expect("subtask")
                .expect("task");
            self.db
                .update_subtask(&client, &subtask.id, None, Some(done))
                .expect("tick");
        }
        self.claim(&client, "agent-7").await;
        self.claim(&review, "agent-9").await;
        Seeded {
            docs,
            client,
            triage,
            review,
            ship,
        }
    }

    async fn run(&self, args: BoardArgs, now: DateTime<Utc>) -> Result<String, Error> {
        let client = Client::new(&self.url, self.token.clone());
        tokio::task::spawn_blocking(move || run(&client, &args, now))
            .await
            .expect("join")
    }

    async fn write(&self, args: BoardArgs) -> Result<String, Error> {
        let client = Client::new(&self.url, self.admin.clone());
        tokio::task::spawn_blocking(move || run(&client, &args, Utc::now()))
            .await
            .expect("join")
    }

    async fn output(&self, args: BoardArgs) -> String {
        self.run(args, Utc::now()).await.expect("output")
    }
}

fn ls(column: Option<&str>, label: Option<&str>, size: Option<&str>) -> BoardArgs {
    BoardArgs {
        board: None,
        verb: BoardVerb::Ls(LsArgs {
            column: column.map(str::to_string),
            label: label.map(str::to_string),
            size: size.map(str::to_string),
        }),
    }
}

fn show(card: &str) -> BoardArgs {
    BoardArgs {
        board: None,
        verb: BoardVerb::Show(ShowArgs {
            card: card.to_string(),
        }),
    }
}

fn add(name: &str, column: Option<&str>, size: Option<&str>, labels: &[&str]) -> BoardArgs {
    BoardArgs {
        board: None,
        verb: BoardVerb::Add(AddArgs {
            name: name.to_string(),
            column: column.map(str::to_string),
            size: size.map(str::to_string),
            label: labels.iter().map(ToString::to_string).collect(),
            description: None,
        }),
    }
}

fn move_to(card: &str, column: &str) -> BoardArgs {
    BoardArgs {
        board: None,
        verb: BoardVerb::Move(MoveArgs {
            card: card.to_string(),
            column: column.to_string(),
        }),
    }
}

fn note(card: &str, text: &str, kind: &str, role: Option<&str>) -> BoardArgs {
    BoardArgs {
        board: None,
        verb: BoardVerb::Note(NoteArgs {
            card: card.to_string(),
            text: text.to_string(),
            kind: kind.to_string(),
            role: role.map(str::to_string),
        }),
    }
}

fn outcome(
    card: &str,
    text: Option<&str>,
    tick: Option<usize>,
    untick: Option<usize>,
) -> BoardArgs {
    BoardArgs {
        board: None,
        verb: BoardVerb::Outcome(OutcomeArgs {
            card: card.to_string(),
            text: text.map(str::to_string),
            tick,
            untick,
        }),
    }
}

fn link(card: &str, kind: LinkKind, other: &str) -> BoardArgs {
    BoardArgs {
        board: None,
        verb: BoardVerb::Link(LinkArgs {
            card: card.to_string(),
            kind,
            other: other.to_string(),
        }),
    }
}

fn label(card: &str, add: &[&str], remove: &[&str]) -> BoardArgs {
    BoardArgs {
        board: None,
        verb: BoardVerb::Label(LabelArgs {
            card: card.to_string(),
            add: add.iter().map(ToString::to_string).collect(),
            remove: remove.iter().map(ToString::to_string).collect(),
        }),
    }
}

fn attach(card: &str, file: &Path, mime: Option<&str>) -> BoardArgs {
    BoardArgs {
        board: None,
        verb: BoardVerb::Attach(AttachArgs {
            card: card.to_string(),
            file: file.to_path_buf(),
            mime: mime.map(str::to_string),
        }),
    }
}

fn short(id: &str) -> &str {
    &id[..8]
}

fn failure(result: Result<String, Error>) -> (i32, String) {
    let error = result.expect_err("an error");
    (error.exit_code(), error.to_string())
}

#[tokio::test(flavor = "multi_thread")]
async fn board_ls_should_list_the_open_cards_in_aligned_columns() {
    let server = start().await;
    let ids = server.seed().await;
    let (docs, client, triage, review) = (
        short(&ids.docs),
        short(&ids.client),
        short(&ids.triage),
        short(&ids.review),
    );
    let expected = [
        format!("{docs}  Backlog  2 Pomodori   docs      -    -        Write the docs"),
        format!("{triage}  Backlog  unsized      -         -    -        Triage inbox"),
        format!("{client}  Doing    >3 Pomodori  cli,rust  1/2  agent-7  Build the client"),
        format!("{review}  Doing    3 Pomodori   review    -    agent-9  Review the diff"),
    ]
    .join("\n");
    assert_eq!(server.output(ls(None, None, None)).await, expected);
}

#[tokio::test(flavor = "multi_thread")]
async fn board_ls_should_leave_out_a_done_column_unless_it_is_named() {
    let server = start().await;
    let ids = server.seed().await;
    let ship = short(&ids.ship);
    assert_eq!(
        server.output(ls(Some("shipped"), None, None)).await,
        format!("{ship}  Shipped  1 Pomodoro  -  Ship the thing")
    );
    let all = server.output(ls(None, None, None)).await;
    assert!(!all.contains(ship), "{all}");
}

#[tokio::test(flavor = "multi_thread")]
async fn board_ls_should_filter_by_column_label_and_size() {
    let server = start().await;
    let ids = server.seed().await;
    let (docs, client, triage, review) = (
        short(&ids.docs),
        short(&ids.client),
        short(&ids.triage),
        short(&ids.review),
    );
    assert_eq!(
        server.output(ls(Some("BACKLOG"), None, None)).await,
        [
            format!("{docs}  Backlog  2 Pomodori  docs  Write the docs"),
            format!("{triage}  Backlog  unsized     -     Triage inbox"),
        ]
        .join("\n")
    );
    assert_eq!(
        server.output(ls(None, Some("cli"), None)).await,
        format!("{client}  Doing  >3 Pomodori  cli,rust  1/2  agent-7  Build the client")
    );
    assert_eq!(
        server.output(ls(None, None, Some("3+"))).await,
        format!("{client}  Doing  >3 Pomodori  cli,rust  1/2  agent-7  Build the client")
    );
    assert_eq!(
        server.output(ls(None, None, Some("2"))).await,
        format!("{docs}  Backlog  2 Pomodori  docs  Write the docs")
    );
    assert_eq!(
        server.output(ls(None, None, Some("UNSIZED"))).await,
        format!("{triage}  Backlog  unsized  -  Triage inbox")
    );
    assert_eq!(
        server
            .output(ls(Some("doing"), Some("review"), Some("3")))
            .await,
        format!("{review}  Doing  3 Pomodori  review  agent-9  Review the diff")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn board_ls_should_say_no_cards_when_nothing_matches() {
    let server = start().await;
    assert_eq!(server.output(ls(None, None, None)).await, "no cards");
    server.seed().await;
    assert_eq!(
        server.output(ls(None, Some("nope"), None)).await,
        "no cards"
    );
    assert_eq!(server.output(ls(None, None, Some("1"))).await, "no cards");
}

#[tokio::test(flavor = "multi_thread")]
async fn board_ls_should_show_ticks_and_holders_only_when_a_listed_card_needs_them() {
    let server = start().await;
    let ids = server.seed().await;
    let (docs, client, review) = (short(&ids.docs), short(&ids.client), short(&ids.review));

    let later = Utc::now() + Duration::hours(2);
    let ticks_only = server.run(ls(Some("doing"), None, None), later).await;
    assert_eq!(
        ticks_only.expect("output"),
        [
            format!("{client}  Doing  >3 Pomodori  cli,rust  1/2  Build the client"),
            format!("{review}  Doing  3 Pomodori   review    -    Review the diff"),
        ]
        .join("\n")
    );

    let holders_only = server.output(ls(None, Some("review"), None)).await;
    assert_eq!(
        holders_only,
        format!("{review}  Doing  3 Pomodori  review  agent-9  Review the diff")
    );

    let neither = server.output(ls(None, Some("docs"), None)).await;
    assert_eq!(
        neither,
        format!("{docs}  Backlog  2 Pomodori  docs  Write the docs")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn board_ls_should_refuse_an_unknown_column_or_size_as_a_usage_error() {
    let server = start().await;
    server.seed().await;
    assert_eq!(
        failure(server.run(ls(Some("nope"), None, None), Utc::now()).await),
        (
            2,
            "unknown column nope, the columns are Backlog, Doing, Shipped".to_string()
        )
    );
    assert_eq!(
        failure(server.run(ls(None, None, Some("5")), Utc::now()).await),
        (
            2,
            "unknown size 5, the sizes are 1, 2, 3, 3+, unsized".to_string()
        )
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn board_ls_should_pick_the_only_board_and_ask_for_a_name_among_several() {
    let server = start().await;
    server.seed().await;
    let owner = server
        .db
        .owner()
        .expect("read")
        .expect("admin")
        .account_id();
    server.db.create_board(&owner, "Other").expect("board");

    let (code, text) = failure(server.run(ls(None, None, None), Utc::now()).await);
    assert_eq!(code, 2);
    assert!(
        text == "the account has more than one board, name one with --board: Pomb, Other"
            || text == "the account has more than one board, name one with --board: Other, Pomb",
        "{text}"
    );

    let mut named = ls(None, None, None);
    named.board = Some("Other".to_string());
    assert_eq!(server.output(named).await, "no cards");
}

#[tokio::test(flavor = "multi_thread")]
async fn board_show_should_print_the_card_its_family_its_outcomes_and_its_comments() {
    let server = start().await;
    let ids = server.seed().await;
    let umbrella = server
        .card(&server.backlog, "Umbrella", Some("green"), &[])
        .await;
    server
        .api(
            "POST",
            &format!("/api/tasks/{}/links", ids.client),
            Some(json!({"kind": "parent", "target_id": umbrella})),
        )
        .await;
    let fields = format!("/api/boards/{}/fields", server.board_id);
    server
        .api(
            "POST",
            &fields,
            Some(
                json!({"key": "agentctl.checks", "label": "Checks", "kind": "table",
                        "columns": [{"key": "run", "label": "Run", "kind": "text"}]}),
            ),
        )
        .await;
    for name in ["allow", "tests", "files"] {
        let body = json!({"key": format!("agentctl.{name}"), "label": name, "kind": "multiline"});
        server.api("POST", &fields, Some(body)).await;
    }
    server
        .api(
            "PATCH",
            &format!("/api/tasks/{}", ids.client),
            Some(json!({
                "description": "  Read the env, then GET the board.\n\nTwo paragraphs.\n\n",
                "fields": {
                    "agentctl.checks": [{"run": "cargo test"}, {"run": "cargo clippy"}],
                    "agentctl.allow": "src/board_cli.rs\n\nsrc/client.rs\n",
                    "agentctl.tests": "board_ls_should",
                    "agentctl.files": "src/main.rs"
                }
            })),
        )
        .await;
    server.comment(
        &ids.client,
        NewComment {
            author: "api-token",
            body: "Started on the client\r\nSecond line\n\nFourth line\n\n",
            actor_kind: ActorKind::ApiToken,
            token_name: Some("builder"),
            token_id: None,
            kind: Some("status"),
        },
    );
    server.comment(
        &ids.client,
        NewComment {
            author: "Chip",
            body: "Looks good",
            actor_kind: ActorKind::User,
            token_name: None,
            token_id: None,
            kind: None,
        },
    );
    server.comment(
        &ids.client,
        NewComment {
            author: "pomb",
            body: "moved",
            actor_kind: ActorKind::System,
            token_name: None,
            token_id: None,
            kind: None,
        },
    );
    let listed = server
        .api("GET", &format!("/api/tasks/{}/comments", ids.client), None)
        .await;
    let [first, second, third] = [0, 1, 2].map(|at| {
        let comment = &listed[at];
        let id = comment["id"].as_str().expect("id")[..8].to_string();
        let stamp = comment["created_at"].as_str().expect("created_at")[..16].replace('T', " ");
        (id, stamp)
    });

    let client = short(&ids.client);
    let expected = [
        format!("{client}  Doing  >3 Pomodori  cli,rust  Build the client"),
        format!("parent: {} Umbrella", short(&umbrella)),
        String::new(),
        "Read the env, then GET the board.".to_string(),
        String::new(),
        "Two paragraphs.".to_string(),
        String::new(),
        "done when:".to_string(),
        "- [x] Client reads the env".to_string(),
        "- [ ] ls works".to_string(),
        String::new(),
        "checks:".to_string(),
        "- cargo test".to_string(),
        "- cargo clippy".to_string(),
        String::new(),
        "allow:".to_string(),
        "- src/board_cli.rs".to_string(),
        "- src/client.rs".to_string(),
        String::new(),
        "tests:".to_string(),
        "- board_ls_should".to_string(),
        String::new(),
        "files:".to_string(),
        "- src/main.rs".to_string(),
        String::new(),
        format!(
            "{}  builder  status  -     {}  Started on the client",
            first.0, first.1
        ),
        format!("{}Second line", " ".repeat(51)),
        String::new(),
        format!("{}Fourth line", " ".repeat(51)),
        format!(
            "{}  human    -       Chip  {}  Looks good",
            second.0, second.1
        ),
        format!("{}  system   -       pomb  {}  moved", third.0, third.1),
    ]
    .join("\n");
    assert_eq!(server.output(show(&ids.client)).await, expected);
    assert_eq!(server.output(show(&client.to_uppercase())).await, expected);
}

#[tokio::test(flavor = "multi_thread")]
async fn board_show_should_count_the_children_of_an_umbrella() {
    let server = start().await;
    let ids = server.seed().await;
    server
        .api(
            "POST",
            &format!("/api/tasks/{}/links", ids.docs),
            Some(json!({"kind": "child", "target_id": ids.client})),
        )
        .await;
    server
        .api(
            "POST",
            &format!("/api/tasks/{}/links", ids.docs),
            Some(json!({"kind": "child", "target_id": ids.ship})),
        )
        .await;
    assert_eq!(
        server.output(show(&ids.docs)).await,
        format!(
            "{}  Backlog  2 Pomodori  docs  Write the docs\nchildren: 1 of 2 done\n\nno description\n\nno comments",
            short(&ids.docs)
        )
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn board_show_should_say_no_description_and_no_comments() {
    let server = start().await;
    let ids = server.seed().await;
    assert_eq!(
        server.output(show(&ids.triage)).await,
        format!(
            "{}  Backlog  unsized  -  Triage inbox\n\nno description\n\nno comments",
            short(&ids.triage)
        )
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn board_show_should_refuse_a_short_non_hex_or_unknown_id() {
    let server = start().await;
    server.seed().await;
    for (word, message) in [
        (
            "abc123",
            "a card id needs at least 8 hex characters: abc123",
        ),
        (
            "zzzzzzzz",
            "a card id needs at least 8 hex characters: zzzzzzzz",
        ),
        ("ffffffff", "no card starts with ffffffff"),
    ] {
        assert_eq!(
            failure(server.run(show(word), Utc::now()).await),
            (1, message.to_string()),
            "{word}"
        );
    }
}

#[test]
fn board_show_should_refuse_an_id_prefix_that_names_two_cards() {
    let cards: Vec<Card> = ["abcdef01-1111", "abcdef01-2222", "abcdef02-3333"]
        .into_iter()
        .map(|id| {
            let task = serde_json::from_value(json!({"id": id, "name": id})).expect("task");
            Card::new(task, &[], &HashMap::new(), Utc::now())
        })
        .collect();
    let message = |word| {
        resolve(&cards, word)
            .map(|card| card.task.id.clone())
            .unwrap_err()
            .to_string()
    };
    assert_eq!(
        message("abcdef01"),
        "abcdef01 matches more than one card, give more characters"
    );
    assert_eq!(
        resolve(&cards, "abcdef01-2")
            .map(|card| card.task.id.as_str())
            .unwrap(),
        "abcdef01-2222"
    );
    assert_eq!(
        resolve(&cards, "ABCDEF02")
            .map(|card| card.task.id.as_str())
            .unwrap(),
        "abcdef02-3333"
    );
}

#[test]
fn board_cli_should_parse_its_flags_and_name_the_environment_in_its_help() {
    let args = Args::try_parse_from([
        "pomb", "board", "--board", "Pomb", "ls", "--column", "Doing", "--label", "cli", "--size",
        "3+",
    ])
    .expect("parse");
    let Some(Verb::Board(board)) = args.verb else {
        panic!("not a board command");
    };
    assert_eq!(board.board.as_deref(), Some("Pomb"));
    let BoardVerb::Ls(ls) = board.verb else {
        panic!("not ls");
    };
    assert_eq!(ls.column.as_deref(), Some("Doing"));
    assert_eq!(ls.label.as_deref(), Some("cli"));
    assert_eq!(ls.size.as_deref(), Some("3+"));

    let mut command = Args::command();
    let help = command
        .find_subcommand_mut("board")
        .expect("board")
        .render_long_help()
        .to_string();
    for word in [
        "POMB_URL",
        "POMB_API_TOKEN",
        "POMB_BOARD",
        "pomb board show 75ecb314",
    ] {
        assert!(help.contains(word), "{word} in {help}");
    }
    assert!(!help.contains("database-path"), "{help}");
    let short = command
        .find_subcommand_mut("board")
        .expect("board")
        .render_help()
        .to_string();
    assert!(!short.contains("database-path"), "{short}");
    let archive = command
        .find_subcommand_mut("archive")
        .expect("archive")
        .render_long_help()
        .to_string();
    assert!(archive.contains("--database-path"), "{archive}");
}

#[test]
fn board_cli_should_keep_database_path_on_the_server_and_archive_only() {
    let after = Args::try_parse_from([
        "pomb",
        "archive",
        "--database-path",
        "a.redb",
        "--out",
        "dir",
    ])
    .expect("parse");
    let Some(Verb::Archive(archive)) = after.verb else {
        panic!("not archive");
    };
    assert_eq!(archive.database_path, Some("a.redb".into()));
    assert_eq!(after.database_path, None);

    let before = Args::try_parse_from([
        "pomb",
        "--database-path",
        "b.redb",
        "archive",
        "--out",
        "dir",
    ])
    .expect("parse");
    assert_eq!(before.database_path, Some("b.redb".into()));

    assert!(Args::try_parse_from(["pomb", "board", "--database-path", "c.redb", "ls"]).is_err());
}

fn assert_without(token: &str, texts: &[&str]) {
    for text in texts {
        assert!(!text.contains(token), "{text}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn board_cli_should_never_print_the_token_in_its_output_or_its_debug() {
    let server = start().await;
    let ids = server.seed().await;
    let listing = server.output(ls(None, None, None)).await;
    let card = server.output(show(&ids.client)).await;
    let client = Client::new(&server.url, server.token.clone());
    assert_without(
        &server.token,
        &[
            &listing,
            &card,
            &format!("{client:?}"),
            &format!("{:?}", ls(None, None, None)),
        ],
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn board_cli_should_never_print_the_token_when_the_server_answers_401() {
    let server = start().await;
    server.seed().await;
    let wrong = "pomb_not-a-real-token";
    let client = Client::new(&server.url, wrong.to_string());
    let args = ls(None, None, None);
    let error = tokio::task::spawn_blocking(move || run(&client, &args, Utc::now()))
        .await
        .expect("join")
        .expect_err("401");
    let text = error.to_string();
    assert!(text.starts_with("pomb answered 401: "), "{text}");
    assert_without(wrong, &[&text, &format!("{error:?}")]);
}

#[tokio::test(flavor = "multi_thread")]
async fn board_cli_should_never_print_the_token_when_the_server_is_unreachable() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("address"));
    drop(listener);
    let token = "pomb_unreachable-token";
    let client = Client::new(&url, token.to_string());
    let args = ls(None, None, None);
    let error = tokio::task::spawn_blocking(move || run(&client, &args, Utc::now()))
        .await
        .expect("join")
        .expect_err("closed port");
    let text = error.to_string();
    assert!(
        text.starts_with(&format!("request to {url}/api/boards failed: ")),
        "{text}"
    );
    assert_without(token, &[&text, &format!("{error:?}")]);
}

#[tokio::test(flavor = "multi_thread")]
async fn board_cli_should_never_print_the_token_in_a_missing_board_error() {
    let server = start().await;
    server.seed().await;
    let mut args = ls(None, None, None);
    args.board = Some("Nope".to_string());
    let (code, text) = failure(server.run(args, Utc::now()).await);
    assert_eq!(
        (code, text.as_str()),
        (2, "no board named Nope, the boards are Pomb")
    );
    assert_without(&server.token, &[&text]);
}

#[tokio::test(flavor = "multi_thread")]
async fn board_cli_should_never_print_the_token_when_the_server_echoes_it() {
    async fn echo_json(headers: HeaderMap) -> (StatusCode, axum::Json<Value>) {
        let sent = headers["authorization"].to_str().expect("header");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(json!({"message": format!("you sent {sent}")})),
        )
    }
    async fn echo_text(headers: HeaderMap) -> (StatusCode, String) {
        let sent = headers["authorization"].to_str().expect("header");
        (StatusCode::INTERNAL_SERVER_ERROR, sent.to_string())
    }
    let stub = Router::new()
        .route("/api/boards", get(echo_json))
        .route("/api/boards/x", get(echo_text));
    let url = serve(stub).await;
    let token = "pomb_echoed-token";
    let client = Client::new(&url, token.to_string());
    let (json_error, text_error) = tokio::task::spawn_blocking(move || {
        (
            client.get::<Value>("/api/boards").expect_err("500"),
            client.get::<Value>("/api/boards/x").expect_err("500"),
        )
    })
    .await
    .expect("join");
    assert_eq!(
        json_error.to_string(),
        "pomb answered 500: you sent Bearer ***"
    );
    assert_eq!(text_error.to_string(), "pomb answered 500: Bearer ***");
    assert_without(
        token,
        &[&json_error.to_string(), &format!("{text_error:?}")],
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn board_add_should_file_a_card_in_the_named_or_first_column() {
    let server = start().await;
    let first = server
        .write(add("Plain card", None, None, &[]))
        .await
        .expect("add");
    assert!(first.ends_with(" to Backlog"), "{first}");
    server
        .write(add("Sized card", Some("DOING"), Some("3+"), &["cli", "ui"]))
        .await
        .expect("add");
    let listing = server.output(ls(Some("doing"), None, None)).await;
    assert!(
        listing.contains("Doing  >3 Pomodori  cli,ui  Sized card"),
        "{listing}"
    );
    let plain = server.output(ls(Some("backlog"), None, None)).await;
    assert!(plain.contains("Backlog  1 Pomodoro"), "{plain}");
}

#[tokio::test(flavor = "multi_thread")]
async fn board_add_should_refuse_an_unknown_column_or_size() {
    let server = start().await;
    let (code, text) = failure(server.write(add("x", Some("Nowhere"), None, &[])).await);
    assert_eq!(code, 2);
    assert!(text.contains("unknown column Nowhere"), "{text}");
    let (code, _) = failure(server.write(add("x", None, Some("9"), &[])).await);
    assert_eq!(code, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn board_move_should_follow_a_renamed_column_without_a_client_change() {
    let server = start().await;
    let ids = server.seed().await;
    server
        .api(
            "PATCH",
            &format!("/api/columns/{}", server.doing),
            Some(json!({"name": "Underway"})),
        )
        .await;
    let said = server
        .write(move_to(short(&ids.docs), "underway"))
        .await
        .expect("move");
    assert_eq!(said, format!("moved {} to Underway", short(&ids.docs)));
    let listing = server.output(ls(Some("Underway"), None, None)).await;
    let first = listing.lines().next().expect("line");
    assert!(first.starts_with(short(&ids.docs)), "{listing}");
    let (code, text) = failure(server.write(move_to(short(&ids.docs), "Doing")).await);
    assert_eq!(code, 2);
    assert!(text.contains("unknown column Doing"), "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn board_move_should_print_the_servers_reason_when_it_refuses() {
    let server = start().await;
    let ids = server.seed().await;
    server
        .db
        .set_column_wip(&server.shipped, Some(1))
        .expect("wip");
    let (code, text) = failure(server.write(move_to(short(&ids.docs), "Shipped")).await);
    assert_eq!(code, 1);
    assert!(text.contains("409") && text.contains("WIP"), "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn board_note_should_comment_with_a_kind_and_a_role() {
    let server = start().await;
    let ids = server.seed().await;
    let said = server
        .write(note(short(&ids.docs), "Started", "status", Some("builder")))
        .await
        .expect("note");
    assert_eq!(said, format!("noted {}", short(&ids.docs)));
    server
        .write(note(short(&ids.docs), "Plain", "agent", None))
        .await
        .expect("note");
    let shown = server.output(show(short(&ids.docs))).await;
    assert!(
        shown.contains("status") && shown.contains("builder") && shown.contains("Started"),
        "{shown}"
    );
    assert!(shown.contains("Plain"), "{shown}");
    let (code, _) = failure(
        server
            .write(note(short(&ids.docs), "  ", "agent", None))
            .await,
    );
    assert_eq!(code, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn board_outcome_should_append_tick_and_untick_an_outcome() {
    let server = start().await;
    let ids = server.seed().await;
    let said = server
        .write(outcome(
            short(&ids.docs),
            Some("The docs build"),
            None,
            None,
        ))
        .await
        .expect("append");
    assert_eq!(said, format!("added outcome 1 to {}", short(&ids.docs)));
    server
        .write(outcome(short(&ids.docs), Some("The docs read"), None, None))
        .await
        .expect("append");
    let said = server
        .write(outcome(short(&ids.docs), None, Some(2), None))
        .await
        .expect("tick");
    assert_eq!(said, format!("ticked outcome 2 of {}", short(&ids.docs)));
    let shown = server.output(show(short(&ids.docs))).await;
    assert!(
        shown.contains("- [ ] The docs build\n- [x] The docs read"),
        "{shown}"
    );
    let said = server
        .write(outcome(short(&ids.docs), None, None, Some(2)))
        .await
        .expect("untick");
    assert_eq!(said, format!("unticked outcome 2 of {}", short(&ids.docs)));
    let shown = server.output(show(short(&ids.docs))).await;
    assert!(shown.contains("- [ ] The docs read"), "{shown}");
}

#[tokio::test(flavor = "multi_thread")]
async fn board_outcome_should_refuse_a_number_the_card_has_no_outcome_for() {
    let server = start().await;
    let ids = server.seed().await;
    for number in [0, 3] {
        let (code, text) = failure(
            server
                .write(outcome(short(&ids.client), None, Some(number), None))
                .await,
        );
        assert_eq!(code, 2);
        assert!(text.contains("it has 2"), "{text}");
    }
    let (code, text) = failure(
        server
            .write(outcome(short(&ids.docs), Some("  "), None, None))
            .await,
    );
    assert_eq!(code, 1);
    assert!(text.contains("400"), "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn board_link_should_join_two_cards_and_print_the_servers_reason_when_it_refuses() {
    let server = start().await;
    let ids = server.seed().await;
    let said = server
        .write(link(
            short(&ids.docs),
            LinkKind::BlockedBy,
            short(&ids.client),
        ))
        .await
        .expect("link");
    assert_eq!(
        said,
        format!(
            "linked {} blocked_by {}",
            short(&ids.docs),
            short(&ids.client)
        )
    );
    let links = server
        .api("GET", &format!("/api/tasks/{}/links", ids.docs), None)
        .await;
    assert_eq!(links.as_array().expect("links").len(), 1, "{links}");
    let (code, text) = failure(
        server
            .write(link(
                short(&ids.docs),
                LinkKind::BlockedBy,
                short(&ids.docs),
            ))
            .await,
    );
    assert_eq!(code, 1);
    assert!(text.contains("400"), "{text}");
    let (code, text) = failure(
        server
            .write(link(short(&ids.docs), LinkKind::Related, "00000000"))
            .await,
    );
    assert_eq!(code, 1);
    assert!(text.contains("no card starts with"), "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn board_label_should_add_and_remove_labels_and_keep_the_others() {
    let server = start().await;
    let ids = server.seed().await;
    let said = server
        .write(label(short(&ids.client), &["+ui", "cli"], &["rust"]))
        .await
        .expect("label");
    assert_eq!(said, format!("labels of {}: cli,ui", short(&ids.client)));
    let listing = server.output(ls(None, Some("ui"), None)).await;
    assert!(listing.contains("Build the client"), "{listing}");
    let listing = server.output(ls(None, Some("rust"), None)).await;
    assert_eq!(listing, "no cards");
    let said = server
        .write(label(short(&ids.client), &[], &["cli", "ui"]))
        .await
        .expect("label");
    assert_eq!(said, format!("labels of {}: -", short(&ids.client)));
    let (code, _) = failure(server.write(label(short(&ids.client), &[], &[])).await);
    assert_eq!(code, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn board_label_should_refuse_a_label_both_added_and_removed_or_empty() {
    let server = start().await;
    let ids = server.seed().await;
    let before = server.db.task(&ids.client).expect("task").expect("found");
    let (code, text) = failure(
        server
            .write(label(short(&ids.client), &["foo"], &["foo"]))
            .await,
    );
    assert_eq!(code, 2);
    assert!(text.contains("foo"), "{text}");
    let (code, text) = failure(server.write(label(short(&ids.client), &["+"], &[])).await);
    assert_eq!(code, 2);
    assert!(text.contains("needs a name"), "{text}");
    let (code, text) = failure(server.write(label(short(&ids.client), &[], &["  "])).await);
    assert_eq!(code, 2);
    assert!(text.contains("needs a name"), "{text}");
    let after = server.db.task(&ids.client).expect("task").expect("found");
    assert_eq!(after.labels, before.labels);
}

#[tokio::test(flavor = "multi_thread")]
async fn board_attach_should_upload_a_file_that_shows_on_the_card() {
    let server = start().await;
    let ids = server.seed().await;
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("trace log.txt");
    fs::write(&path, b"line one\nline two\n").expect("write");
    let said = server
        .write(attach(short(&ids.docs), &path, Some("text/plain")))
        .await
        .expect("attach");
    assert!(
        said.starts_with("attached trace log.txt (")
            && said.ends_with(&format!("to {}", short(&ids.docs))),
        "{said}"
    );
    let shown = server.output(show(short(&ids.docs))).await;
    assert!(shown.contains("trace log.txt 18 bytes"), "{shown}");
    let row = server.db.task(&ids.docs).expect("task").expect("found");
    let attachment = &row.attachments[0];
    assert_eq!(attachment.mime, "text/plain");
    let stored = server
        .db
        .read_attachment_data(&attachment.id)
        .expect("read")
        .expect("bytes");
    assert_eq!(stored, b"line one\nline two\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn board_attach_should_say_why_a_file_or_the_server_refuses() {
    let server = start().await;
    let ids = server.seed().await;
    let dir = tempfile::tempdir().expect("tempdir");
    let (code, text) = failure(
        server
            .write(attach(
                short(&ids.docs),
                &dir.path().join("missing.txt"),
                None,
            ))
            .await,
    );
    assert_eq!(code, 1);
    assert!(text.contains("cannot read"), "{text}");
    let empty = dir.path().join("empty.txt");
    fs::write(&empty, b"").expect("write");
    let (code, text) = failure(server.write(attach("00000000", &empty, None)).await);
    assert_eq!(code, 1);
    assert!(text.contains("no card starts with"), "{text}");
    let big = dir.path().join("big.bin");
    fs::write(
        &big,
        vec![7u8; AccountDb::MAX_ATTACHMENT_BYTES as usize + 1],
    )
    .expect("write");
    let (code, text) = failure(server.write(attach(short(&ids.docs), &big, None)).await);
    assert_eq!(code, 1);
    assert!(text.contains("50 MiB attachment limit"), "{text}");
}
