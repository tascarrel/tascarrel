mod support;

// Exercises fragmented tool calls through a complete read, edit, and final
// response.
#[tokio::test]
async fn read_and_edit_scenario() {
    support::run_scenario("read_and_edit.json").await;
}

// Exercises stale-read rejection followed by a model-driven re-read and
// successful retry.
#[tokio::test]
async fn stale_edit_recovery_scenario() {
    support::run_scenario("stale_edit_recovery.json").await;
}

// Exercises read-before-write enforcement for existing files and atomic
// new-file creation.
#[tokio::test]
async fn guarded_writes_scenario() {
    support::run_scenario("guarded_writes.json").await;
}

// Exercises all-file preflight when one target in a multi-file edit becomes
// stale.
#[tokio::test]
async fn multi_file_edit_preflight_scenario() {
    support::run_scenario("multi_file_edit_preflight.json").await;
}

// Exercises rejection of a terminal model stream containing an incomplete tool
// call.
#[tokio::test]
async fn incomplete_tool_call_scenario() {
    support::run_scenario("incomplete_tool_call.json").await;
}

// Exercises the rule that length-limited tool arguments are never executed.
#[tokio::test]
async fn length_limited_tool_call_scenario() {
    support::run_scenario("length_limited_tool_call.json").await;
}

// Exercises malformed JSON arguments as a recoverable tool result.
#[tokio::test]
async fn malformed_tool_arguments_scenario() {
    support::run_scenario("malformed_tool_arguments.json").await;
}

// Exercises cancellation propagated through the model boundary.
#[tokio::test]
async fn cancelled_model_request_scenario() {
    support::run_scenario("cancelled_model_request.json").await;
}

// Exercises discarding an interrupted partial response and retrying only the
// current model step.
#[tokio::test]
async fn interrupted_model_retry_scenario() {
    support::run_scenario("interrupted_model_retry.json").await;
}

// Exercises rejection of ambiguous exact-text edits without changing the
// file.
#[tokio::test]
async fn ambiguous_edit_scenario() {
    support::run_scenario("ambiguous_edit.json").await;
}

// Exercises workspace-boundary enforcement for model-supplied file paths.
#[tokio::test]
async fn path_escape_scenario() {
    support::run_scenario("path_escape.json").await;
}

// Exercises dynamic system guidance and project-instruction injection.
#[tokio::test]
async fn system_prompt_contract_scenario() {
    support::run_scenario("system_prompt_contract.json").await;
}

// Exercises omission of disabled capabilities and their guidance.
#[tokio::test]
async fn enabled_tool_prompt_scenario() {
    support::run_scenario("enabled_tool_prompt.json").await;
}

// Exercises paged-read coverage enforcement followed by a safe edit retry.
#[tokio::test]
async fn paged_read_coverage_scenario() {
    support::run_scenario("paged_read_coverage.json").await;
}

// Exercises a complete supervised background-process lifecycle.
#[tokio::test]
async fn background_process_scenario() {
    support::run_scenario("background_process.json").await;
}

// Exercises foreground output and non-zero exit status reporting.
#[tokio::test]
async fn foreground_command_scenario() {
    support::run_scenario("foreground_command.json").await;
}

// Exercises foreground timeout enforcement and process-group cleanup.
#[tokio::test]
async fn command_timeout_scenario() {
    support::run_scenario("command_timeout.json").await;
}

// Exercises complete-observation enforcement for full-file rewrites.
#[tokio::test]
async fn partial_write_scenario() {
    support::run_scenario("partial_write.json").await;
}

// Exercises agent cancellation terminating a foreground process group before
// descendant side effects can occur.
#[tokio::test]
async fn cancelled_command_scenario() {
    support::run_scenario("cancelled_command.json").await;
}

// Exercises explicit termination of a background process group and cleanup of
// its retained supervisor state.
#[tokio::test]
async fn terminated_process_scenario() {
    support::run_scenario("terminated_process.json").await;
}

// Exercises bounded foreground output while retaining the most recent lines.
#[tokio::test]
async fn truncated_command_scenario() {
    support::run_scenario("truncated_command.json").await;
}

// Exercises byte-offset continuation and complete coverage for one overlong
// UTF-8 line.
#[tokio::test]
async fn overlong_line_read_scenario() {
    support::run_scenario("overlong_line_read.json").await;
}
