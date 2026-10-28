//! Pure, archive-backed selection of working conversation history.
//!
//! The caller must save the *original* messages to `archive_source` before using
//! this result. References always use indices in that original slice. Nothing
//! here executes tools, summarizes observations as facts, or modifies an archive.
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    Capacity,
    Repetition,
    Recovery,
}

#[derive(Debug)]
pub struct Compacted {
    /// Chronological messages, excluding system/developer prompts. Tool batches
    /// are selected together; their original calls and arguments are retained.
    pub recent: Vec<Value>,
    /// Historical references and explicitly attributed tool metadata, not new
    /// instructions or assertions about the current state of the project.
    pub evidence: Vec<Value>,
    pub stats: Value,
}

/// A deliberately conservative, tokenizer-independent estimate for arbitrary
/// JSON. UTF-8 bytes are counted rather than Unicode scalar values; non-ASCII
/// bytes and JSON punctuation receive extra weight. Local image references also
/// incur a pixel budget, even though their encoded pixels are absent from JSON.
pub fn estimate_tokens(value: &Value) -> usize {
    let text = value.to_string();
    let heavy = text
        .bytes()
        .filter(|b| !b.is_ascii() || b.is_ascii_punctuation())
        .count();
    text.len()
        .saturating_add(heavy)
        .div_ceil(2)
        .saturating_add(16)
        .saturating_add(image_tokens(value))
}

fn image_tokens(value: &Value) -> usize {
    match value {
        Value::Array(values) => values
            .iter()
            .map(image_tokens)
            .fold(0, usize::saturating_add),
        Value::Object(fields) => {
            let is_image = matches!(
                fields.get("type").and_then(Value::as_str),
                Some("image" | "image_url" | "input_image")
            );
            let own = if is_image { image_cost(value) } else { 0 };
            fields.iter().fold(own, |total, (key, value)| {
                total.saturating_add(if key == "_chuggin_images" {
                    value.as_array().map_or(0, |images| {
                        images.iter().map(image_cost).fold(0, usize::saturating_add)
                    })
                } else {
                    image_tokens(value)
                })
            })
        }
        _ => 0,
    }
}

fn image_cost(image: &Value) -> usize {
    let width = image["width"].as_u64();
    let height = image["height"].as_u64();
    match width.zip(height).filter(|(w, h)| *w > 0 && *h > 0) {
        Some((width, height)) => {
            let patches = width.div_ceil(32).saturating_mul(height.div_ceil(32));
            usize::try_from(patches.saturating_mul(4).saturating_add(1024)).unwrap_or(usize::MAX)
        }
        None => 16_384,
    }
}

#[derive(Clone)]
struct Message {
    index: usize,
    value: Value,
}

struct Group {
    messages: Vec<Message>,
    pending: bool,
    important: bool,
}

fn cost(messages: &[Message]) -> usize {
    messages
        .iter()
        .map(|m| estimate_tokens(&m.value))
        .fold(0, usize::saturating_add)
}

fn decoded_content(message: &Value) -> Value {
    match message.get("content") {
        Some(Value::String(text)) => serde_json::from_str(text).unwrap_or(Value::Null),
        Some(value) => value.clone(),
        None => Value::Null,
    }
}

fn relevant_key(key: &str) -> bool {
    matches!(
        key,
        "ok" | "success"
            | "passed"
            | "is_error"
            | "error"
            | "errors"
            | "exit_code"
            | "timed_out"
            | "running"
            | "status"
            | "stop_reason"
            | "command_id"
            | "session_id"
            | "job_id"
            | "cell_id"
            | "log_id"
            | "next_output_offset"
            | "next_offset"
            | "offset"
            | "output_log_changed"
            | "pending_checks"
            | "checks_complete"
            | "interrupted"
            | "recovered"
            | "mode"
            | "argv"
            | "workspace"
            | "artifact_directory"
            | "process_namespace"
            | "origin_task_id"
            | "origin_cycle"
    )
}

/// Keep observed status/handles in their original structure. We never infer a
/// successful check from prose such as "all tests passed" in a tool's output.
fn metadata(value: &Value) -> Option<Value> {
    match value {
        Value::Object(fields) => {
            let result: Map<_, _> = fields
                .iter()
                .filter_map(|(key, value)| {
                    if relevant_key(key) {
                        Some((key.clone(), value.clone()))
                    } else {
                        metadata(value).map(|value| (key.clone(), value))
                    }
                })
                .collect();
            (!result.is_empty()).then_some(Value::Object(result))
        }
        Value::Array(values) => {
            let result: Vec<_> = values.iter().filter_map(metadata).collect();
            (!result.is_empty()).then_some(Value::Array(result))
        }
        Value::String(text) if text.trim_start().starts_with(['{', '[']) => {
            serde_json::from_str(text)
                .ok()
                .and_then(|value| metadata(&value))
        }
        _ => None,
    }
}

fn important(value: &Value) -> bool {
    match value {
        Value::Object(fields) => fields.iter().any(|(key, value)| {
            matches!(
                key.as_str(),
                "command_id" | "session_id" | "job_id" | "cell_id"
            ) && !value.is_null()
                || matches!(key.as_str(), "ok" | "passed" | "success") && value == false
                || matches!(key.as_str(), "running" | "is_error" | "timed_out") && value == true
                || key == "exit_code" && value.as_i64().is_some_and(|code| code != 0)
                || matches!(key.as_str(), "error" | "errors")
                    && !value.is_null()
                    && value != ""
                    && value != false
                    && value.as_array().is_none_or(|values| !values.is_empty())
                || key == "status"
                    && matches!(
                        value.as_str(),
                        Some(
                            "running"
                                | "pending"
                                | "queued"
                                | "awaiting_input"
                                | "failed"
                                | "error"
                                | "interrupted"
                                | "timed_out"
                        )
                    )
                || important(value)
        }),
        Value::Array(values) => values.iter().any(important),
        Value::String(text) if text.trim_start().starts_with(['{', '[']) => {
            serde_json::from_str(text)
                .ok()
                .is_some_and(|value| important(&value))
        }
        _ => false,
    }
}

fn reference(source: &str, index: usize, kind: &str) -> Value {
    json!({"source_id":source,"message_index":index,"kind":kind,
        "historical_evidence":true,"retrieve_with":"read_history"})
}

fn tool_reference(source: &str, message: &Message) -> Value {
    let mut result = reference(source, message.index, "archived_tool_result");
    for key in ["role", "tool_call_id", "tool_name", "name"] {
        if let Some(value) = message.value.get(key) {
            result[key] = value.clone();
        }
    }
    if let Some(observed) = metadata(&decoded_content(&message.value)) {
        result["observed_metadata"] = observed;
    }
    if message.value.get("_chuggin_images").is_some() {
        result["images_in_archive"] = json!(true);
    }
    result
}

fn group_reference(source: &str, group: &Group) -> Value {
    let first = &group.messages[0];
    let mut result = reference(source, first.index, "archived_exchange");
    result["role"] = first.value["role"].clone();
    let tools: Vec<_> = group
        .messages
        .iter()
        .filter(|m| m.value["role"] == "tool")
        .map(|m| tool_reference(source, m))
        .collect();
    if !tools.is_empty() {
        result["tool_results"] = json!(tools);
    } else if let Some(observed) = metadata(&decoded_content(&first.value)) {
        result["observed_metadata"] = observed;
    }
    result
}

fn short_group(source: &str, group: &Group) -> Vec<Message> {
    group
        .messages
        .iter()
        .map(|message| {
            let mut message = message.clone();
            if message.value["role"] == "tool" {
                let archived = tool_reference(source, &message);
                let mut shortened = message.value.clone();
                shortened["content"] = json!(archived.to_string());
                // Pixels are available from the original message, not silently
                // retained as an unbudgeted attachment to the compact text.
                if let Some(object) = shortened.as_object_mut() {
                    object.remove("_chuggin_images");
                    object.remove("_chuggin_images_observed");
                    object.remove("_chuggin_responses");
                }
                if estimate_tokens(&shortened) < estimate_tokens(&message.value) {
                    message.value = shortened;
                }
            }
            message
        })
        .collect()
}

fn calls(message: &Value) -> &[Value] {
    message["tool_calls"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn saved_progress_note(group: &Group) -> bool {
    group.messages.iter().any(|message| {
        message.value["role"] == "assistant"
            && calls(&message.value)
                .iter()
                .any(|call| call["function"]["name"] == "save_progress_note")
    })
}

fn memory_tool(group: &Group) -> bool {
    saved_progress_note(group)
        || group.messages.iter().any(|message| {
            message.value["role"] == "assistant"
                && calls(&message.value).iter().any(|call| {
                    matches!(
                        call["function"]["name"].as_str(),
                        Some("read_progress_note" | "read_task_evidence" | "read_context_memory")
                    )
                })
        })
}

fn quarantined_memory_reference(source: &str, group: &Group) -> Value {
    let mut entry = group_reference(source, group);
    entry["kind"] = json!(if saved_progress_note(group) {
        "quarantined_progress_note_exchange"
    } else {
        "quarantined_memory_tool_exchange"
    });
    // Memory reads can embed model-authored JSON, including strings under keys
    // like `error`. Do not copy those strings into a metadata preview and seed
    // the same narration through another route. The fresh handoff independently
    // carries controller evidence and explicitly structured episode records.
    if let Some(object) = entry.as_object_mut() {
        object.remove("observed_metadata");
    }
    if let Some(results) = entry["tool_results"].as_array_mut() {
        for result in results {
            if let Some(object) = result.as_object_mut() {
                object.remove("observed_metadata");
            }
        }
    }
    entry
}

fn inspection(name: &str, arguments: &Value) -> bool {
    matches!(
        name,
        "read_file"
            | "read_progress_note"
            | "read_context_memory"
            | "read_task_evidence"
            | "read_history"
            | "search_history"
            | "read_agent_result"
            | "read_command_log"
            | "search"
            | "list_files"
            | "project_map"
            | "lookup_symbol"
            | "read_messages"
            | "computer_info"
            | "inspect_document"
            | "web_search"
            | "read_web_page"
    ) || name == "browser"
        && matches!(
            arguments["action"].as_str(),
            Some("read" | "tabs" | "status")
        )
        || name == "notebook"
            && matches!(
                arguments["action"].as_str(),
                Some("read" | "list" | "search")
            )
}

fn inspection_fingerprint(group: &Group) -> Option<String> {
    if group.pending {
        return None;
    }
    let assistant = &group.messages.first()?.value;
    let requested = calls(assistant);
    if requested.is_empty() {
        return None;
    }
    let mut results = Vec::new();
    let mut claimed = vec![false; group.messages.len()];
    for call in requested {
        let name = call["function"]["name"].as_str()?;
        let raw_arguments = &call["function"]["arguments"];
        let arguments = raw_arguments
            .as_str()
            .and_then(|text| serde_json::from_str::<Value>(text).ok())
            .unwrap_or_else(|| raw_arguments.clone());
        if !inspection(name, &arguments) {
            return None;
        }
        let (index, result) =
            group
                .messages
                .iter()
                .enumerate()
                .skip(1)
                .find(|(index, message)| {
                    !claimed[*index]
                        && message.value["role"] == "tool"
                        && matches_call(call, &message.value)
                })?;
        claimed[index] = true;
        let mut result = result.value.clone();
        if let Some(object) = result.as_object_mut() {
            object.remove("tool_call_id");
        }
        // Include attachment references and all other result metadata so equal
        // text with a new image or changed status still counts as new evidence.
        results.push(json!({"tool":name,"arguments":arguments,"result":result}));
    }
    Some(Value::Array(results).to_string())
}

fn repeated_inspections(groups: &[Group]) -> Vec<bool> {
    let mut fingerprints = BTreeMap::<String, Vec<usize>>::new();
    for (index, group) in groups.iter().enumerate() {
        if let Some(fingerprint) = inspection_fingerprint(group) {
            fingerprints.entry(fingerprint).or_default().push(index);
        }
    }
    let mut repeated = vec![false; groups.len()];
    for indices in fingerprints
        .into_values()
        .filter(|indices| indices.len() >= 3)
    {
        for index in indices {
            repeated[index] = true;
        }
    }
    repeated
}

fn matches_call(call: &Value, message: &Value) -> bool {
    if let Some(id) = call.get("id").filter(|id| !id.is_null()) {
        message.get("tool_call_id") == Some(id)
    } else {
        call["function"]["name"].as_str().is_some_and(|name| {
            message
                .get("tool_name")
                .or_else(|| message.get("name"))
                .and_then(Value::as_str)
                == Some(name)
        })
    }
}

fn groups(messages: Vec<Message>) -> (Vec<Group>, Vec<Message>) {
    let mut result = Vec::new();
    let mut orphans = Vec::new();
    let mut iter = messages.into_iter().peekable();
    while let Some(message) = iter.next() {
        if message.value["role"] == "tool" {
            orphans.push(message);
            continue;
        }
        let expected = calls(&message.value).to_vec();
        let mut answered = vec![false; expected.len()];
        let mut batch = vec![message];
        if !expected.is_empty() {
            while iter
                .peek()
                .is_some_and(|message| message.value["role"] == "tool")
            {
                let reply = iter.next().unwrap();
                if let Some(index) = expected
                    .iter()
                    .enumerate()
                    .position(|(index, call)| !answered[index] && matches_call(call, &reply.value))
                {
                    answered[index] = true;
                    batch.push(reply);
                } else {
                    orphans.push(reply);
                }
            }
        }
        let relevant = batch
            .iter()
            .any(|message| important(&decoded_content(&message.value)));
        result.push(Group {
            messages: batch,
            pending: answered.contains(&false),
            important: relevant,
        });
    }
    (result, orphans)
}

fn quarantine_reasoning(message: &mut Value) -> bool {
    let Some(object) = message.as_object_mut() else {
        return false;
    };
    let mut removed = false;
    for key in [
        "_chuggin_reasoning",
        "reasoning_content",
        "reasoning_details",
    ] {
        removed |= object.remove(key).is_some();
    }
    let native_reasoning = object.get("_chuggin_responses").is_some_and(|native| {
        native["items"]
            .as_array()
            .is_none_or(|items| items.iter().any(|item| item["type"] == "reasoning"))
    });
    if native_reasoning {
        // Encrypted reasoning cannot be inspected for the pattern that caused
        // a thinking-watch recovery. Invalidate its native cache and use the
        // exact generic content/calls; the Responses adapter already supports
        // that representation when changing providers or model targets.
        object.remove("_chuggin_responses");
        removed = true;
    }
    removed
}

fn narration(messages: &[Value], reason: Reason) -> (Vec<Message>, Vec<usize>) {
    let cross_response = if reason == Reason::Repetition {
        crate::prose_watch::repeated_narration_indices(messages)
    } else {
        Vec::new()
    };
    let mut duplicates = BTreeMap::<String, usize>::new();
    if reason == Reason::Repetition {
        for message in messages.iter().filter(|m| m["role"] == "assistant") {
            if let Some(text) = message["content"].as_str() {
                // Exact code/data repetitions can be relevant observations.
                // The streaming prose detector separately handles prose tails.
                if text.trim_start().starts_with(['{', '['])
                    || text.contains("```")
                    || text.contains("~~~")
                {
                    continue;
                }
                let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
                if !normalized.is_empty() {
                    *duplicates.entry(normalized).or_default() += 1;
                }
            }
        }
    }
    let mut result = Vec::new();
    let mut quarantined = Vec::new();
    for (index, original) in messages.iter().enumerate() {
        if matches!(original["role"].as_str(), Some("system" | "developer")) {
            continue;
        }
        let mut value = original.clone();
        if reason == Reason::Repetition
            && original["role"] == "assistant"
            && quarantine_reasoning(&mut value)
        {
            quarantined.push(index);
        }
        if reason == Reason::Repetition
            && original["role"] == "assistant"
            && let Some(text) = original["content"].as_str()
        {
            let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
            let repeated_message = cross_response.contains(&index)
                || duplicates.get(&normalized).is_some_and(|count| {
                    *count >= 3 && normalized.len().saturating_mul(*count) >= 128
                });
            let start = if repeated_message {
                Some(0)
            } else {
                crate::repetition::start(text)
            };
            if let Some(start) = start {
                if quarantined.last() != Some(&index) {
                    quarantined.push(index);
                }
                let prefix = text[..start].trim_end();
                if prefix.is_empty() && calls(original).is_empty() {
                    continue;
                }
                value["content"] = json!(prefix);
                // Saved native Responses items otherwise bypass content and
                // could replay the very narration that was quarantined.
                if let Some(object) = value.as_object_mut() {
                    object.remove("_chuggin_responses");
                }
            }
        }
        result.push(Message { index, value });
    }
    (result, quarantined)
}

/// Select history within `budget_tokens`, including returned evidence. The only
/// permitted overage is an unresolved tool batch: losing a pending call/result
/// would change execution state. Completed oversized exchanges receive archive
/// references instead. The caller should surface `budget_exceeded` rather than
/// repeatedly refreshing an oversized pending batch.
pub fn compact(
    messages: &[Value],
    archive_source: &str,
    reason: Reason,
    budget_tokens: usize,
) -> Compacted {
    let (prepared, quarantined) = narration(messages, reason);
    let (groups, orphans) = groups(prepared);
    // Executed arguments remain immutable evidence. A completed note save can
    // carry the same freeform model narration as its assistant content. Memory
    // reads can reintroduce it too. Archive those whole completed batches instead
    // of editing executed arguments. Explicit structured episode fields and
    // controller evidence are retained separately by the caller's fresh handoff.
    let quarantined_notes: Vec<_> = groups
        .iter()
        .map(|group| reason == Reason::Repetition && !group.pending && memory_tool(group))
        .collect();
    let quarantined_inspections = if reason == Reason::Repetition {
        repeated_inspections(&groups)
    } else {
        vec![false; groups.len()]
    };
    let original_tokens = messages
        .iter()
        .map(estimate_tokens)
        .fold(0, usize::saturating_add);
    let archive_usable =
        !archive_source.trim().is_empty() && archive_source.split('@').next() != Some("current");
    if !archive_usable {
        // Never replace evidence with a mutable or missing source. In ordinary
        // use the caller supplies its newly written immutable archive.
        let recent: Vec<_> = messages
            .iter()
            .filter(|m| !matches!(m["role"].as_str(), Some("system" | "developer")))
            .cloned()
            .collect();
        let retained_tokens = recent
            .iter()
            .map(estimate_tokens)
            .fold(0, usize::saturating_add);
        return Compacted {
            recent,
            evidence: Vec::new(),
            stats: json!({
            "archive_reference_usable":false,"original_tokens":original_tokens,
            "retained_tokens":retained_tokens,"evidence_tokens":0,"budget_tokens":budget_tokens,
            "budget_exceeded":retained_tokens > budget_tokens,
            "reason":format!("{reason:?}").to_lowercase()}),
        };
    }
    let mut selected = BTreeMap::<usize, Vec<Message>>::new();
    let mut references = BTreeMap::<usize, Value>::new();
    let mut used = 0usize;
    let mut pending_count = 0usize;
    let mut oversized_pending = Vec::new();
    for (index, group) in groups.iter().enumerate().filter(|(_, group)| group.pending) {
        let tokens = cost(&group.messages);
        used = used.saturating_add(tokens);
        pending_count += 1;
        if tokens > budget_tokens {
            oversized_pending.push(reference(
                archive_source,
                group.messages[0].index,
                "oversized_pending_exchange",
            ));
        }
        selected.insert(index, group.messages.clone());
    }
    // Reserve at most half the available space for old failure/running-handle
    // evidence. This keeps useful recent exchanges from being displaced by an
    // arbitrarily long list of old command observations.
    let evidence_allowance = budget_tokens.saturating_sub(used) / 2;
    let mut reserved = 0usize;
    for (index, group) in groups.iter().enumerate().rev().filter(|(index, group)| {
        !group.pending
            && (group.important || quarantined_notes[*index] || quarantined_inspections[*index])
    }) {
        let mut entry = if quarantined_notes[index] {
            quarantined_memory_reference(archive_source, group)
        } else {
            group_reference(archive_source, group)
        };
        if !quarantined_notes[index] && quarantined_inspections[index] {
            entry["kind"] = json!("quarantined_repeated_inspection");
        }
        let tokens = estimate_tokens(&entry);
        if tokens <= evidence_allowance.saturating_sub(reserved) {
            reserved += tokens;
            used = used.saturating_add(tokens);
            references.insert(index, entry);
        }
    }
    let mut shortened_messages = 0usize;
    for (index, group) in groups
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, group)| !group.pending)
    {
        if quarantined_notes[index] || quarantined_inspections[index] {
            continue;
        }
        let reserved_tokens = references.get(&index).map_or(0, estimate_tokens);
        let available = budget_tokens.saturating_sub(used.saturating_sub(reserved_tokens));
        let original_cost = cost(&group.messages);
        let candidate = if original_cost <= available {
            Some(group.messages.clone())
        } else {
            let shortened = short_group(archive_source, group);
            (cost(&shortened) <= available).then_some(shortened)
        };
        if let Some(candidate) = candidate {
            shortened_messages += candidate
                .iter()
                .zip(&group.messages)
                .filter(|(a, b)| a.value != b.value)
                .count();
            used = used
                .saturating_sub(reserved_tokens)
                .saturating_add(cost(&candidate));
            references.remove(&index);
            selected.insert(index, candidate);
        }
    }
    // Add compact references to skipped batches with whatever space remains.
    // Narration references contain no snippets that could seed the same loop.
    let mut extra = Vec::new();
    for (index, group) in groups.iter().enumerate().rev() {
        if selected.contains_key(&index) || references.contains_key(&index) {
            continue;
        }
        let mut entry = if quarantined_notes[index] {
            quarantined_memory_reference(archive_source, group)
        } else {
            group_reference(archive_source, group)
        };
        if !quarantined_notes[index] && quarantined_inspections[index] {
            entry["kind"] = json!("quarantined_repeated_inspection");
        }
        let tokens = estimate_tokens(&entry);
        if tokens <= budget_tokens.saturating_sub(used) {
            used += tokens;
            references.insert(index, entry);
        }
    }
    for message in &orphans {
        let mut entry = tool_reference(archive_source, message);
        entry["kind"] = json!("unmatched_historical_tool_result");
        let tokens = estimate_tokens(&entry);
        if tokens <= budget_tokens.saturating_sub(used) {
            used += tokens;
            extra.push(entry);
        }
    }
    for &index in &quarantined {
        let entry = reference(archive_source, index, "quarantined_assistant_narration");
        let tokens = estimate_tokens(&entry);
        if tokens <= budget_tokens.saturating_sub(used) {
            used += tokens;
            extra.push(entry);
        }
    }
    let important_unrepresented = groups
        .iter()
        .enumerate()
        .filter(|(index, group)| {
            group.important && !selected.contains_key(index) && !references.contains_key(index)
        })
        .count();
    let retained_count = selected.values().map(Vec::len).sum::<usize>();
    let recent: Vec<_> = selected
        .into_values()
        .flatten()
        .map(|message| message.value)
        .collect();
    let mut evidence: Vec<_> = references.into_values().chain(extra).collect();
    evidence.sort_by_key(|entry| entry["message_index"].as_u64().unwrap_or(0));
    let retained_tokens = recent
        .iter()
        .map(estimate_tokens)
        .fold(0, usize::saturating_add);
    let evidence_tokens = evidence
        .iter()
        .map(estimate_tokens)
        .fold(0, usize::saturating_add);
    Compacted {
        recent,
        evidence,
        stats: json!({
            "reason":format!("{reason:?}").to_lowercase(),"archive_source_id":archive_source,
            "archive_reference_usable":true,"budget_tokens":budget_tokens,
            "original_tokens":original_tokens,"retained_tokens":retained_tokens,
            "evidence_tokens":evidence_tokens,"estimated_tokens":retained_tokens.saturating_add(evidence_tokens),
            "budget_exceeded":retained_tokens.saturating_add(evidence_tokens) > budget_tokens,
            "retained_messages":retained_count,"archived_messages":messages.len().saturating_sub(retained_count),
            "shortened_tool_results":shortened_messages,"quarantined_narration":quarantined.len(),
            "quarantined_progress_note_groups":groups.iter().enumerate().filter(|(index,group)|quarantined_notes[*index] && saved_progress_note(group)).count(),
            "quarantined_memory_tool_groups":quarantined_notes.iter().filter(|value| **value).count(),
            "quarantined_inspection_groups":quarantined_inspections.iter().filter(|value| **value).count(),
            "pending_groups":pending_count,"oversized_protected_group_count":oversized_pending.len(),
            "oversized_protected_groups":oversized_pending,"unrepresented_important_groups":important_unrepresented,
            "unmatched_tool_results":orphans.len()
        }),
    }
}

/// First-stage capacity relief: return the entire original conversation in its
/// original order, replacing only bulky, *older completed* tool payloads. The
/// most recent tool batch, the token-budgeted recent window, and pending batches
/// stay byte-for-byte intact. References are embedded in the replacement tool
/// content, so `evidence` is empty and can safely be omitted by the caller.
///
/// `recent_budget_tokens` sizes the protected window, not the returned transcript.
/// The caller must measure that transcript against its provider context budget.
pub fn clear_old_tool_results(
    messages: &[Value],
    archive_source: &str,
    recent_budget_tokens: usize,
) -> Compacted {
    let archive_usable =
        !archive_source.trim().is_empty() && archive_source.split('@').next() != Some("current");
    let indexed: Vec<_> = messages
        .iter()
        .enumerate()
        .map(|(index, value)| Message {
            index,
            value: value.clone(),
        })
        .collect();
    let (groups, _) = groups(indexed);
    let latest_tool_group = groups.iter().rposition(|group| {
        group
            .messages
            .iter()
            .any(|message| message.value["role"] == "tool")
    });
    let mut protected = vec![false; groups.len()];
    let mut protected_tokens = 0usize;
    for (index, group) in groups.iter().enumerate().rev() {
        let in_window = protected_tokens < recent_budget_tokens;
        if in_window || group.pending || latest_tool_group == Some(index) {
            protected[index] = true;
        }
        if in_window {
            protected_tokens = protected_tokens.saturating_add(cost(&group.messages));
        }
    }
    let mut recent = messages.to_vec();
    let mut shortened_messages = 0usize;
    if archive_usable {
        for (_, group) in groups
            .iter()
            .enumerate()
            .filter(|(index, _)| !protected[*index])
        {
            let shortened = short_group(archive_source, group);
            for (original, replacement) in group.messages.iter().zip(shortened) {
                // Small observations are useful context and do not justify a
                // rewrite simply because an archive reference is shorter.
                if original.value["role"] == "tool"
                    && estimate_tokens(&original.value) >= 1024
                    && estimate_tokens(&original.value)
                        .saturating_sub(estimate_tokens(&replacement.value))
                        >= 256
                {
                    recent[original.index] = replacement.value;
                    shortened_messages += 1;
                }
            }
        }
    }
    let original_tokens = messages
        .iter()
        .map(estimate_tokens)
        .fold(0, usize::saturating_add);
    let retained_tokens = recent
        .iter()
        .map(estimate_tokens)
        .fold(0, usize::saturating_add);
    Compacted {
        recent,
        evidence: Vec::new(),
        stats: json!({
            "reason":"tool_payloads","archive_source_id":archive_source,
            "archive_reference_usable":archive_usable,"budget_tokens":recent_budget_tokens,
            "protected_recent_tokens":protected_tokens,"original_tokens":original_tokens,
            "retained_tokens":retained_tokens,"evidence_tokens":0,"estimated_tokens":retained_tokens,
            "estimated_tokens_saved":original_tokens.saturating_sub(retained_tokens),
            "budget_exceeded":retained_tokens > recent_budget_tokens,
            "retained_messages":messages.len(),"archived_messages":0,
            "shortened_tool_results":shortened_messages,"quarantined_narration":0,
            "pending_groups":groups.iter().filter(|group| group.pending).count(),
            "oversized_protected_group_count":groups.iter().filter(|group| group.pending && cost(&group.messages) > recent_budget_tokens).count(),
            "oversized_protected_groups":groups.iter().filter(|group| group.pending && cost(&group.messages) > recent_budget_tokens).map(|group| reference(archive_source,group.messages[0].index,"oversized_pending_exchange")).collect::<Vec<_>>(),
            "unrepresented_important_groups":0
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const SOURCE: &str = "cycle-000001/conversation-before-refresh-0.json";

    fn call(id: &str) -> Value {
        json!({"role":"assistant","content":"","tool_calls":[{"id":id,"type":"function","function":{"name":"read_file","arguments":{"path":"src/main.rs"}}}]})
    }

    fn reply(id: &str, result: Value) -> Value {
        json!({"role":"tool","tool_call_id":id,"tool_name":"read_file","content":result.to_string()})
    }

    fn paired(messages: &[Value]) {
        let mut pending = Vec::<Value>::new();
        for message in messages {
            if message["role"] == "assistant" {
                assert!(pending.is_empty());
                pending.extend(calls(message).iter().map(|call| call["id"].clone()));
            } else if message["role"] == "tool" {
                let index = pending
                    .iter()
                    .position(|id| *id == message["tool_call_id"])
                    .expect("matching call");
                pending.remove(index);
            }
        }
        assert!(pending.is_empty());
    }

    #[test]
    fn small_history_is_kept_exactly_and_source_is_immutable() {
        let input = vec![
            json!({"role":"system","content":"prompt"}),
            json!({"role":"user","content":"task"}),
            call("one"),
            reply("one", json!({"output":"a file"})),
        ];
        let original = input.clone();
        let output = compact(&input, SOURCE, Reason::Capacity, 10_000);
        assert_eq!(output.recent, input[1..]);
        assert!(output.evidence.is_empty());
        assert_eq!(input, original);
        paired(&output.recent);
    }

    #[test]
    fn huge_old_result_fits_budget_with_exact_reference_and_pairing() {
        let input = vec![
            call("large"),
            reply(
                "large",
                json!({"ok":true,"output":"界".repeat(30_000),"log_id":"command-1.log"}),
            ),
            call("recent"),
            reply("recent", json!({"output":"current result"})),
        ];
        let output = compact(&input, SOURCE, Reason::Capacity, 1800);
        assert!(output.stats["estimated_tokens"].as_u64().unwrap() <= 1800);
        assert_eq!(output.recent[output.recent.len() - 1], input[3]);
        paired(&output.recent);
        let old = output
            .recent
            .iter()
            .find(|message| message["tool_call_id"] == "large")
            .unwrap();
        let reference: Value = serde_json::from_str(old["content"].as_str().unwrap()).unwrap();
        assert_eq!(reference["source_id"], SOURCE);
        assert_eq!(reference["message_index"], 1);
        assert_eq!(reference["observed_metadata"]["ok"], true);
        assert_eq!(reference["observed_metadata"]["log_id"], "command-1.log");
    }

    #[test]
    fn multi_call_batches_are_atomic_and_unmatched_results_are_archived() {
        let mut batch = call("a");
        batch["tool_calls"]
            .as_array_mut()
            .unwrap()
            .push(call("b")["tool_calls"][0].clone());
        let input = vec![
            batch,
            reply("b", json!({"output":"second"})),
            reply("a", json!({"output":"first"})),
            reply("orphan", json!({"output":"unmatched"})),
            json!({"role":"user","content":"next"}),
        ];
        let output = compact(&input, SOURCE, Reason::Recovery, 1200);
        paired(&output.recent);
        assert!(
            !output
                .recent
                .iter()
                .any(|message| message["tool_call_id"] == "orphan")
        );
        assert_eq!(output.stats["unmatched_tool_results"], 1);
        assert!(
            output
                .evidence
                .iter()
                .any(|entry| entry["message_index"] == 3)
        );
    }

    #[test]
    fn giant_completed_arguments_are_archived_but_pending_calls_are_protected() {
        let mut giant = call("giant");
        giant["tool_calls"][0]["function"]["arguments"]["text"] = json!("x".repeat(20_000));
        let complete = vec![
            giant.clone(),
            reply("giant", json!({"ok":true})),
            json!({"role":"user","content":"continue"}),
        ];
        let output = compact(&complete, SOURCE, Reason::Capacity, 600);
        assert_eq!(output.stats["budget_exceeded"], false);
        paired(&output.recent);
        assert!(
            output
                .evidence
                .iter()
                .any(|entry| entry["message_index"] == 0)
        );
        let output = compact(&[giant.clone()], SOURCE, Reason::Recovery, 600);
        assert_eq!(output.recent, vec![giant]);
        assert_eq!(output.stats["budget_exceeded"], true);
        assert_eq!(output.stats["oversized_protected_group_count"], 1);
    }

    #[test]
    fn failure_metadata_and_running_handles_survive_bulk_output_compaction() {
        let failure = json!({"ok":false,"error":"permission denied","result":{"passed":false,"exit_code":7,"output":"z".repeat(12_000),"log_id":"failed.log"}});
        let running = json!({"ok":true,"result":{"running":true,"command_id":"7-check","session_id":19,"next_output_offset":123,"output_tail":"z".repeat(12_000)}});
        let input = vec![
            call("failure"),
            reply("failure", failure),
            call("running"),
            reply("running", running),
            json!({"role":"user","content":"continue the work"}),
        ];
        let output = compact(&input, SOURCE, Reason::Recovery, 1600);
        assert_eq!(output.stats["budget_exceeded"], false);
        let find_metadata = |id: &str| -> Value {
            if let Some(message) = output
                .recent
                .iter()
                .find(|message| message["tool_call_id"] == id)
            {
                let content: Value =
                    serde_json::from_str(message["content"].as_str().unwrap()).unwrap();
                return content.get("observed_metadata").cloned().unwrap_or(content);
            }
            output
                .evidence
                .iter()
                .flat_map(|entry| entry["tool_results"].as_array().into_iter().flatten())
                .find(|entry| entry["tool_call_id"] == id)
                .unwrap()["observed_metadata"]
                .clone()
        };
        let failed = find_metadata("failure");
        assert_eq!(failed["ok"], false);
        assert_eq!(failed["error"], "permission denied");
        assert_eq!(failed["result"]["exit_code"], 7);
        assert_eq!(failed["result"]["log_id"], "failed.log");
        let live = find_metadata("running");
        assert_eq!(live["result"]["running"], true);
        assert_eq!(live["result"]["command_id"], "7-check");
        assert_eq!(live["result"]["session_id"], 19);
        paired(&output.recent);
    }

    #[test]
    fn repetition_quarantines_only_repeated_assistant_narration() {
        let cycle = "I will inspect the same detail and reconsider the same approach. ".repeat(12);
        let user = json!({"role":"user","content":cycle});
        let useful = json!({"role":"assistant","content":"The parser accepts the new field; next inspect the error branch."});
        let mut looped = json!({"role":"assistant","content":format!("Useful finding.\n{cycle}")});
        looped["_chuggin_responses"] =
            json!({"model":"x","items":[{"type":"message","content":cycle}]});
        let input = vec![user.clone(), useful.clone(), looped];
        let recovery = compact(&input, SOURCE, Reason::Recovery, 10_000);
        assert_eq!(recovery.recent, input);
        let output = compact(&input, SOURCE, Reason::Repetition, 10_000);
        assert!(output.recent.contains(&user));
        assert!(output.recent.contains(&useful));
        assert_eq!(output.recent.last().unwrap()["content"], "Useful finding.");
        assert!(
            output
                .recent
                .last()
                .unwrap()
                .get("_chuggin_responses")
                .is_none()
        );
        assert_eq!(output.stats["quarantined_narration"], 1);
    }

    #[test]
    fn repetition_quarantines_near_duplicate_prose_but_retains_distinct_findings_and_calls() {
        let base = "The parser still needs a careful inspection of the same branch before I choose the next action. I should consider the existing evidence and determine whether another small adjustment might clarify the behavior of this particular error handling path.";
        let user = json!({"role":"user","content":base});
        let useful = json!({"role":"assistant","content":"A separate finding: the new option is accepted by the command line parser."});
        let mut input = vec![user.clone(), useful.clone()];
        for index in 0..4 {
            let id = index.to_string();
            let mut assistant = call(&id);
            assistant["content"] = json!(format!("{base} Candidate line {index}."));
            input.push(assistant);
            input.push(reply(
                &id,
                json!({"output":format!("source observation {index}")}),
            ));
        }
        let output = compact(&input, SOURCE, Reason::Repetition, 10_000);
        assert!(output.recent.contains(&user));
        assert!(output.recent.contains(&useful));
        for index in (2..input.len()).step_by(2) {
            let id = &input[index]["tool_calls"][0]["id"];
            let assistant = output
                .recent
                .iter()
                .find(|message| &message["tool_calls"][0]["id"] == id)
                .unwrap();
            assert_eq!(assistant["content"], "");
            assert_eq!(assistant["tool_calls"], input[index]["tool_calls"]);
            assert!(output.recent.contains(&input[index + 1]));
        }
        paired(&output.recent);
    }

    #[test]
    fn brief_idle_responses_are_quarantined_after_short_response_detection() {
        let user = json!({"role":"user","content":"Continue the investigation."});
        let useful = json!({"role":"assistant","content":"The newest observation reveals a different error branch."});
        let mut input = vec![user.clone()];
        input.extend((0..16).map(|_| json!({"role":"assistant","content":"Done."})));
        input.push(useful.clone());
        let output = compact(&input, SOURCE, Reason::Repetition, 10_000);
        assert_eq!(output.recent, vec![user, useful]);
        assert_eq!(output.stats["quarantined_narration"], 16);
    }

    #[test]
    fn duplicate_narration_does_not_delete_tool_calls_or_user_text() {
        let text = "I will keep checking this same source before deciding on the next action.";
        let mut with_call = call("one");
        with_call["content"] = json!(text);
        let input = vec![
            json!({"role":"user","content":text}),
            json!({"role":"assistant","content":text}),
            with_call,
            reply("one", json!({"output":"observed"})),
            json!({"role":"assistant","content":text}),
        ];
        let output = compact(&input, SOURCE, Reason::Repetition, 4000);
        assert_eq!(output.recent[0], input[0]);
        assert_eq!(output.recent[1]["tool_calls"], input[2]["tool_calls"]);
        assert_eq!(output.recent[1]["content"], "");
        paired(&output.recent);
    }

    #[test]
    fn repeated_code_and_tool_arguments_are_not_quarantined_as_narrative() {
        let code = format!("```rust\n{}\n```", "let value = 1;\n".repeat(25));
        let mut assistant = call("code");
        assistant["tool_calls"][0]["function"]["arguments"]["new_text"] =
            json!("Keep thinking. ".repeat(50));
        let input = vec![
            json!({"role":"assistant","content":code}),
            json!({"role":"assistant","content":code}),
            json!({"role":"assistant","content":code}),
            assistant,
            reply("code", json!({"output":"observed"})),
        ];
        let output = compact(&input, SOURCE, Reason::Repetition, 10_000);
        assert_eq!(output.recent, input);
        assert_eq!(output.stats["quarantined_narration"], 0);
    }

    #[test]
    fn repetition_recovery_cannot_replay_cached_cloud_or_native_reasoning() {
        let mut cloud = call("cloud");
        cloud["content"] = json!("Inspected the error branch.");
        cloud["_chuggin_reasoning"] = json!({"model":"openrouter/model","content":"Keep reconsidering. ".repeat(30),"details":[{"type":"reasoning.encrypted","data":"opaque"}]});
        cloud["reasoning_content"] = json!("Projected old reasoning.");
        cloud["reasoning_details"] = json!([{"data":"projected reasoning"}]);
        let mut native = call("native");
        native["content"] = json!("The second observation remains useful.");
        native["_chuggin_responses"] = json!({"model":"chatgpt/model","items":[
            {"type":"reasoning","encrypted_content":"opaque repetitive thought"},
            {"type":"function_call","call_id":"native","name":"read_file","arguments":"{}"}
        ]});
        let input = vec![
            cloud,
            reply("cloud", json!({"output":"first observation"})),
            native,
            reply("native", json!({"output":"second observation"})),
        ];
        for reason in [Reason::Capacity, Reason::Recovery] {
            assert_eq!(compact(&input, SOURCE, reason, 10_000).recent, input);
        }
        let output = compact(&input, SOURCE, Reason::Repetition, 10_000);
        assert_eq!(output.recent.len(), input.len());
        for index in [0, 2] {
            assert_eq!(output.recent[index]["content"], input[index]["content"]);
            assert_eq!(
                output.recent[index]["tool_calls"],
                input[index]["tool_calls"]
            );
            for key in [
                "_chuggin_reasoning",
                "_chuggin_responses",
                "reasoning_content",
                "reasoning_details",
            ] {
                assert!(output.recent[index].get(key).is_none());
            }
        }
        assert_eq!(output.recent[1], input[1]);
        assert_eq!(output.recent[3], input[3]);
        assert_eq!(output.stats["quarantined_narration"], 2);
        paired(&output.recent);
    }

    #[test]
    fn native_cache_without_reasoning_retains_useful_protocol_metadata() {
        let mut assistant = call("native");
        assistant["_chuggin_responses"] = json!({"model":"chatgpt/model","items":[
            {"type":"message","phase":"commentary","content":[{"type":"output_text","text":"Inspecting source."}]},
            {"type":"function_call","call_id":"native","name":"read_file","arguments":"{}"}
        ]});
        let input = vec![assistant, reply("native", json!({"output":"observation"}))];
        assert_eq!(
            compact(&input, SOURCE, Reason::Repetition, 10_000).recent,
            input
        );
    }

    #[test]
    fn repetition_archives_completed_note_batches_without_editing_executed_arguments() {
        let mut note = call("note");
        note["tool_calls"][0]["function"]["name"] = json!("save_progress_note");
        note["tool_calls"][0]["function"]["arguments"] = json!({
            "note":"A long model-authored explanation that should not seed recovery.",
            "observations":[{"source_id":SOURCE,"message_index":7}],
            "decisions":["Inspect the error branch next."]
        });
        note["tool_calls"]
            .as_array_mut()
            .unwrap()
            .push(call("read")["tool_calls"][0].clone());
        let input = vec![
            note.clone(),
            reply("note", json!({"ok":true,"result":"Progress note saved."})),
            reply("read", json!({"output":"source observation"})),
            json!({"role":"user","content":"Continue the investigation."}),
        ];
        assert_eq!(
            compact(&input, SOURCE, Reason::Capacity, 10_000).recent,
            input
        );
        let output = compact(&input, SOURCE, Reason::Repetition, 10_000);
        assert_eq!(output.recent, vec![input[3].clone()]);
        assert_eq!(output.stats["quarantined_progress_note_groups"], 1);
        let evidence = output
            .evidence
            .iter()
            .find(|entry| entry["kind"] == "quarantined_progress_note_exchange")
            .unwrap();
        assert_eq!(evidence["source_id"], SOURCE);
        assert_eq!(evidence["message_index"], 0);
        assert_eq!(evidence["tool_results"][0]["message_index"], 1);
        assert_eq!(evidence["tool_results"][1]["message_index"], 2);
        assert_eq!(input[0], note);
        paired(&output.recent);
    }

    #[test]
    fn repetition_keeps_pending_note_save_arguments_and_recorded_replies_exact() {
        let mut pending = call("note");
        pending["tool_calls"][0]["function"]["name"] = json!("save_progress_note");
        pending["tool_calls"][0]["function"]["arguments"] = json!({"note":"Pending model note."});
        pending["tool_calls"]
            .as_array_mut()
            .unwrap()
            .push(call("missing")["tool_calls"][0].clone());
        let input = vec![
            pending,
            reply("note", json!({"ok":true,"result":"Progress note saved."})),
        ];
        let output = compact(&input, SOURCE, Reason::Repetition, 10_000);
        assert_eq!(output.recent, input);
        assert_eq!(output.stats["quarantined_progress_note_groups"], 0);
        assert_eq!(output.stats["pending_groups"], 1);
    }

    #[test]
    fn repetition_archives_memory_reads_that_could_reintroduce_model_narration() {
        for name in [
            "read_progress_note",
            "read_task_evidence",
            "read_context_memory",
        ] {
            let mut request = call("memory");
            request["tool_calls"][0]["function"]["name"] = json!(name);
            let result = reply(
                "memory",
                json!({"ok":true,"result":{"note":"PRIVATE_MODELnarration","error":"PRIVATE_MODELnarration"}}),
            );
            let mut input = vec![request.clone(), result.clone()];
            input.extend((0..16).map(|_| json!({"role":"assistant","content":"Done."})));
            let original = input.clone();
            let output = compact(&input, SOURCE, Reason::Repetition, 10_000);
            assert!(output.recent.is_empty());
            assert_eq!(output.stats["quarantined_memory_tool_groups"], 1);
            let evidence = output
                .evidence
                .iter()
                .find(|entry| entry["kind"] == "quarantined_memory_tool_exchange")
                .unwrap();
            assert_eq!(evidence["source_id"], SOURCE);
            assert_eq!(evidence["message_index"], 0);
            assert_eq!(evidence["tool_results"][0]["message_index"], 1);
            assert!(
                evidence["tool_results"][0]
                    .get("observed_metadata")
                    .is_none()
            );
            assert_eq!(input, original);
            // An unanswered memory read still represents pending execution
            // state, so its original calls/arguments must remain untouched.
            let pending = compact(&[request.clone()], SOURCE, Reason::Repetition, 10_000);
            assert_eq!(pending.recent, vec![request]);
            assert_eq!(pending.stats["quarantined_memory_tool_groups"], 0);
            assert_eq!(
                compact(&input, SOURCE, Reason::Recovery, 10_000).recent,
                input
            );
        }
    }

    #[test]
    fn repetition_archives_identical_inspections_and_retains_new_results_and_pending_calls() {
        let mut input = Vec::new();
        for index in 0..3 {
            let id = format!("repeated-{index}");
            let mut assistant = call(&id);
            if index == 1 {
                assistant["tool_calls"][0]["function"]["arguments"] =
                    json!("{\"path\":\"src/main.rs\"}");
            }
            input.push(assistant);
            input.push(reply(
                &id,
                json!({"ok":true,"result":"unchanged file observation"}),
            ));
        }
        let novel = reply(
            "novel",
            json!({"ok":true,"result":"a newly changed file observation"}),
        );
        input.push(call("novel"));
        input.push(novel.clone());
        let pending = call("pending");
        input.push(pending.clone());
        let original = input.clone();
        for reason in [Reason::Capacity, Reason::Recovery] {
            assert_eq!(compact(&input, SOURCE, reason, 10_000).recent, input);
        }
        let output = compact(&input, SOURCE, Reason::Repetition, 10_000);
        assert_eq!(output.recent, vec![input[6].clone(), novel, pending]);
        assert_eq!(output.stats["quarantined_inspection_groups"], 3);
        let evidence: Vec<_> = output
            .evidence
            .iter()
            .filter(|entry| entry["kind"] == "quarantined_repeated_inspection")
            .collect();
        assert_eq!(evidence.len(), 3);
        for (index, entry) in evidence.iter().enumerate() {
            assert_eq!(entry["source_id"], SOURCE);
            assert_eq!(entry["message_index"], index * 2);
            assert_eq!(entry["tool_results"][0]["message_index"], index * 2 + 1);
        }
        assert_eq!(input, original);
    }

    #[test]
    fn repetition_retains_repeated_finite_commands_and_mixed_batches() {
        let mut input = Vec::new();
        for index in 0..3 {
            let id = format!("command-{index}");
            let mut command = call(&id);
            command["tool_calls"][0]["function"] =
                json!({"name":"run_command","arguments":{"argv":["cargo","test"]}});
            input.push(command.clone());
            input.push(reply(
                &id,
                json!({"ok":true,"result":{"exit_code":0,"output":"same command output"}}),
            ));
            let mut mixed = call(&format!("read-{index}"));
            mixed["tool_calls"]
                .as_array_mut()
                .unwrap()
                .push(command["tool_calls"][0].clone());
            input.push(mixed);
            input.push(reply(
                &format!("read-{index}"),
                json!({"output":"same inspection output"}),
            ));
            input.push(reply(
                &id,
                json!({"ok":true,"result":{"exit_code":0,"output":"same command output"}}),
            ));
        }
        let output = compact(&input, SOURCE, Reason::Repetition, 20_000);
        assert_eq!(output.recent, input);
        assert_eq!(output.stats["quarantined_inspection_groups"], 0);
        paired(&output.recent);
    }

    #[test]
    fn equal_inspection_text_with_different_image_artifacts_is_new_evidence() {
        let mut input = Vec::new();
        for index in 0..3 {
            let id = index.to_string();
            input.push(call(&id));
            let mut result = reply(&id, json!({"output":"same textual image label"}));
            result["_chuggin_images"] =
                json!([{"path":format!("/cache/image-{index}.png"),"width":32,"height":32}]);
            input.push(result);
        }
        let output = compact(&input, SOURCE, Reason::Repetition, 10_000);
        assert_eq!(output.recent, input);
        assert_eq!(output.stats["quarantined_inspection_groups"], 0);
    }

    #[test]
    fn unicode_and_images_are_not_estimated_as_tiny_text() {
        let ascii = estimate_tokens(&json!("a".repeat(1000)));
        let unicode = estimate_tokens(&json!("界".repeat(1000)));
        assert!(unicode >= 3000);
        assert!(unicode > ascii * 3);
        let image = json!({"role":"tool","content":"snapshot","_chuggin_images":[{"path":"/cache/image.png","width":8192,"height":8192}]});
        assert!(estimate_tokens(&image) > 200_000);
        let input = vec![
            call("image"),
            {
                let mut image = image;
                image["tool_call_id"] = json!("image");
                image
            },
            json!({"role":"user","content":"continue"}),
        ];
        let output = compact(&input, SOURCE, Reason::Capacity, 1500);
        assert_eq!(output.stats["budget_exceeded"], false);
        assert!(
            output
                .recent
                .iter()
                .all(|message| message.get("_chuggin_images").is_none())
        );
    }

    #[test]
    fn mutable_current_is_never_used_to_replace_evidence() {
        let input = vec![
            call("one"),
            reply("one", json!({"output":"x".repeat(10_000)})),
        ];
        let output = compact(&input, "current", Reason::Capacity, 200);
        assert_eq!(output.recent, input);
        assert_eq!(output.stats["archive_reference_usable"], false);
    }

    #[test]
    fn first_stage_changes_only_old_bulky_results_and_preserves_full_order() {
        let mut old = reply(
            "old",
            json!({"ok":false,"error":"permission denied","result":{"exit_code":3,"log_id":"old.log","output":"x".repeat(20_000)}}),
        );
        old["_chuggin_responses"] =
            json!({"items":[{"type":"function_call_output","output":"must not replay"}]});
        let input = vec![
            json!({"role":"system","content":"prompt"}),
            json!({"role":"user","content":"preserve all user text"}),
            call("old"),
            old,
            json!({"role":"assistant","content":"A useful observation."}),
            call("new"),
            reply("new", json!({"output":"y".repeat(20_000)})),
            call("pending"),
        ];
        let original = input.clone();
        let output = clear_old_tool_results(&input, SOURCE, 300);
        assert_eq!(output.recent.len(), input.len());
        for (index, message) in input.iter().enumerate() {
            if index != 3 {
                assert_eq!(&output.recent[index], message);
            }
        }
        assert!(output.recent[3].get("_chuggin_responses").is_none());
        let archived: Value =
            serde_json::from_str(output.recent[3]["content"].as_str().unwrap()).unwrap();
        assert_eq!(archived["source_id"], SOURCE);
        assert_eq!(archived["message_index"], 3);
        assert_eq!(archived["observed_metadata"]["error"], "permission denied");
        assert_eq!(archived["observed_metadata"]["result"]["exit_code"], 3);
        assert_eq!(output.stats["shortened_tool_results"], 1);
        assert!(output.stats["estimated_tokens_saved"].as_u64().unwrap() > 5000);
        assert!(output.evidence.is_empty());
        assert_eq!(input, original);
    }

    #[test]
    fn first_stage_keeps_small_results_and_all_pending_replies_exact() {
        let mut pending = call("a");
        pending["tool_calls"]
            .as_array_mut()
            .unwrap()
            .push(call("b")["tool_calls"][0].clone());
        let input = vec![
            call("small"),
            reply("small", json!({"output":"a short result"})),
            pending,
            reply("a", json!({"output":"x".repeat(20_000)})),
        ];
        let output = clear_old_tool_results(&input, SOURCE, 0);
        assert_eq!(output.recent, input);
        assert_eq!(output.stats["pending_groups"], 1);
    }

    #[test]
    fn arbitrary_completed_histories_never_exceed_the_budget() {
        for budget in [0, 50, 200, 512, 1200, 4000] {
            let mut input = Vec::new();
            for index in 0..15 {
                let id = index.to_string();
                input.push(call(&id));
                input.push(reply(
                    &id,
                    json!({"ok":true,"output":"界".repeat(index * 317),"command_id":id}),
                ));
            }
            let output = compact(&input, SOURCE, Reason::Capacity, budget);
            let actual = output
                .recent
                .iter()
                .chain(&output.evidence)
                .map(estimate_tokens)
                .sum::<usize>();
            assert!(actual <= budget, "{actual} > {budget}");
            assert_eq!(output.stats["estimated_tokens"], actual);
            paired(&output.recent);
        }
    }
}
