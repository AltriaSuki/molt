# Molt

Molt is a self-improving agent built on a microkernel. The kernel is small, never
changes itself and has no model inside it. Everything that thinks, remembers or
acts is a separate service that the agent can rewrite, test and roll back, the
way a crab sheds its shell to grow.

This repository is at **milestone 3: memory**. `molt do` takes a task in
plain words, settles on a done-check first, runs parallel attempts in private
forks of the project, verifies each with the check and applies the first that
passes. Each run starts from what Molt knows about the project: a map of the
code most relevant to the task, from an index of every definition and
reference that follows the files as they change, and the notes it learned
from earlier runs there. After the run, memory reads it back from the audit
log and keeps what would help next time. The agent cannot rewrite itself yet;
that starts with milestone 5.

## Architecture

```
 mutable services (agent may rewrite)     protected (human approves)
 planner · memory · tools · improver      evaluator · promotion gate
                 \                          /
                  every message, checked and logged
                               |
 ┌───────────────────────── kernel ───────────────────────────┐
 │ message bus · capabilities · supervisor · registry · audit │
 └────────────────────────────────────────────────────────────┘
```

| Part | What it does | Where |
| --- | --- | --- |
| Message bus | Routes requests, replies and events; bounded mailbox per service; per-pair FIFO | `molt-kernel/src/kernel.rs` |
| Capabilities | Unforgeable, budgeted rights to call one target; delegation can split a budget, never grow it | `molt-kernel/src/caps.rs` |
| Supervisor | Runs services as processes with rlimits, restarts with backoff, drains on stop | `molt-kernel/src/supervisor.rs` |
| Module registry | Content-addressed versions; promotion is compare-and-swap; tier rules enforced | `molt-kernel/src/registry.rs` |
| Audit log | Append-only, hash-chained, written before delivery, batched fsync; large payload values (a model conversation's messages) written once and referenced after | `molt-kernel/src/audit.rs` |
| Transports | `Transport` trait with Unix-socket and NATS implementations | `molt-transport` |
| Service SDK | Calls, replies, events, delegation | `molt-sdk` |
| Agent payloads | The request and reply types of every agent service | `molt-api` |
| Model gateway | `model.*`: the Anthropic Messages API; the only holder of the API key | `molt-gateway` |
| File and shell services | `fs.*` and `shell.*`: files, forks, diffs, merges and commands, confined to a root | `molt-tools` |
| Planner | `planner.run`: check first, parallel attempts, verify, apply; `planner.control` steers a run that is going | `molt-planner` |
| Memory | `memory.*`: notes with provenance, learned from finished runs; the project model (definitions, references, repo map) | `molt-memory` |
| Web service | `web.fetch`: pages from the hosts the user allows, over https, as text; off unless allowed | `molt-web` |

The kernel never trusts the sender named in a message. The transport
authenticates each service's endpoint (a per-service secret on Unix sockets,
per-service users and subject permissions on NATS) and the kernel stamps the
sender from that.

## Guarantees and the tests that hold them

| Guarantee | Test |
| --- | --- |
| No service can grant itself a capability or raise its own budget | `crates/molt-kernel/tests/invariants.rs` (`a_service_cannot_use…`, `delegation_can_split…`, `budgets_are_enforced…`) |
| Topic subscriptions cannot publish or delegate broader rights; denied events are audited, and revoked subscription leases stop future events on both transports | `crates/molt-kernel/tests/topic_rights.rs` (`unix_topic_subscribers_cannot_publish_or_widen_their_rights`, `nats_topic_subscribers_cannot_publish_or_widen_their_rights`), `crates/molt-kernel/src/caps.rs` (`topic_delegation_cannot_upgrade_subscription_to_publication`) |
| Check design retains unknown and cancelled model settlement; attempts cannot receive a fabricated remaining budget | `crates/molt-planner/src/runner.rs` (`design_preserves_missing_usage_unknown_prices_and_failed_call_settlement`, `cancelled_designs_return_pending_or_late_settlement_and_drop_their_fork`), `crates/molt/src/tui/app/check.rs` (`incomplete_design_cost_is_shown_and_cannot_be_spent_as_free`), `crates/molt/src/tui/app/task.rs` (`attempts_use_the_remaining_effective_budget_without_a_minimum_top_up`) |
| Designing the check stops at its share of the run's budget (one call past it at most, as the run's budget allows), a design budget can split the run's but never grow it, and no attempt starts when it runs out | `crates/molt-planner/tests/planner.rs` (`a_designer_that_never_submits_stops_within_its_share_and_says_what_to_do`, `a_design_budget_given_or_set_as_a_share_caps_designing_and_one_call_can_pass_it`), `crates/molt/tests/agent.rs` (`molt_do_stops_a_designer_that_never_submits_at_its_budget_and_says_what_to_do`) |
| No service can promote a kernel or protected component, change a tier, or add capabilities without a human | `only_humans_promote_protected_and_kernel_components` |
| No service can delete or rewrite audit entries; every message is on the record before delivery | `every_message_is_logged_before_it_is_delivered`, `the_audit_log_has_no_write_path_and_detects_tampering` |
| Audit verification and startup retain one bounded record; tail retains only the requested entries, independent of total log size | `audit_readers_keep_memory_bounded_as_the_log_grows`, `malformed_oversized_and_unterminated_records_are_never_discarded` |
| A failing service never takes the kernel down | `a_service_that_never_replies…`, `a_crashed_and_reconnected…`, `a_flooding_service…`, `garbage_on_the_wire_is_ignored`, `crates/molt/tests/e2e.rs` |
| Both transports behave the same: ordering, at-most-once, sender stamping, isolation, bounded inboxes | `crates/molt-transport/src/testkit.rs`, run by `conformance_unix.rs` and `conformance_nats.rs` |
| A Bearer-only gateway run works and sandbox commands cannot read its token | `crates/molt/tests/agent.rs` (`a_token_only_cli_run_keeps_bearer_credentials_out_of_sandbox_commands`), `crates/molt-gateway/tests/gateway.rs` (`anthropic_auth_token_uses_bearer_and_api_key_takes_precedence`) |
| `molt do` never runs a config the project ships, and the API key never reaches the commands it runs | `crates/molt/tests/agent.rs` (`the_cli_carries_out_a_task`) |
| A project only picks which environment profiles apply: it cannot name a path to mount, a variable to pass or the network, and built-in profiles never mount the Cargo home's credentials or open the network | `crates/molt-tools/src/env_profile.rs` (`a_venv_puts_its_interpreter_first_and_a_project_cannot_name_what_is_mounted`, `a_rust_project_gets_rustups_toolchain_and_the_cargo_cache_but_not_the_cargo_home`, `the_built_in_profiles_cover_four_toolchains_and_never_open_the_network`) |
| Only trusted external configuration installs hooks; formatting cannot bypass a protected done-check | `agent::tests::only_configuration_outside_the_project_can_install_hooks`, `tests/agent.rs::lifecycle_formatting_is_rechecked_and_cannot_weaken_the_protected_check` |
| A conversation sent whole every turn grows the log by its new messages, and reads back byte for byte; a changed or missing referenced value fails verification | `a_conversation_sent_whole_every_turn_is_logged_once_and_read_back_exactly`, `a_changed_or_missing_blob_is_found` |
| Reading the audit log takes a capability, and a read is recorded without a second copy of what it returned | `reading_the_log_takes_a_capability_and_is_recorded_without_a_second_copy` |
| Every learned note cites the messages behind it, and a run is learned from once | `crates/molt/tests/agent.rs` (`a_second_run_starts_with_what_the_first_learned…`), `crates/molt-memory/src/consolidate.rs` |
| A benchmark run is graded by tests the agent is not given, which its own edits cannot replace | `crates/molt-bench/src/grade.rs` (`grading_overwrites_the_agents_copy_of_a_hidden_file`), `crates/molt-bench/src/task.rs` (`overlay_replaces_files_links_and_directories_and_stays_inside`) |
| Every benchmark task fails its check as given and passes it with the reference solution | `crates/molt-bench/tests/tasks.rs` |
| Fixed review rules distinguish framework skip calls from process exit calls while still flagging actual test skips | `review::tests::process_exit_and_larger_identifiers_are_not_skip_markers_but_real_xit_still_is`, `an_approved_cli_with_a_process_exit_is_applied` |
| A workspace apply persists preimages before writing, identifies actual post-crash file states, and refuses recovery over later edits | `crates/molt-tools/src/apply.rs` (`first_middle_and_last_write_failures_can_resume_or_undo_idempotently`, `restarting_after_each_durable_stage_observes_disk_truth_and_recovers`, `later_edits_and_candidate_changes_are_preserved_and_reported_by_path`), `crates/molt-tools/tests/fs.rs` (`a_durable_merge_retains_preimages_for_files_links_modes_and_dropped_forks`), `crates/molt/tests/apply.rs` |
| Attempt symbol queries verify fork ownership and content freshness, isolate parallel views, and never update the durable project model | `crates/molt-memory/tests/forks.rs` (`rename_add_move_delete_and_shell_edits_stay_inside_the_attempt`, `unverified_wrong_workspace_and_symlink_forks_are_refused_without_widening_roots`, `an_edit_during_refresh_returns_no_stale_symbols`) |
| A benchmark starts a run only while what it spent, plus the whole `--task-usd` of that run and of each run in flight, fits in `--max-usd`; it refuses models it has no price for | `crates/molt-bench/src/run.rs` (`runs_stop_before_they_could_pass_the_budget`, `a_run_of_unknown_cost_stops_the_benchmark`), `crates/molt-bench/src/cli.rs` (`a_model_without_a_price_is_refused`) |
| A model with no price stays of unknown cost and stops the run; prices come only from the user's environment or data directory, never from a project, and a download that does not validate replaces nothing | `crates/molt-gateway/src/prices.rs` (`the_data_directory_holds_the_snapshot_and_nothing_else_is_read`, `a_download_that_does_not_validate_is_refused`, `malformed_declared_prices_stop_startup_and_name_the_item`), `crates/molt-gateway/tests/gateway.rs` (`a_snapshot_prices_a_third_party_model…`) |
| `molt do --commit` commits exactly the files the run merged and leaves what the user staged alone; `molt undo` takes it back with a new commit, never touching later changes, and the undone run no longer lends any note confidence | `crates/molt/src/git.rs` (`a_run_commit_holds_only_the_merged_files…`, `undo_refuses_files_that_changed_since_the_run`), `crates/molt/tests/agent.rs` (`a_committed_run_can_be_undone_and_memory_stops_counting_it`), `crates/molt-memory/src/notes.rs` (`an_undone_run_lends_no_note_confidence…`) |
| Part of a result, or one edited by hand, is applied only after its check and the review's fixed rules run again on just what is applied, in a fork of its own; the files chosen are in the audit log | `crates/molt-tools/tests/fs.rs` (`part_of_a_fork_can_be_diffed_narrowed…`), `crates/molt/tests/agent.rs` (`molt_apply_checks_the_chosen_files_or_a_hand_edit_again…`), `crates/molt/src/tui/app/apply.rs` (`the_apply_confirmation_applies_only_the_chosen_files…`) |
| The agent reads the web only when the user allows hosts: over https, from those hosts after every redirect, never from a loopback, private or link-local address; commands stay offline, and every page read is in the audit log | `crates/molt-web/src/allow.rs` (`only_https_urls_on_allowed_hosts_pass`, `only_public_addresses_are_public`), `crates/molt-web/tests/web.rs` (`hosts_off_the_list_are_refused_before_any_request_and_on_every_redirect`), `crates/molt/tests/agent.rs` (`allowed_web_hosts_start_the_web_service_and_offer_fetch_url_only_then`), `crates/molt-planner/tests/planner.rs` (`fetch_url_is_offered_only_to_a_planner_that_may_read_the_web`) |
| A benchmark run that fails on the model API is not counted as the agent's failure, and can be run again | `crates/molt-bench/src/agent.rs` (`a_run_that_failed_on_the_model_api…`), `crates/molt-bench/src/run.rs` (`a_benchmark_that_cannot_reach_the_model_stops`, `an_agent_that_crashes_is_charged_with_it`) |
| Issue CI rejects PR triggers and probes the sandbox before model spend | `scripts/ci/test_issue_to_pr.py::test_only_labelled_open_issues_are_eligible`, `test_sandbox_failure_posts_a_summary_and_never_starts_a_model` |
| A skill source cannot approve itself; only the CLI manages activation and withdrawal preserves evidence | `skills::tests::drafts_need_independent_evidence_and_disable_cannot_be_undone_by_learning`, `tests/service.rs::an_agent_cannot_manage_or_promote_its_own_skills` |
| Queued tasks retain their submitted execution settings and wait for apply/discard/commit settlement | `tui::app::queue::tests::queued_tasks_snapshot_execution_settings_and_plain_input_still_steers_the_current_task`, `tui::app::queue::tests::a_queue_waits_for_apply_commit_and_discard_to_settle_before_starting_the_next_task` |
| Explicit DoH avoids fake-IP bootstrap while keeping private-address refusal, bounded answers and per-redirect host checks | `molt_web::resolver::tests`, `repeated_host_dns_failures_remove_fetch_url_and_refuse_stale_calls` |
| Idle TUI undo notifies the current memory service; locked CLI undo leaves Git untouched and incomplete notices stay retryable | `session_undo_notifies_memory_and_cli_undo_refuses_its_lock_before_changing_git`, `tui::app::status::tests::quitting_waits_for_an_uninterruptible_git_operation_to_settle` |
| An interrupted task's attempt forks are not dropped before the interface has looked at them; a fork whose changes cannot be listed is kept, and what the stopped task spent is counted, with unknown calls left uncounted | `runner::tests::a_cancelled_run_leaves_its_attempts_forks_to_a_caller_that_asked_for_them`, `tui::tests::an_interrupted_fork_whose_changes_cannot_be_listed_is_kept_while_it_exists`, `tui::app::task::tests::an_interrupt_counts_what_the_stopped_run_spent_and_keeps_unknown_calls_uncounted`, `tui::app::task::tests::a_single_esc_does_not_stop_a_running_task` |
| An interface that stops mid-task keeps the run's checkpoint and forks; it goes on only when the user resumes it, under its own id and counted once, and cancelling or dropping it removes both; a damaged checkpoint or lost fork starts nothing | `crates/molt/tests/agent.rs` (`the_interfaces_session_keeps_a_stopped_runs_forks_to_resume_it_and_drops_them_when_told`), `tui::app::runs::tests::a_task_cut_off_by_a_crash_goes_on_from_its_checkpoint_only_when_asked_and_its_cost_is_counted_once`, `stopping_a_checkpointed_task_drops_its_checkpoint_and_dropping_a_stopped_run_counts_what_it_spent`, `a_damaged_checkpoint_or_a_lost_fork_is_refused_with_why_and_starts_nothing` |
| Kept results survive restart; conflicting applies leave user edits and the fork intact | `tui::app::kept::tests::plan_results_are_browsable_and_remain_available_until_apply_or_discard_succeeds`, `a_kept_session_result_survives_restart_and_refuses_workspace_conflicts` |
| Project command templates cannot set permissions or modes, and their checks are confirmed before attempts | `tui::app::command::tests::project_commands_set_one_request_and_confirm_their_check`, `tui::commands::tests::bad_files_conflicts_and_escaping_symlinks_do_not_block_other_commands` |
| Commands build into a private writable directory outside the fork, and a result never carries the build artifacts commands leave in the fork: they are reported and not applied | `crates/molt/tests/agent.rs` (`build_output_of_the_check_is_left_out_of_the_result_and_the_workspace`), `crates/molt-tools/tests/fs.rs` (`build_artifacts_that_commands_leave_in_a_fork_are_reported_and_never_merged`), `crates/molt-tools/tests/sandbox.rs` (`build_output_goes_to_a_private_writable_directory_outside_the_fork_that_later_commands_reuse`) |
| A request names a workflow and never defines one: only the planner's built-ins and its own configuration define workflows, and every result says what it was verified by (`command`, `judged`, `rules` or `none`) in the reply, `--json` and a `Molt-Verification` commit trailer | `crates/molt-planner/tests/planner.rs` (`a_request_can_name_a_workflow_but_only_one_the_planner_has`, `every_result_carries_its_verification_level`), `crates/molt/tests/agent.rs` (`the_verification_level_is_in_the_json_and_the_commit_trailers`) |
| A project's `.molt/config.toml` holds only data: one with services, hooks, `env` or `pass_env` is refused with why | `layers::tests::a_project_config_with_services_hooks_or_environment_is_refused_with_why` |
| A question (`ask`) makes no fork, designs no check and can change no file; `quick` runs exactly one attempt and designs nothing; the workflow `auto` picks is said before anything runs, never lighter than a given check allows, and the full one when the pick fails | `crates/molt-planner/tests/planner.rs` (`a_question_is_answered_in_the_workspace_without_a_fork_a_design_or_a_change`, `quick_runs_exactly_one_attempt_without_designing_and_reviews_by_the_fixed_rules`, `auto_picks_a_light_workflow_says_so_first_and_runs_it`, `with_a_check_auto_never_picks_ask_and_a_failed_pick_is_the_full_workflow`) |
| Offline embedding models are hash-pinned and cannot reintroduce forgotten or cross-project notes | `semantic::tests::pinned_models_retrieve_synonyms_and_reject_changed_files`, `semantic::tests::synonyms_are_recalled_with_scores_and_snapshots_survive_forgetting` |

## Quick start

```sh
cargo install --path crates/molt     # installs molt, molt-echo and the agent services
export ANTHROPIC_API_KEY=...
cd your-project
molt                                 # the interface: one task after another
molt do "make the parser accept trailing commas" --check "cargo test"
```

Without `--check`, the planner designs a check first (it may write test files
for it) and every attempt is held to it. The kernel on its own, with the demo
echo service:

```sh
cp molt.example.toml molt.toml        # then set its --scratch paths
molt run                              # Ctrl-C to stop
molt audit verify                     # check the hash chain
molt audit tail -n 20
```

To run across hosts on NATS, set `transport = "nats"`, run `molt nats-config`
to generate per-service credentials, put the printed `authorization` block and
`max_payload` setting in your nats-server config, and start `molt run`.

## Documentation

| Page | What it covers |
| --- | --- |
| [The interface](docs/tui.md) | `molt` with no subcommand: the terminal interface, its slash commands, project task commands, terminal colors and controls |
| [`molt do`](docs/do.md) | One task from the command line: the check and its budget, commits and `molt undo`, `molt apply`, checkpoints and `molt resume`, long tasks, project instructions, syntax errors after an edit |
| [Configuration](docs/config.md) | Config and secrets, the configuration layers, trusted lifecycle hooks |
| [Projects](docs/projects.md) | Projects and their directories, reading the web |
| [Memory](docs/memory.md) | The project model, learned notes, notes for every project, offline semantic notes |
| [Model providers and prices](docs/providers-and-prices.md) | The providers the gateway supports, and where model prices come from |
| [Linux command isolation](docs/sandbox.md) | The sandbox, environment profiles and `molt env doctor`, dependencies offline |
| [Declarative project skills](docs/skills.md) | Workflows memory drafts from verified runs, and how they are inspected, approved and withdrawn |
| [Benchmark](docs/bench.md) | Running `molt bench` and what its results mean; the task format is in [bench/README.md](bench/README.md) |
| [Recovering a workspace apply](docs/recovery.md) | How an apply into the workspace is journaled, and how to resume or undo one cut off part way |
| [Known limits](docs/limits.md) | What does not work yet, and how cancellation and model settlement are counted |
| [Tests and CI](docs/tests-and-ci.md) | Running the tests, and from an issue to a PR in CI |

## Roadmap

1. **Kernel**: bus with both transports, capabilities, supervisor, registry, audit log, invariant and conformance tests.
2. **A working agent**: Anthropic model gateway, planner, file and shell tools, parallel verified attempts, check-first.
3. **Memory** (this milestone): semantic store, recall, the living project model, consolidation.
4. **Evaluation**: deterministic replay from the audit log, regression and held-out suites, the promotion gate.
5. **Self-improvement, low risk**: prompts and skills only, auto-approved when the gate passes.
6. **Self-improvement, code**: Wasm services under wasmtime, sandbox builds, canary runs, hot swap.

## License

Molt is dual-licensed under the [MIT license](LICENSE-MIT) or the [Apache License, Version 2.0](LICENSE-APACHE), at your option. Contributions are accepted under the same terms; see [CONTRIBUTING.md](CONTRIBUTING.md#license).
