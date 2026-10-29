use std::vec;

use serde::Serialize;

use crate::db::DbResult;

/// A JSON object that ends in an array of tasks, written as the pieces a body sends one at a
/// time, so the tasks are never held whole.
#[derive(Debug)]
pub struct TaskArray {
    /// The JSON before the first task, sent first.
    head: Option<String>,
    /// The tasks still to send, in order.
    ids: vec::IntoIter<String>,
    /// Whether a task went out already, so the next one needs a comma.
    sent_task: bool,
    /// Whether the closing bytes went out.
    closed: bool,
}

impl TaskArray {
    /// Returns the array that writes `head`, serialized with an empty `tasks` array as its last
    /// field, then the tasks `ids` names.
    ///
    /// # Errors
    ///
    /// Returns an error when `head` does not serialize to an object that ends in `"tasks":[]`.
    pub fn new(head: &impl Serialize, ids: Vec<String>) -> DbResult<Self> {
        let head = serde_json::to_string(head)?;
        let open = head
            .strip_suffix("[]}")
            .filter(|open| open.ends_with("\"tasks\":"))
            .ok_or("the head must end in an empty tasks array")?;
        Ok(Self {
            head: Some(format!("{open}[")),
            ids: ids.into_iter(),
            sent_task: false,
            closed: false,
        })
    }

    /// Returns the next piece of the JSON: the head, then each task `read` finds, led by a comma
    /// after the first and skipped once gone, then the closing bytes. Returns [`None`] once every
    /// piece is out.
    pub fn next_piece<T: Serialize>(
        &mut self,
        mut read: impl FnMut(&str) -> DbResult<Option<T>>,
    ) -> DbResult<Option<Vec<u8>>> {
        if let Some(head) = self.head.take() {
            return Ok(Some(head.into_bytes()));
        }
        for id in self.ids.by_ref() {
            let Some(task) = read(&id)? else {
                continue;
            };
            let mut piece = Vec::new();
            if self.sent_task {
                piece.push(b',');
            }
            serde_json::to_writer(&mut piece, &task)?;
            self.sent_task = true;
            return Ok(Some(piece));
        }
        if self.closed {
            return Ok(None);
        }
        self.closed = true;
        Ok(Some(b"]}".to_vec()))
    }
}

#[cfg(test)]
mod tests {
    use serde::Serialize;
    use serde_json::{Value, json};

    use super::TaskArray;

    fn drain(array: &mut TaskArray, found: &[&str]) -> String {
        let mut text = String::new();
        while let Some(piece) = array
            .next_piece(|id| Ok(found.contains(&id).then(|| json!({"id": id}))))
            .expect("piece")
        {
            text.push_str(std::str::from_utf8(&piece).expect("utf8"));
        }
        text
    }

    #[test]
    fn next_piece_should_write_the_head_then_each_found_task_then_close() {
        let head = json!({"name": "Board", "tasks": []});
        let ids = vec!["a".to_string(), "gone".to_string(), "b".to_string()];
        let mut array = TaskArray::new(&head, ids).expect("array");
        let text = drain(&mut array, &["a", "b"]);
        let parsed: Value = serde_json::from_str(&text).expect("json");
        assert_eq!(
            parsed,
            json!({"name": "Board", "tasks": [{"id": "a"}, {"id": "b"}]})
        );
    }

    #[test]
    fn next_piece_should_write_an_empty_array_for_no_tasks() {
        let head = json!({"tasks": []});
        let mut array = TaskArray::new(&head, Vec::new()).expect("array");
        assert_eq!(drain(&mut array, &[]), r#"{"tasks":[]}"#);
    }

    #[test]
    fn new_should_refuse_a_head_that_does_not_end_in_tasks() {
        #[derive(Serialize)]
        struct Backwards {
            tasks: Vec<String>,
            name: &'static str,
        }
        let head = Backwards {
            tasks: Vec::new(),
            name: "Board",
        };
        assert!(TaskArray::new(&head, Vec::new()).is_err());
    }
}
