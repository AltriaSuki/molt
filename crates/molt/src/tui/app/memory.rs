//! The memory screen: notes to list, filter, inspect, correct and forget;
//! and what memory learned from a run.
use super::*;

pub struct MemoryView {
    pub query: String,
    pub notes: Vec<Recalled>,
    pub selected: usize,
    pub loading: bool,
    pub error: Option<String>,
    pub edit: Option<MemoryEdit>,
}

pub enum MemoryEdit {
    Filter(Input),
    /// Why the selected note is wrong.
    Reason(Input),
    /// Why the selected note holds in every project.
    Promote(Input),
    Change(Box<NoteChange>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ChangeStep {
    Text,
    Reason,
    Dependencies,
    Confirm,
}

pub struct NoteChange {
    pub review: ReviewRequest,
    pub text: Option<Input>,
    pub reason: Input,
    pub dependencies: Input,
    pub step: ChangeStep,
}

impl NoteChange {
    fn new(workspace: &str, note: &Recalled, correct: bool) -> Self {
        Self {
            review: ReviewRequest {
                workspace: workspace.to_owned(),
                id: note.note.id.clone(),
                expected_rev: note.note.rev,
                reason: String::new(),
                depends_on: Vec::new(),
                temporary: note.details.temporary,
            },
            text: correct.then(|| Input::with(&note.note.text)),
            reason: Input::default(),
            dependencies: Input::with(
                &note.details.dependencies.iter().map(|d| d.path.as_str()).collect::<Vec<_>>().join("; "),
            ),
            step: if correct { ChangeStep::Text } else { ChangeStep::Reason },
        }
    }

    pub fn input(&self) -> Option<&Input> {
        match self.step {
            ChangeStep::Text => self.text.as_ref(),
            ChangeStep::Reason => Some(&self.reason),
            ChangeStep::Dependencies => Some(&self.dependencies),
            ChangeStep::Confirm => None,
        }
    }

    pub(super) fn input_mut(&mut self) -> Option<&mut Input> {
        match self.step {
            ChangeStep::Text => self.text.as_mut(),
            ChangeStep::Reason => Some(&mut self.reason),
            ChangeStep::Dependencies => Some(&mut self.dependencies),
            ChangeStep::Confirm => None,
        }
    }

    fn advance(&mut self) -> Result<Option<Action>, &'static str> {
        self.step = match self.step {
            ChangeStep::Text if self.text.as_ref().is_none_or(|i| i.text.trim().is_empty()) => {
                return Err("enter the corrected note")
            }
            ChangeStep::Text => ChangeStep::Reason,
            ChangeStep::Reason if self.reason.text.trim().is_empty() => {
                return Err("say why this review or correction is justified")
            }
            ChangeStep::Reason => ChangeStep::Dependencies,
            ChangeStep::Dependencies => ChangeStep::Confirm,
            ChangeStep::Confirm => {
                let mut review = self.review.clone();
                review.reason = self.reason.text.trim().to_owned();
                review.depends_on = self
                    .dependencies
                    .text
                    .split(';')
                    .map(str::trim)
                    .filter(|p| !p.is_empty())
                    .map(str::to_owned)
                    .collect();
                return Ok(Some(Action::ChangeNote {
                    review,
                    text: self.text.as_ref().map(|i| i.text.trim().to_owned()),
                }));
            }
        };
        Ok(None)
    }
}

impl App {
    /// Load the notes again when the memory screen is open.
    pub(super) fn reload_notes(&mut self) -> Vec<Action> {
        match &mut self.screen {
            Screen::Memory(view) => {
                view.loading = true;
                vec![Action::LoadNotes { query: view.query.clone() }]
            }
            Screen::Main => Vec::new(),
        }
    }

    pub(super) fn learned(&mut self, result: Result<ConsolidateResponse, String>) {
        let head = |rest: String| Line::from(vec![span("◇ memory", Tone::Mem), span(format!("  {rest}"), Tone::Dim)]);
        match result {
            Err(e) => self.push(head(format!("learned nothing: {}", clean_line(&e)))),
            Ok(r) => {
                self.spent_usd += r.cost_usd;
                self.uncounted_calls = self.uncounted_calls.saturating_add(r.uncounted_calls);
                let count = |n: usize| {
                    if n == 1 {
                        "1 note".to_owned()
                    } else {
                        format!("{n} notes")
                    }
                };
                let mut parts = Vec::new();
                if !r.added.is_empty() {
                    parts.push(format!("learned {}", count(r.added.len())));
                }
                if !r.reinforced.is_empty() {
                    parts.push(format!("confirmed {}", count(r.reinforced.len())));
                }
                if !r.contradicted.is_empty() {
                    parts.push(format!("disputed {}", count(r.contradicted.len())));
                }
                if !r.skill_drafts.is_empty() {
                    parts.push(format!("drafted {} project skills; molt skill evaluates them", r.skill_drafts.len()));
                }
                if !r.proposed.is_empty() {
                    parts.push(format!("proposed {} for every project (/memory, p)", count(r.proposed.len())));
                }
                if parts.is_empty() {
                    parts.push(match &r.skipped {
                        Some(why) => format!("nothing new: {}", clean_line(why)),
                        None => "nothing new".into(),
                    });
                }
                self.push(head(parts.join(" · ")));
                for note in &r.added {
                    self.push(Line::from(vec![span("  + ", Tone::Mem), span(clean_line(&note.text), Tone::Plain)]));
                }
            }
        }
    }

    pub(super) fn open_used(&mut self, run: Option<String>) -> Vec<Action> {
        let Some(run) = run else {
            self.flash = Some("no task has run yet; use /memory-used RUN".into());
            return Vec::new();
        };
        self.viewer = Some(Viewer {
            title: format!("memory used: {run}"),
            lines: block("loading recorded context", "", Tone::Dim),
            scroll: 0,
        });
        vec![Action::UsedNotes { run }]
    }

    pub(super) fn memory_key(&mut self, key: &KeyEvent) -> Vec<Action> {
        let Screen::Memory(view) = &mut self.screen else { return Vec::new() };
        if let Some(MemoryEdit::Change(change)) = &mut view.edit {
            match key.code {
                KeyCode::Esc => view.edit = None,
                KeyCode::Enter => match change.advance() {
                    Ok(Some(action)) => {
                        view.edit = None;
                        view.loading = true;
                        view.error = None;
                        return vec![action];
                    }
                    Ok(None) => {}
                    Err(error) => self.flash = Some(error.into()),
                },
                KeyCode::BackTab => {
                    change.step = match change.step {
                        ChangeStep::Confirm => ChangeStep::Dependencies,
                        ChangeStep::Dependencies => ChangeStep::Reason,
                        ChangeStep::Reason if change.text.is_some() => ChangeStep::Text,
                        step => step,
                    }
                }
                KeyCode::Char('t') if change.step == ChangeStep::Confirm => {
                    change.review.temporary = !change.review.temporary
                }
                _ => {
                    if let Some(input) = change.input_mut() {
                        input.key(key);
                    }
                }
            }
            return Vec::new();
        }
        if let Some(edit) = &mut view.edit {
            match key.code {
                KeyCode::Esc => view.edit = None,
                KeyCode::Enter => match view.edit.take() {
                    Some(MemoryEdit::Filter(mut input)) => {
                        view.query = input.take().trim().to_owned();
                        view.loading = true;
                        view.selected = 0;
                        return vec![Action::LoadNotes { query: view.query.clone() }];
                    }
                    Some(MemoryEdit::Reason(mut input)) => {
                        let reason = input.take().trim().to_owned();
                        let Some(note) = view.notes.get(view.selected) else { return Vec::new() };
                        if reason.is_empty() {
                            view.edit = Some(MemoryEdit::Reason(input));
                            self.flash = Some("say why the note is wrong".into());
                            return Vec::new();
                        }
                        return vec![Action::Forget { id: note.note.id.clone(), reason }];
                    }
                    Some(MemoryEdit::Promote(mut input)) => {
                        let reason = input.take().trim().to_owned();
                        let Some(note) = view.notes.get(view.selected) else { return Vec::new() };
                        if reason.is_empty() {
                            view.edit = Some(MemoryEdit::Promote(input));
                            self.flash = Some("say why the note holds in every project".into());
                            return Vec::new();
                        }
                        return vec![Action::Promote { id: note.note.id.clone(), reason }];
                    }
                    Some(MemoryEdit::Change(_)) | None => {}
                },
                _ => {
                    if let MemoryEdit::Filter(input) | MemoryEdit::Reason(input) | MemoryEdit::Promote(input) = edit {
                        input.key(key);
                    }
                }
            }
            return Vec::new();
        }
        if view.loading && matches!(key.code, KeyCode::Char('a' | 'c' | 'f' | 'v') | KeyCode::Enter) {
            return Vec::new();
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.screen = Screen::Main,
            KeyCode::Down | KeyCode::Char('j') => {
                view.selected = (view.selected + 1).min(view.notes.len().saturating_sub(1));
            }
            KeyCode::Up | KeyCode::Char('k') => view.selected = view.selected.saturating_sub(1),
            KeyCode::Char('/') => view.edit = Some(MemoryEdit::Filter(Input::with(&view.query))),
            KeyCode::Char('f') if !view.notes.is_empty() => view.edit = Some(MemoryEdit::Reason(Input::default())),
            KeyCode::Char('p') => match view.notes.get(view.selected) {
                Some(note) if note.layer == Layer::User => {
                    self.flash = Some("that note holds everywhere already".into())
                }
                Some(_) => view.edit = Some(MemoryEdit::Promote(Input::default())),
                None => {}
            },
            KeyCode::Char('a') | KeyCode::Char('c') => {
                if let Some(note) = view.notes.get(view.selected) {
                    view.edit = Some(MemoryEdit::Change(Box::new(NoteChange::new(
                        &self.workspace_path,
                        note,
                        key.code == KeyCode::Char('c'),
                    ))));
                }
            }
            KeyCode::Enter | KeyCode::Char('v') => {
                if let Some(note) = view.notes.get(view.selected) {
                    self.viewer = Some(Viewer {
                        title: format!("{} revision {}", clean_line(&note.note.id), note.note.rev),
                        lines: memory_lines(note),
                        scroll: 0,
                    });
                }
            }
            KeyCode::Char('u') => return self.open_used(self.last_run.clone()),
            KeyCode::Char('r') => {
                view.loading = true;
                return vec![Action::LoadNotes { query: view.query.clone() }];
            }
            _ => {}
        }
        Vec::new()
    }
}

/// Shared by the detail pane and its scrollable full view. Every field is
/// display data, including evidence IDs, dependency paths, and note text.
pub fn memory_lines(r: &Recalled) -> Vec<Line<'static>> {
    let n = &r.note;
    let mut fields = vec![
        format!("{} revision {} · {}", n.id, n.rev, n.kind.as_str()),
        format!("score {:.3} · confidence {:.3} · recency {:.3}", r.score, r.reason.confidence, r.reason.recency),
        format!("keywords: {}", r.reason.keywords.as_deref().unwrap_or("none (confidence and recency)")),
        format!("relevance: {}", r.reason.relevance.map_or("unavailable".into(), |v| format!("{v:.3}"))),
    ];
    if r.layer == Layer::User {
        fields.push("layer: yours, for every project".into());
    } else if r.details.everywhere {
        fields.push("layer: this project · proposed for every project (p to promote)".into());
    } else if let Some(to) = &r.details.promoted_to {
        fields.push(format!("layer: this project · promoted as {to}"));
    }
    if r.details.temporary {
        fields.push("temporary experience · verify before reuse".into());
    }
    if !r.details.needs_review.is_empty() {
        fields.push(format!("needs review: {}", r.details.needs_review.join(", ")));
    }
    fields.push(format!("review reason: {}", if r.details.reason.is_empty() { "none" } else { &r.details.reason }));
    if let Some(similarity) = r.reason.similarity {
        fields.push(format!("cosine similarity: {similarity:.3}"));
    }
    if let Some(hash) = &r.reason.model_sha256 {
        fields.push(format!("embedding model SHA-256: {hash}"));
    }
    if let Some(weight) = r.reason.semantic_weight {
        fields.push(format!("semantic weight: {weight:.3}"));
    }
    for d in &r.details.dependencies {
        fields.push(format!("depends on: {} · sha256 {}", d.path, d.sha256));
    }
    if let Some(id) = &r.details.supersedes {
        fields.push(format!("supersedes: {id}"));
    }
    fields.extend([
        format!("source: {}@{} · run {}", n.provenance.service, n.provenance.version, n.provenance.trace),
        format!("learned: {} · confirmed {}× · confidence {:.2}", text::when(n.created_ms), n.reinforced, n.confidence),
        format!("disputes: {}", n.conflicts.join(", ")),
        format!("evidence: {}", n.provenance.events.join(", ")),
        String::new(),
        n.text.clone(),
    ]);
    fields.into_iter().flat_map(|field| block(&field, "", Tone::Plain)).collect()
}

pub(super) fn snapshot_lines(snapshots: &[RecallSnapshot]) -> Vec<Line<'static>> {
    if snapshots.is_empty() {
        return block("no recorded memory context in this project", "", Tone::Dim);
    }
    let mut lines = Vec::new();
    for s in snapshots {
        lines.extend(block(
            &format!("run {} · call {} · {}\nquery: {}", s.run, s.call, s.purpose, s.query),
            "",
            Tone::Mem,
        ));
        if s.notes.is_empty() {
            lines.extend(block("no notes returned", "", Tone::Dim));
        }
        for r in &s.notes {
            lines.extend(memory_lines(r));
            lines.push(Line::default());
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::super::command::help;
    use super::super::testing::*;
    use super::*;

    #[test]
    fn recalled_notes_and_learning_are_shown_once() {
        let mut app = App::new("/w", None);
        up(&mut app);
        let actions = type_in(&mut app, "task");
        let [Action::Design { trace, .. }] = actions.as_slice() else { panic!() };
        let run = trace.to_string();
        let recalled = Progress::Recalled {
            run: run.clone(),
            notes: vec![RecalledNote { id: "note_1".into(), text: "Tests need --features x".into(), confidence: 0.78 }],
            map_tokens: Some(2800),
        };
        app.update(Msg::Progress(recalled.clone()));
        app.update(Msg::Progress(recalled));
        app.update(Msg::Progress(Progress::Note { run: run.clone(), message: "project model: 3 files".into() }));
        app.update(Msg::Progress(Progress::Note { run, message: "project model: 3 files".into() }));
        assert!(
            log(&app).ends_with(&format!(
                "◇ memory  map 2800 tokens · 1 note\n  /memory-used {trace} · recorded recall context\n  0.78 Tests need --features x\n  project model: 3 files"
            )),
            "{}",
            log(&app)
        );
    }

    #[test]
    fn memory_learns_with_the_model_set_for_it() {
        // What the next run's learning asks for, after `commands`.
        let learns_with = |commands: &[&str]| {
            let mut app = App::new("/w", None);
            up(&mut app);
            for c in commands.iter().chain(&["/check make test"]) {
                assert!(type_in(&mut app, c).is_empty(), "{c}");
            }
            let actions = type_in(&mut app, "task");
            let [Action::Run { trace, .. }] = actions.as_slice() else { panic!("{actions:?}") };
            let actions =
                app.update(Msg::Ran { trace: trace.clone(), result: Box::new(Ok(passed(false, Some("/f")))) });
            // Confirm mode applies the passing result first.
            let [Action::Apply { .. }, Action::Learn { model, .. }] = actions.as_slice() else { panic!("{actions:?}") };
            model.clone()
        };
        assert_eq!(learns_with(&[]), None, "memory's own default");
        assert_eq!(learns_with(&["/model opus"]), None, "/model is for the attempts only");
        assert_eq!(learns_with(&["/memory-model glm-4.6"]).as_deref(), Some("glm-4.6"));
        assert_eq!(learns_with(&["/model glm-4.6", "/memory-model same"]).as_deref(), Some("glm-4.6"));
        assert_eq!(learns_with(&["/memory-model same"]), None, "the attempts use the default too");
        assert_eq!(learns_with(&["/memory-model haiku", "/memory-model default"]), None);

        let mut app = App::new("/w", None);
        up(&mut app);
        type_in(&mut app, "/memory-model glm-4.6");
        assert_eq!(app.flash.as_deref(), Some("memory model: glm-4.6"));
        type_in(&mut app, "/memory-model same");
        assert_eq!(app.flash.as_deref(), Some("memory model: the attempts' model"));
        assert!(plain(&help()).iter().any(|l| l.contains("/memory-model NAME")));
    }

    #[test]
    fn learning_that_was_skipped_says_why() {
        let mut app = App::new("/w", None);
        let skipped = ConsolidateResponse {
            skill_drafts: Vec::new(),
            added: vec![],
            reinforced: vec![],
            contradicted: vec![],
            skipped: Some("the episode held nothing worth keeping".into()),
            proposed: vec![],
            usage: Usage::default(),
            cost_usd: 0.01,
            uncounted_calls: 1,
        };
        app.learned(Ok(skipped));
        assert_eq!(app.uncounted_calls, 1, "a learning call without a price leaves the session's spending a subtotal");
        assert!(log(&app).ends_with("◇ memory  nothing new: the episode held nothing worth keeping"), "{}", log(&app));
    }

    #[test]
    fn notes_for_every_project_are_remembered_and_promoted_by_hand() {
        let mut app = App::new("/w", None);
        up(&mut app);
        assert_eq!(
            type_in(&mut app, "/remember Answer in Chinese."),
            [Action::Remember { text: "Answer in Chinese.".into() }]
        );
        assert!(type_in(&mut app, "/remember").is_empty(), "nothing to remember");
        app.update(Msg::Remembered(Ok("note_u".into())));
        assert_eq!(app.flash.as_deref(), Some("remembered note_u for every project"));

        type_in(&mut app, "/memory");
        let note = |id: &str, layer: Layer| Recalled {
            note: serde_json::from_value(serde_json::json!({
                "id": id, "kind": "fact", "text": "t", "confidence": 0.5, "created_ms": 0, "updated_ms": 0,
                "provenance": { "trace": "tr", "service": "memory", "version": "v" }, "rev": 1
            }))
            .unwrap(),
            score: 1.0,
            reason: Default::default(),
            details: Default::default(),
            layer,
        };
        app.update(Msg::Notes(Ok(vec![note("note_u", Layer::User), note("note_p", Layer::Project)])));
        app.update(key(KeyCode::Char('p')));
        assert_eq!(app.flash.as_deref(), Some("that note holds everywhere already"));
        app.update(key(KeyCode::Down));
        app.update(key(KeyCode::Char('p')));
        assert!(app.update(key(KeyCode::Enter)).is_empty(), "a reason is required");
        app.update(Msg::Paste("all my projects use make".into()));
        assert_eq!(
            app.update(key(KeyCode::Enter)),
            [Action::Promote { id: "note_p".into(), reason: "all my projects use make".into() }]
        );
        assert_eq!(
            app.update(Msg::Promoted { id: "note_p".into(), result: Ok("note_c".into()) }),
            [Action::LoadNotes { query: String::new() }]
        );
        assert_eq!(app.flash.as_deref(), Some("note_p now holds in every project as note_c"));
    }

    #[test]
    fn the_memory_screen_lists_filters_and_forgets() {
        let mut app = App::new("/w", None);
        up(&mut app);
        assert_eq!(type_in(&mut app, "/memory parser"), [Action::LoadNotes { query: "parser".into() }]);
        let note = |id: &str| Recalled {
            note: serde_json::from_value(serde_json::json!({
                "id": id, "kind": "fact", "text": "t", "confidence": 0.5, "created_ms": 0, "updated_ms": 0,
                "provenance": { "trace": "tr", "service": "memory", "version": "v" }, "rev": 1
            }))
            .unwrap(),
            score: 1.0,
            reason: Default::default(),
            details: Default::default(),
            layer: Default::default(),
        };
        app.update(Msg::Notes(Ok(vec![note("note_a"), note("note_b")])));
        app.update(key(KeyCode::Down));
        app.update(key(KeyCode::Char('f')));
        assert!(app.update(key(KeyCode::Enter)).is_empty(), "a reason is required");
        app.update(Msg::Paste("the build moved to make".into()));
        assert_eq!(
            app.update(key(KeyCode::Enter)),
            [Action::Forget { id: "note_b".into(), reason: "the build moved to make".into() }]
        );
        assert_eq!(
            app.update(Msg::Forgot { id: "note_b".into(), result: Ok(true) }),
            [Action::LoadNotes { query: "parser".into() }]
        );
        app.update(key(KeyCode::Char('/')));
        app.update(ctrl_key('u'));
        app.update(Msg::Paste("lexer".into()));
        assert_eq!(app.update(key(KeyCode::Enter)), [Action::LoadNotes { query: "lexer".into() }]);
        app.update(key(KeyCode::Esc));
        assert!(matches!(app.screen, Screen::Main));
    }

    #[test]
    fn memory_inspection_shows_the_reason_and_reviews_the_revision_the_person_saw() {
        let mut app = App::new("/w", None);
        up(&mut app);
        type_in(&mut app, "/memory cargo");
        app.update(Msg::Notes(Ok(vec![inspected_note()])));
        let frame = frame_text(&mut app);
        for expected in [
            "[review]",
            "[temporary]",
            "revision 7",
            "keywords: cargo",
            "needs review: Cargo.toml",
            "old_hash",
            "source_event",
        ] {
            assert!(frame.contains(expected), "missing {expected}: {frame}");
        }
        app.update(key(KeyCode::Char('v')));
        assert!(plain(&app.viewer.as_ref().unwrap().lines).join("\n").contains("relevance: 0.900"));
        app.update(key(KeyCode::Esc));
        app.update(key(KeyCode::Char('a')));
        assert!(app.update(key(KeyCode::Enter)).is_empty());
        assert!(app.flash.as_ref().unwrap().contains("justified"));
        app.update(Msg::Paste("checked current config".into()));
        app.update(key(KeyCode::Enter));
        // A refresh while editing must not silently raise the saved lease.
        let mut newer = inspected_note();
        newer.note.rev = 8;
        app.update(Msg::Notes(Ok(vec![newer.clone()])));
        app.update(ctrl_key('u'));
        app.update(Msg::Paste("Cargo.toml; src/lib.rs".into()));
        app.update(key(KeyCode::Enter));
        app.update(key(KeyCode::Char('t')));
        let actions = app.update(key(KeyCode::Enter));
        assert_eq!(
            actions,
            [Action::ChangeNote {
                review: ReviewRequest {
                    workspace: "/w".into(),
                    id: "note_config".into(),
                    expected_rev: 7,
                    reason: "checked current config".into(),
                    depends_on: vec!["Cargo.toml".into(), "src/lib.rs".into()],
                    temporary: false,
                },
                text: None
            }]
        );
        assert!(app
            .update(Msg::ChangedNote(Err("note changed: expected revision 7, current 8; inspect it again".into())))
            .is_empty());
        let Screen::Memory(view) = &app.screen else { panic!() };
        assert_eq!(view.notes, [newer]);
        assert!(!view.loading && view.error.as_ref().unwrap().contains("current 8"));
        assert!(frame_text(&mut app).contains("current 8"));
    }

    #[test]
    fn correcting_a_note_requires_a_reason_and_confirmation_and_refreshes_after_success() {
        let mut app = App::new("/w", None);
        up(&mut app);
        type_in(&mut app, "/memory");
        let note = inspected_note();
        app.update(Msg::Notes(Ok(vec![note.clone()])));
        app.update(key(KeyCode::Char('c')));
        app.update(ctrl_key('u'));
        app.update(Msg::Paste("Build with make".into()));
        app.update(key(KeyCode::Enter));
        app.update(Msg::Paste("verified the migration".into()));
        app.update(key(KeyCode::Enter));
        app.update(ctrl_key('u'));
        app.update(key(KeyCode::Enter));
        assert!(frame_text(&mut app).contains("Enter save"));
        assert_eq!(
            app.update(key(KeyCode::Enter)),
            [Action::ChangeNote {
                review: ReviewRequest {
                    workspace: "/w".into(),
                    id: note.note.id.clone(),
                    expected_rev: 7,
                    reason: "verified the migration".into(),
                    depends_on: vec![],
                    temporary: true,
                },
                text: Some("Build with make".into())
            }]
        );
        assert_eq!(
            app.update(Msg::ChangedNote(Ok(ReviewResponse { note: note.note, details: note.details }))),
            [Action::LoadNotes { query: String::new() }]
        );
        // Cancelling a later form never sends a mutation.
        app.update(Msg::Notes(Ok(vec![inspected_note()])));
        app.update(key(KeyCode::Char('c')));
        assert!(app.update(key(KeyCode::Esc)).is_empty());
        let Screen::Memory(view) = &app.screen else { panic!() };
        assert!(view.edit.is_none());
    }

    #[test]
    fn used_memory_shows_the_immutable_run_context_and_ignores_late_viewer_replies() {
        let mut app = App::new("/w", None);
        up(&mut app);
        let actions = type_in(&mut app, "task");
        let [Action::Design { trace, .. }] = actions.as_slice() else { panic!() };
        let run = trace.to_string();
        assert_eq!(type_in(&mut app, "/memory-used"), [Action::UsedNotes { run: run.clone() }]);
        let record = RecallSnapshot {
            run: run.clone(),
            call: "captured_call".into(),
            workspace: "/w".into(),
            query: "cargo".into(),
            purpose: "context".into(),
            created_ms: 1,
            notes: vec![inspected_note()],
        };
        app.update(Msg::UsedNotes { run: run.clone(), result: Ok(vec![record.clone()]) });
        let frame = frame_text(&mut app);
        assert!(frame.contains("captured_call") && frame.contains("revision 7") && frame.contains("Build with cargo"));
        app.update(key(KeyCode::Esc));
        app.update(Msg::UsedNotes { run: run.clone(), result: Ok(vec![record]) });
        assert!(app.viewer.is_none());
        type_in(&mut app, "/memory");
        app.update(Msg::Notes(Ok(vec![])));
        assert_eq!(app.update(key(KeyCode::Char('u'))), [Action::UsedNotes { run }]);
    }
}
