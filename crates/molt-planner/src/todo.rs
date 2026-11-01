//! `todo` (#264): an attempt keeps its own list of steps and how far each
//! is.
//!
//! The list is the model's account of its work and nothing more. A call
//! replaces the whole list, changes nothing in the fork and calls no
//! service. The attempt keeps the list in its checkpoint and in the
//! standing part of the record of left-out turns, so fitting the
//! conversation never loses it, and sends it out as a [`Progress::Todo`]
//! event for the interface and ACP. Whether the task is done is still the
//! done-check's and the review's to say: a list all completed is not a
//! pass.
//!
//! [`Progress::Todo`]: molt_api::progress::Progress::Todo

use molt_api::model::ToolUse;
use molt_api::planner::tools::{self as t, Todo, TodoItem, TodoStatus};
use serde_json::{json, Value};

/// Most steps a list holds.
pub(crate) const MAX_ITEMS: usize = 30;
/// Most bytes of one step.
pub(crate) const MAX_ITEM: usize = 200;

/// The tool's definition, offered to attempts after the other tools.
pub(crate) fn definition() -> Value {
    json!({
        "name": t::TODO,
        "description": "Keep your own list of the steps of this task and how far each is. Each call gives the \
                        whole list as it stands now, which replaces the one before. Use it for work of several \
                        steps: list them at the start, mark one in_progress as you begin it and completed once it \
                        is done, add steps you find and mark those you drop cancelled. It changes nothing in the \
                        workspace. The list is kept when earlier turns are left out to make room. It is your own \
                        record: the task is done only when the done-check passes, never because every step is \
                        marked completed.",
        "input_schema": {
            "type": "object",
            "properties": {
                "items": {
                    "type": "array",
                    "description": format!("Every step, in order, at most {MAX_ITEMS}."),
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": {
                                "type": "string",
                                "description": format!("The step, in one short sentence of at most {MAX_ITEM} bytes."),
                            },
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "completed", "cancelled"],
                            },
                        },
                        "required": ["content", "status"],
                        "additionalProperties": false,
                    },
                },
            },
            "required": ["items"],
            "additionalProperties": false,
        },
        "strict": true,
    })
}

/// `tools` with `todo` after them.
pub(crate) fn with_todo(mut tools: Vec<Value>) -> Vec<Value> {
    tools.push(definition());
    tools
}

/// The list a call sets, trimmed, or what is wrong with it.
pub(crate) fn parse(call: &ToolUse) -> Result<Vec<TodoItem>, String> {
    let todo: Todo = serde_json::from_value(call.input.clone())
        .map_err(|e| format!("Invalid input for {}: {e}. Check the tool's input schema and try again.", t::TODO))?;
    if todo.items.len() > MAX_ITEMS {
        return Err(format!(
            "The list has {} steps, more than the {MAX_ITEMS} it may hold. Merge small steps, and leave out those \
             that are done and no longer matter.",
            todo.items.len()
        ));
    }
    let mut items = Vec::with_capacity(todo.items.len());
    for (i, item) in todo.items.into_iter().enumerate() {
        let content = item.content.split_whitespace().collect::<Vec<_>>().join(" ");
        if content.is_empty() {
            return Err(format!("Step {} has no content: say what the step is.", i + 1));
        }
        if content.len() > MAX_ITEM {
            return Err(format!(
                "Step {} is {} bytes, more than the {MAX_ITEM} a step may have: say it in one short sentence.",
                i + 1,
                content.len()
            ));
        }
        items.push(TodoItem { content, status: item.status });
    }
    Ok(items)
}

/// What the model is told once its list is set.
pub(crate) fn result_text(items: &[TodoItem]) -> String {
    if items.is_empty() {
        return "Your list is now empty.".to_owned();
    }
    let done = count(items, TodoStatus::Completed);
    let open = items.iter().filter(|i| open(i.status)).count();
    format!(
        "Your list is saved: {done} of {} steps completed, {open} still to do. It is your own record; the task is \
         done only when the done-check passes.",
        items.len()
    )
}

/// The list as lines of the standing record, two spaces in.
pub(crate) fn lines(items: &[TodoItem]) -> String {
    items.iter().map(|i| format!("  - [{}] {}", i.status.name(), i.content)).collect::<Vec<_>>().join("\n")
}

/// The steps not yet done or dropped.
pub(crate) fn unfinished(items: &[TodoItem]) -> Vec<String> {
    items.iter().filter(|i| open(i.status)).map(|i| i.content.clone()).collect()
}

fn open(status: TodoStatus) -> bool {
    matches!(status, TodoStatus::Pending | TodoStatus::InProgress)
}

fn count(items: &[TodoItem], status: TodoStatus) -> usize {
    items.iter().filter(|i| i.status == status).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(input: Value) -> ToolUse {
        ToolUse { id: "toolu_1".into(), name: t::TODO.into(), input }
    }

    #[test]
    fn a_list_is_trimmed_bounded_and_told_back_as_the_models_own_record() {
        let items = parse(&call(json!({ "items": [
            { "content": "  read  the lexer ", "status": "completed" },
            { "content": "fix it", "status": "in_progress" },
            { "content": "old idea", "status": "cancelled" },
        ] })))
        .unwrap();
        assert_eq!(items[0].content, "read the lexer");
        assert_eq!(unfinished(&items), ["fix it"]);
        let told = result_text(&items);
        assert!(told.starts_with("Your list is saved: 1 of 3 steps completed, 1 still to do."), "{told}");
        assert!(told.contains("done only when the done-check passes"), "{told}");
        assert_eq!(lines(&items), "  - [completed] read the lexer\n  - [in_progress] fix it\n  - [cancelled] old idea");

        let many: Vec<Value> =
            (0..=MAX_ITEMS).map(|i| json!({ "content": format!("s{i}"), "status": "pending" })).collect();
        assert!(parse(&call(json!({ "items": many }))).unwrap_err().contains("more than the 30"));
        let long = "x".repeat(MAX_ITEM + 1);
        let err = parse(&call(json!({ "items": [{ "content": long, "status": "pending" }] }))).unwrap_err();
        assert!(err.starts_with("Step 1 is 201 bytes"), "{err}");
        let err = parse(&call(json!({ "items": [{ "content": " ", "status": "pending" }] }))).unwrap_err();
        assert!(err.starts_with("Step 1 has no content"), "{err}");
        let err = parse(&call(json!({ "items": [{ "content": "a", "status": "done" }] }))).unwrap_err();
        assert!(err.starts_with("Invalid input for todo"), "{err}");
        assert_eq!(result_text(&[]), "Your list is now empty.");
    }
}
