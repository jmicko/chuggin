//! Source-linked memory for already delivered human directions and explicit
//! model-authored episode notes. This module does no I/O or inference. In
//! particular, it never opens a mailbox to discover human instructions.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const PAGE_BYTES: usize = 6_000;
#[cfg(test)]
const HUMAN_CONTEXT_TOKENS: usize = 6_000;
#[cfg(test)]
const EPISODE_CONTEXT_TOKENS: usize = 2_000;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Memory {
    pub human_directions: Vec<HumanDirection>,
    pub episodes: Vec<Episode>,
    backfill_complete: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HumanDirection {
    pub id: String,
    /// Only this exact authored text is a human direction. Other fields in the
    /// original operator/mailbox record can be harness or model metadata.
    pub text: String,
    pub kind: String,
    pub sources: Vec<Value>,
    pub original: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Episode {
    pub id: String,
    pub sources: Vec<Value>,
    /// The complete submitted arguments, including any freeform note. The note
    /// is available by retrieval but is deliberately absent from live previews.
    pub original: Value,
    pub observations: Vec<Value>,
    pub decisions: Vec<Value>,
    pub reasons: Vec<Value>,
    pub open_questions: Vec<Value>,
    pub attempts: Vec<Value>,
}

struct PendingCall {
    name: String,
    id: Option<Value>,
    args: Value,
    message_index: usize,
    call_index: usize,
}

impl Memory {
    pub fn is_empty(&self) -> bool {
        self.human_directions.is_empty() && self.episodes.is_empty()
    }

    /// Old conversations need one explicit scan of their immutable archives.
    /// Merely ingesting current messages cannot establish that scan is complete.
    pub fn needs_backfill(&self) -> bool {
        !self.backfill_complete
    }

    pub fn mark_backfilled(&mut self) {
        self.backfill_complete = true;
    }

    /// Ingest only recognizable delivered records. A `user` role alone is not
    /// evidence of human authorship: the runner uses it for generated notices.
    /// Tool outputs require their actual preceding assistant tool call, so a
    /// file, web result, or quoted transcript cannot become a human direction.
    pub fn ingest_messages(&mut self, messages: &[Value], archive_source: &str) {
        let mut pending = Vec::<PendingCall>::new();
        for (message_index, message) in messages.iter().enumerate() {
            match message["role"].as_str() {
                Some("user") => {
                    let Some(content) = decoded(&message["content"]) else {
                        continue;
                    };
                    let Some(notes) = content["operator_update"]["notes"].as_array() else {
                        continue;
                    };
                    for (note_index, note) in notes.iter().enumerate() {
                        let Some(revision) = note["revision"].as_u64().filter(|n| *n > 0) else {
                            continue;
                        };
                        // `summary`, `instruction`, and changed-path notices
                        // describe the harness/operator's actions. Only these
                        // explicitly authored fields carry human directions.
                        for field in ["note", "goal"] {
                            let Some(text) = note[field].as_str() else {
                                continue;
                            };
                            let source = json!({
                                "source_id":archive_source,"message_index":message_index,
                                "operator_revision":revision,"note_index":note_index,"field":field
                            });
                            self.add_human(
                                &format!("operator:{revision}:{field}"),
                                &format!("operator_{field}"),
                                text,
                                source,
                                note,
                            );
                        }
                    }
                }
                Some("assistant") => {
                    let Some(calls) = message["tool_calls"].as_array() else {
                        continue;
                    };
                    for (call_index, call) in calls.iter().enumerate() {
                        let Some(name) = call["function"]["name"].as_str() else {
                            continue;
                        };
                        if !matches!(name, "read_messages" | "save_progress_note") {
                            continue;
                        }
                        pending.push(PendingCall {
                            name: name.into(),
                            id: call.get("id").filter(|id| !id.is_null()).cloned(),
                            args: decoded(&call["function"]["arguments"]).unwrap_or(Value::Null),
                            message_index,
                            call_index,
                        });
                    }
                }
                Some("tool") => {
                    let result_id = message.get("tool_call_id").filter(|id| !id.is_null());
                    let result_name = message["tool_name"].as_str();
                    let Some(position) = pending.iter().position(|call| {
                        result_name.is_none_or(|name| name == call.name)
                            && match result_id {
                                Some(id) => call.id.as_ref() == Some(id),
                                None => call.id.is_none() && result_name == Some(&call.name),
                            }
                    }) else {
                        continue;
                    };
                    let call = pending.remove(position);
                    let Some(envelope) = decoded(&message["content"]) else {
                        continue;
                    };
                    if envelope["ok"] != true {
                        continue;
                    }
                    let source = json!({
                        "source_id":archive_source,"message_index":message_index,
                        "request_message_index":call.message_index,"call_index":call.call_index,
                        "tool_call_id":call.id,"tool_name":call.name
                    });
                    if call.name == "save_progress_note" {
                        // Invalid old records remain in their original archive;
                        // they cannot abort a handoff or manufacture memory.
                        let _ = self.record_episode(&call.args, source);
                        continue;
                    }
                    let Some(result) = decoded(&envelope["result"]) else {
                        continue;
                    };
                    if result["receipt"] != "fetched" {
                        continue;
                    }
                    let Some(mail) = result["messages"].as_array() else {
                        continue;
                    };
                    for original in mail {
                        if original["sender"] != "human" || original["context_only"] == true {
                            continue;
                        }
                        let (Some(id), Some(text)) = (
                            original["id"].as_u64().filter(|id| *id > 0),
                            original["body"].as_str(),
                        ) else {
                            continue;
                        };
                        let mut mail_source = source.clone();
                        mail_source["mailbox_message_id"] = json!(id);
                        mail_source["reply_to"] = original["reply_to"].clone();
                        mail_source["thread_id"] = original["thread_id"].clone();
                        self.add_human(
                            &format!("mailbox:{id}"),
                            "mailbox",
                            text,
                            mail_source,
                            original,
                        );
                    }
                }
                _ => {}
            }
        }
    }

    fn add_human(
        &mut self,
        identity: &str,
        kind: &str,
        text: &str,
        source: Value,
        original: &Value,
    ) {
        // Same source identity with conflicting text is retained as a distinct
        // record; it never silently replaces or supersedes the earlier text.
        let id = fingerprint("human", &json!([identity, text]));
        if let Some(record) = self.human_directions.iter_mut().find(|r| r.id == id) {
            add_source(&mut record.sources, source);
        } else {
            self.human_directions.push(HumanDirection {
                id,
                text: text.into(),
                kind: kind.into(),
                sources: vec![source],
                original: original.clone(),
            });
        }
    }

    /// Retain exact explicit structured notes, with no extraction of claims
    /// from prose and no assignment of a goal, task, or human authority. An item
    /// may be a string or an object with evidence refs/reasons/outcomes. Its
    /// contents remain model-authored and unverified even if it claims otherwise.
    /// `discoveries` is accepted as an alias/addition to `observations`.
    pub fn record_episode(&mut self, args: &Value, source: Value) -> Result<()> {
        ensure!(args.is_object(), "Episode arguments must be an object");
        let mut observations = items(args, "observations")?;
        observations.extend(items(args, "discoveries")?);
        let decisions = items(args, "decisions")?;
        let reasons = items(args, "reasons")?;
        let open_questions = items(args, "open_questions")?;
        let attempts = items(args, "attempts")?;
        if observations.is_empty()
            && decisions.is_empty()
            && reasons.is_empty()
            && open_questions.is_empty()
            && attempts.is_empty()
        {
            return Ok(());
        }
        let id = fingerprint("episode", args);
        if let Some(episode) = self.episodes.iter_mut().find(|r| r.id == id) {
            add_source(&mut episode.sources, source);
            return Ok(());
        }
        self.episodes.push(Episode {
            id,
            sources: vec![source],
            original: args.clone(),
            observations,
            decisions,
            reasons,
            open_questions,
            attempts,
        });
        Ok(())
    }

    #[cfg(test)]
    pub fn protected_context(&self) -> Value {
        self.protected_context_budget(HUMAN_CONTEXT_TOKENS)
    }

    #[cfg(test)]
    pub fn episode_context(&self) -> Value {
        self.episode_context_budget(EPISODE_CONTEXT_TOKENS)
    }

    /// Bounded preview only. Every full record remains serialized in `Memory`
    /// and is available through `page`; omission never means supersession.
    pub fn protected_context_budget(&self, budget_tokens: usize) -> Value {
        let latest_goal = self.latest_goal_revision();
        let records: Vec<_> = self
            .human_directions
            .iter()
            .rev()
            .map(|r| {
                json!({"id":r.id,"text":r.text,"kind":r.kind,
                    "source":memory_source(&r.sources, &r.id, "human"),"source_count":r.sources.len(),
                    "scope":self.human_scope(r,latest_goal)})
            })
            .collect();
        bounded_context("human", records, budget_tokens)
    }

    pub fn episode_context_budget(&self, budget_tokens: usize) -> Value {
        let records: Vec<_> = self
            .episodes
            .iter()
            .rev()
            .map(|r| {
                json!({"id":r.id,"authorship":"model","verification":"unverified",
                    "observations":r.observations,"decisions":r.decisions,"reasons":r.reasons,
                    "open_questions":r.open_questions,"attempts":r.attempts,
                    "source":memory_source(&r.sources, &r.id, "episode"),"source_count":r.sources.len()})
            })
            .collect();
        bounded_context("episode", records, budget_tokens)
    }

    #[cfg(test)]
    pub fn page(&self, section: &str, offset: usize) -> Result<Value> {
        self.page_versioned(section, offset, None)
    }

    #[cfg(test)]
    pub fn page_versioned(
        &self,
        section: &str,
        offset: usize,
        expected_revision: Option<&str>,
    ) -> Result<Value> {
        self.page_selected(section, offset, expected_revision, None)
    }

    /// UTF-8 byte pages of a versioned JSON snapshot. Follow `next_offset` with
    /// the returned `revision`; a changed memory rejects old cursors instead of
    /// silently splicing pages from different snapshots. Exact authored text
    /// and original records remain intact, with derived scope annotations added.
    pub fn page_selected(
        &self,
        section: &str,
        offset: usize,
        expected_revision: Option<&str>,
        record_id: Option<&str>,
    ) -> Result<Value> {
        let text = match section {
            "human" => {
                // Scope uses the full ledger, including goals outside a selected
                // record. Selection must not make a replaced goal current again.
                let latest_goal = self.latest_goal_revision();
                let records: Vec<_> = self
                    .human_directions
                    .iter()
                    .filter(|record| record_id.is_none_or(|id| record.id == id))
                    .map(|record| {
                        let mut value = serde_json::to_value(record)?;
                        value["scope"] = self.human_scope(record, latest_goal);
                        Ok(value)
                    })
                    .collect::<Result<_>>()?;
                ensure!(
                    record_id.is_none() || !records.is_empty(),
                    "Unknown human memory record_id: {}",
                    record_id.unwrap_or_default()
                );
                serde_json::to_string(&records)?
            }
            "episode" => {
                let records: Vec<_> = self
                    .episodes
                    .iter()
                    .filter(|record| record_id.is_none_or(|id| record.id == id))
                    .collect();
                ensure!(
                    record_id.is_none() || !records.is_empty(),
                    "Unknown episode memory record_id: {}",
                    record_id.unwrap_or_default()
                );
                serde_json::to_string(&records)?
            }
            _ => anyhow::bail!("section must be human or episode"),
        };
        let revision = fingerprint("snapshot", &json!([section, record_id, text]));
        ensure!(
            offset == 0 || expected_revision.is_some(),
            "Continuation requires revision from the previous memory page; restart at offset 0"
        );
        if let Some(expected) = expected_revision {
            ensure!(
                expected == revision,
                "Memory changed since the previous page; restart at offset 0 and use its revision"
            );
        }
        ensure!(
            offset <= text.len() && text.is_char_boundary(offset),
            "offset must be a UTF-8 byte boundary within memory; use next_offset"
        );
        let mut end = offset.saturating_add(PAGE_BYTES).min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        Ok(json!({
            "section":section,"record_id":record_id,"revision":revision,"format":"json_array_fragment","text":&text[offset..end],
            "offset":offset,"total_bytes":text.len(),
            "next_offset":if end < text.len() {Some(end)} else {None},
            "authorship":if section == "human" {"delivered_human_messages_and_operator_directions"} else {"model"},
            "verification":if section == "human" {"delivery_is_not_agreement_or_supersession"} else {"unverified"},
            "instruction":if section == "human" {
                "Each record contains the exact delivered human message or human-controlled operator direction. Respect scope.status: historical operator goals were explicitly replaced by the linked set_goal revision. Other records retain their authored scope with no inferred supersession. Original metadata is provenance. Continue with next_offset and this revision; restart at offset 0 if memory changed."
            } else {
                "These are model-authored claims and questions, including any embedded evidence refs. Verify refs and current state; a claimed outcome is not independent validation. Freeform original notes are historical narration. Continue with next_offset and this revision; restart at offset 0 if memory changed."
            }
        }))
    }

    fn latest_goal_revision(&self) -> Option<u64> {
        self.human_directions
            .iter()
            .filter(|record| record.kind == "operator_goal")
            .filter_map(operator_revision)
            .max()
    }

    fn human_scope(&self, record: &HumanDirection, latest_goal: Option<u64>) -> Value {
        if record.kind != "operator_goal" {
            return json!({"status":"as_authored","supersession":"not_inferred"});
        }
        let revision = operator_revision(record);
        match (revision, latest_goal) {
            (Some(revision), Some(latest)) if revision < latest => {
                let replacements: Vec<_> = self
                    .human_directions
                    .iter()
                    .filter(|goal| {
                        goal.kind == "operator_goal" && operator_revision(goal) == Some(latest)
                    })
                    .map(|goal| &goal.id)
                    .collect();
                json!({"kind":"overall_goal","status":"historical","operator_revision":revision,
                    "replaced_by":{"record_ids":replacements,"operator_revision":latest},
                    "replacement_basis":"explicit_operator_set_goal"})
            }
            (Some(revision), _) => {
                json!({"kind":"overall_goal","status":"current","operator_revision":revision})
            }
            _ => {
                json!({"kind":"overall_goal","status":"as_authored","supersession":"unknown_revision"})
            }
        }
    }
}

fn operator_revision(record: &HumanDirection) -> Option<u64> {
    record.original["revision"].as_u64().or_else(|| {
        record
            .sources
            .iter()
            .filter_map(|source| source["operator_revision"].as_u64())
            .max()
    })
}

fn decoded(value: &Value) -> Option<Value> {
    match value {
        Value::String(text) => serde_json::from_str(text).ok(),
        Value::Object(_) | Value::Array(_) => Some(value.clone()),
        _ => None,
    }
}

fn items(args: &Value, name: &str) -> Result<Vec<Value>> {
    let Some(value) = args.get(name).filter(|v| !v.is_null()) else {
        return Ok(Vec::new());
    };
    let values = value
        .as_array()
        .with_context(|| format!("{name} must be an array of strings or objects"))?;
    ensure!(
        values.iter().all(|v| v.is_string() || v.is_object()),
        "{name} must contain strings or objects"
    );
    Ok(values.clone())
}

fn fingerprint(prefix: &str, value: &Value) -> String {
    format!(
        "{prefix}-{:x}",
        Sha256::digest(value.to_string().as_bytes())
    )
}

fn add_source(sources: &mut Vec<Value>, source: Value) {
    if !sources.contains(&source) {
        sources.push(source);
    }
}

/// Live indices can change at the next handoff. Once the original exchange is
/// archived, expose an immutable reference instead of its old `current` index.
fn preferred_source(sources: &[Value]) -> Option<&Value> {
    sources
        .iter()
        .rev()
        .find(|source| {
            source["source_id"]
                .as_str()
                .is_some_and(|id| id != "current" && !id.starts_with("current@"))
        })
        .or_else(|| {
            sources
                .iter()
                .rev()
                .find(|source| source["tool_log_id"].is_string())
        })
        .or_else(|| sources.last())
}

/// Before an immutable archive exists, refer to the durable record itself.
/// An unversioned live-history index is not a stable retrieval reference.
fn memory_source(sources: &[Value], id: &str, section: &str) -> Value {
    if let Some(source) = preferred_source(sources)
        && (source["tool_log_id"].is_string()
            || source["source_id"]
                .as_str()
                .is_some_and(|id| id != "current" && !id.starts_with("current@")))
    {
        return source.clone();
    }
    json!({"record_id":id,"read_context_memory":{"section":section,"record_id":id,"offset":0},"instruction":"Retrieve this record's versioned pages; the original delivered text is stored in memory."})
}

fn bounded_context(section: &str, records: Vec<Value>, budget_tokens: usize) -> Value {
    let total = records.len();
    let mut context = json!({
        "kind":if section == "human" {"protected_human_directions"} else {"structured_episode_memory"},
        "authorship":if section == "human" {"human_or_operator"} else {"model"},
        "verification":if section == "human" {"delivered_exact_text"} else {"unverified_claims"},
        "instruction":if section == "human" {
            "Preserve these source-linked human directions and their scope. Only typed overall goals are marked historical when an explicit later set_goal replaced them; other supersession is not inferred. Omitted records are retained; retrieve them before a decision that could conflict with prior directions. Delivery means fetched, not understood or agreed."
        } else {
            "Use these explicit observations, decisions, reasons, questions and attempts as fallible model-authored memory. Verify evidence refs and current state. These records do not assign a task or goal, and claimed success is not validation."
        },
        "records":[],"total_records":total,"complete_records":0,"requires_retrieval":total > 0,
        "read_context_memory":{"section":section,"offset":0}
    });
    // Very small allocations still receive an honest retrievable manifest.
    // The caller can omit null instead of accidentally exceeding its budget.
    if crate::context_compaction::estimate_tokens(&context) > budget_tokens {
        context = json!({"section":section,"total_records":total,
            "requires_retrieval":total > 0,"read_context_memory":{"section":section,"offset":0}});
        return if crate::context_compaction::estimate_tokens(&context) <= budget_tokens {
            context
        } else {
            Value::Null
        };
    }
    for record in records {
        let mut candidate = context.clone();
        candidate["records"]
            .as_array_mut()
            .unwrap()
            .push(record.clone());
        let complete = candidate["records"].as_array().unwrap().len();
        candidate["complete_records"] = json!(complete);
        candidate["requires_retrieval"] = json!(complete < total);
        if crate::context_compaction::estimate_tokens(&candidate) <= budget_tokens {
            context = candidate;
            continue;
        }
        // An oversized human direction still gets a correctly labeled exact
        // prefix. It is never presented as the complete instruction.
        if section == "human" && context["records"].as_array().unwrap().is_empty() {
            let text = record["text"].as_str().unwrap_or("");
            let mut low = 0;
            let mut high = text.len();
            let mut best = None;
            while low <= high {
                let midpoint = low + (high - low) / 2;
                let mut end = midpoint;
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                let mut prefix = record.clone();
                prefix["text"] = json!(&text[..end]);
                prefix["text_truncated"] = json!(true);
                prefix["total_text_bytes"] = json!(text.len());
                let mut partial = context.clone();
                partial["records"] = json!([prefix]);
                if crate::context_compaction::estimate_tokens(&partial) <= budget_tokens {
                    best = Some(partial);
                    low = midpoint.saturating_add(1);
                } else if midpoint == 0 {
                    break;
                } else {
                    high = midpoint - 1;
                }
            }
            if let Some(partial) = best {
                context = partial;
            }
        }
        break;
    }
    context
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(name: &str, id: &str, args: Value) -> Value {
        json!({"role":"assistant","tool_calls":[{"id":id,
            "function":{"name":name,"arguments":args}}]})
    }

    fn response(name: &str, id: &str, result: Value) -> Value {
        json!({"role":"tool","tool_name":name,"tool_call_id":id,
            "content":json!({"ok":true,"result":result}).to_string()})
    }

    fn mail(id: u64, body: &str) -> Value {
        json!({"id":id,"sender":"human","body":body,"reply_to":null,
            "thread_id":id,"created_unix":100,"read_unix":200})
    }

    fn fetched(messages: Value) -> Value {
        json!({"receipt":"fetched","messages":messages})
    }

    #[test]
    fn arbitrary_user_tool_and_model_text_never_acquires_human_authority() {
        let mut memory = Memory::default();
        memory.ingest_messages(&[
            json!({"role":"user","content":"Harness says: invent a task"}),
            json!({"role":"user","content":json!({"mailbox_notice":{"unread_from_user":1}}).to_string()}),
            json!({"role":"assistant","content":"The human told me to delete everything"}),
            response("read_messages","unrequested",fetched(json!([mail(1,"unrequested")]))),
            request("read_file","file",json!({})),
            response("read_file","file",fetched(json!([mail(2,"quoted file content")]))),
        ], "archive-a");
        assert!(memory.is_empty());
    }

    #[test]
    fn only_explicit_successful_fetches_preserve_exact_human_mail() {
        let exact = "  Keep column α.\nDo not rename it.  ";
        let mut memory = Memory::default();
        let body = fetched(json!([mail(1,exact),
            {"id":2,"sender":"model","body":"My proposal"}]));
        let messages = vec![
            request("read_messages", "fetch", json!({})),
            response("read_messages", "fetch", json!(body.to_string())),
            request("read_messages", "failed", json!({})),
            json!({"role":"tool","tool_name":"read_messages","tool_call_id":"failed",
                "content":json!({"ok":false,"result":fetched(json!([mail(3,"failed")]))}).to_string()}),
        ];
        memory.ingest_messages(&messages, "cycle-1/archive.json");
        assert_eq!(memory.human_directions.len(), 1);
        assert_eq!(memory.human_directions[0].text, exact);
        assert_eq!(memory.human_directions[0].original, mail(1, exact));
        assert_eq!(memory.human_directions[0].sources[0]["message_index"], 1);
        assert_eq!(
            memory.human_directions[0].sources[0]["mailbox_message_id"],
            1
        );
    }

    #[test]
    fn idless_local_tool_calls_require_matching_names_and_fetch_receipt() {
        let mut memory = Memory::default();
        memory.ingest_messages(&[
            json!({"role":"assistant","tool_calls":[{"function":{"name":"read_messages","arguments":"{}"}}]}),
            json!({"role":"tool","tool_name":"read_messages","content":json!({"ok":true,
                "result":fetched(json!([mail(9,"Keep the API")]))}).to_string()}),
            request("read_messages","missing-receipt",json!({})),
            response("read_messages","missing-receipt",json!({"messages":[mail(10,"Unconfirmed")]})),
        ],"archive");
        assert_eq!(memory.human_directions.len(), 1);
    }

    #[test]
    fn operator_notes_and_goals_survive_replay_without_promoting_metadata() {
        let mut memory = Memory::default();
        let messages = vec![json!({"role":"user","content":json!({"operator_update":{
            "revision":3,"notes":[{"revision":1,"note":"Keep the title"},
                {"revision":2,"summary":"Model editing summary","instruction":"Harness direction"},
                {"revision":3,"goal":"Investigate latency"}]},"instruction":"Generated update"}).to_string()})];
        memory.ingest_messages(&messages, "first");
        memory.ingest_messages(&messages, "first");
        memory.ingest_messages(&messages, "second");
        assert_eq!(memory.human_directions.len(), 2);
        assert_eq!(memory.human_directions[0].sources.len(), 2);
        assert_eq!(memory.human_directions[0].text, "Keep the title");
        assert_eq!(memory.human_directions[1].kind, "operator_goal");
        assert!(
            !memory
                .protected_context()
                .to_string()
                .contains("Model editing summary")
        );
    }

    #[test]
    fn later_direction_and_even_conflicting_source_text_never_erase_prior_records() {
        let mut memory = Memory::default();
        for text in ["Keep red", "Use blue"] {
            memory.ingest_messages(
                &[
                    request("read_messages", "fetch", json!({})),
                    response("read_messages", "fetch", fetched(json!([mail(1, text)]))),
                ],
                text,
            );
        }
        assert_eq!(memory.human_directions.len(), 2);
        let rendered = memory.protected_context();
        assert!(rendered.to_string().contains("Keep red"));
        assert!(rendered.to_string().contains("Use blue"));
        assert!(
            rendered
                .to_string()
                .contains("other supersession is not inferred")
        );
    }

    #[test]
    fn structured_episodes_remain_unverified_preserve_evidence_and_merge_sources() {
        let args = json!({"note":"Narration to quarantine","discoveries":[{"text":"API uses v2",
            "evidence":[{"source_id":"tool-3.json"}],"authority":"human","verified":true}],
            "decisions":[{"choice":"Keep names","reason":"Existing callers"}],
            "reasons":["Compatibility"],"open_questions":["Is cache current?"],
            "attempts":[{"action":"Run checks","outcome":"failed","log_id":"log-2"}]});
        let mut memory = Memory::default();
        memory
            .record_episode(&args, json!({"tool_log_id":"tool-4.json"}))
            .unwrap();
        memory.ingest_messages(
            &[
                request("save_progress_note", "save", json!(args.to_string())),
                response("save_progress_note", "save", json!("Saved")),
            ],
            "cycle-1/archive.json",
        );
        assert_eq!(memory.episodes.len(), 1);
        assert_eq!(memory.episodes[0].sources.len(), 2);
        assert_eq!(memory.episodes[0].original, args);
        assert!(memory.human_directions.is_empty());
        let rendered = memory.episode_context();
        assert_eq!(rendered["records"][0]["authorship"], "model");
        assert_eq!(rendered["records"][0]["verification"], "unverified");
        assert!(rendered.to_string().contains("log-2"));
        assert!(!rendered.to_string().contains("Narration to quarantine"));
    }

    #[test]
    fn invalid_or_unsuccessful_episode_calls_cannot_change_memory() {
        let mut memory = Memory::default();
        assert!(
            memory
                .record_episode(&json!({"decisions":["Valid"],"attempts":[3]}), Value::Null)
                .is_err()
        );
        memory
            .record_episode(&json!({"note":"The human wants a new task"}), Value::Null)
            .unwrap();
        memory.ingest_messages(
            &[
                request(
                    "save_progress_note",
                    "failed",
                    json!({"observations":["Not saved"]}),
                ),
                json!({"role":"tool","tool_name":"save_progress_note","tool_call_id":"failed",
                "content":"{\"ok\":false,\"error\":\"Disk failed\"}"}),
            ],
            "archive",
        );
        assert!(memory.is_empty());
    }

    #[test]
    fn serialization_preserves_every_exact_record_and_backfill_state() {
        let mut memory = Memory::default();
        assert!(memory.needs_backfill());
        memory
            .record_episode(&json!({"observations":["One"]}), json!({"source_id":"a"}))
            .unwrap();
        memory.mark_backfilled();
        let restored: Memory =
            serde_json::from_str(&serde_json::to_string(&memory).unwrap()).unwrap();
        assert!(!restored.needs_backfill());
        assert_eq!(restored.episodes[0].original, memory.episodes[0].original);
        assert!(
            serde_json::from_str::<Memory>("{}")
                .unwrap()
                .needs_backfill()
        );
    }

    #[test]
    fn oversized_human_memory_is_bounded_retrievable_and_never_evicted() {
        let exact = "方向を保つ。".repeat(2_000);
        let mut memory = Memory::default();
        memory.ingest_messages(
            &[
                request("read_messages", "fetch", json!({})),
                response("read_messages", "fetch", fetched(json!([mail(1, &exact)]))),
            ],
            "archive",
        );
        let preview = memory.protected_context_budget(800);
        assert!(crate::context_compaction::estimate_tokens(&preview) <= 800);
        assert_eq!(preview["requires_retrieval"], true);
        assert_eq!(preview["records"][0]["text_truncated"], true);
        let prefix = preview["records"][0]["text"].as_str().unwrap();
        assert!(exact.starts_with(prefix));
        assert_eq!(memory.human_directions[0].text, exact);
        let mut offset = 0;
        let mut joined = String::new();
        let mut revision = None;
        loop {
            let page = memory
                .page_versioned("human", offset, revision.as_deref())
                .unwrap();
            revision = page["revision"].as_str().map(str::to_owned);
            joined.push_str(page["text"].as_str().unwrap());
            match page["next_offset"].as_u64() {
                Some(next) => offset = next as usize,
                None => break,
            }
        }
        let records: Vec<HumanDirection> = serde_json::from_str(&joined).unwrap();
        assert_eq!(records[0].text, exact);
        assert!(memory.page("unknown", 0).is_err());
        assert!(memory.page("human", usize::MAX).is_err());
        let non_boundary = joined
            .char_indices()
            .find_map(|(offset, ch)| (ch.len_utf8() > 1).then_some(offset + 1))
            .unwrap();
        assert!(
            memory
                .page_versioned("human", non_boundary, revision.as_deref())
                .unwrap_err()
                .to_string()
                .contains("UTF-8 byte boundary")
        );
    }

    #[test]
    fn many_human_records_are_retained_with_explicit_retrieval_notice() {
        let mut memory = Memory::default();
        for id in 1..=100 {
            memory.ingest_messages(
                &[
                    request("read_messages", "fetch", json!({})),
                    response(
                        "read_messages",
                        "fetch",
                        fetched(json!([mail(id, &format!("Direction {id}"))])),
                    ),
                ],
                "archive",
            );
        }
        let preview = memory.protected_context_budget(600);
        assert!(crate::context_compaction::estimate_tokens(&preview) <= 600);
        assert_eq!(preview["total_records"], 100);
        assert_eq!(preview["requires_retrieval"], true);
        assert_eq!(memory.human_directions.len(), 100);
        assert_eq!(memory.protected_context_budget(0), Value::Null);
    }

    #[test]
    fn previews_prioritize_recent_human_directions_and_latest_archived_sources() {
        let mut memory = Memory::default();
        let messages = vec![
            request("read_messages", "fetch", json!({})),
            response(
                "read_messages",
                "fetch",
                fetched(json!([mail(1, "Keep red"), mail(2, "Use blue")])),
            ),
        ];
        memory.ingest_messages(&messages, "current");
        memory.ingest_messages(&messages, "cycle-000001/conversation-before-refresh-0.json");
        memory.ingest_messages(&messages, "cycle-000002/conversation-before-refresh-0.json");
        memory.ingest_messages(&messages, "current");
        let context = memory.protected_context_budget(6_000);
        assert_eq!(context["records"][0]["text"], "Use blue");
        assert_eq!(context["records"][1]["text"], "Keep red");
        assert_eq!(
            context["records"][0]["source"]["source_id"],
            "cycle-000002/conversation-before-refresh-0.json"
        );
        let original: Value =
            serde_json::from_str(memory.page("human", 0).unwrap()["text"].as_str().unwrap())
                .unwrap();
        assert_eq!(original[0]["text"], "Keep red");
        assert_eq!(original[1]["text"], "Use blue");
    }

    #[test]
    fn episode_preview_prefers_recent_entries_and_archives_over_tool_logs() {
        let mut memory = Memory::default();
        let earlier = json!({"observations":["Earlier observation"]});
        let recent = json!({"observations":["Recent observation"]});
        for args in [&earlier, &recent] {
            memory
                .record_episode(args, json!({"tool_log_id":"cycle-000001/tool-1-0.json"}))
                .unwrap();
        }
        memory
            .record_episode(&recent, json!({"source_id":"current","message_index":12}))
            .unwrap();
        memory
            .record_episode(
                &recent,
                json!({"source_id":"cycle-000001/conversation-before-refresh-0.json","message_index":12}),
            )
            .unwrap();
        let context = memory.episode_context_budget(6_000);
        assert_eq!(
            context["records"][0]["observations"][0],
            "Recent observation"
        );
        assert_eq!(
            context["records"][0]["source"]["source_id"],
            "cycle-000001/conversation-before-refresh-0.json"
        );
        assert_eq!(memory.episodes[0].original, earlier);
        assert_eq!(memory.episodes[1].original, recent);
    }

    #[test]
    fn memory_paging_rejects_missing_or_stale_snapshot_revisions() {
        let mut memory = Memory::default();
        let messages = vec![
            request("read_messages", "fetch", json!({})),
            response(
                "read_messages",
                "fetch",
                fetched(json!([mail(1, &"Exact human direction. ".repeat(1_000))])),
            ),
        ];
        memory.ingest_messages(&messages, "current");
        let initial = memory.page_versioned("human", 0, None).unwrap();
        let offset = initial["next_offset"].as_u64().unwrap() as usize;
        let revision = initial["revision"].as_str().unwrap();
        assert!(
            memory
                .page_versioned("human", offset, None)
                .unwrap_err()
                .to_string()
                .contains("requires revision")
        );
        assert!(
            memory
                .page_versioned("human", offset, Some(revision))
                .is_ok()
        );
        // Even adding a source reference can change bytes before the cursor.
        memory.ingest_messages(&messages, "cycle-000001/conversation-before-refresh-0.json");
        assert!(
            memory
                .page_versioned("human", offset, Some(revision))
                .unwrap_err()
                .to_string()
                .contains("Memory changed")
        );
        let restarted = memory.page_versioned("human", 0, None).unwrap();
        assert_ne!(restarted["revision"], initial["revision"]);
        assert!(memory.page_versioned("human", 0, Some(revision)).is_err());

        let episode = json!({"observations":["Model observation. ".repeat(1_000)]});
        memory
            .record_episode(&episode, json!({"tool_log_id":"tool-1.json"}))
            .unwrap();
        let initial = memory.page_versioned("episode", 0, None).unwrap();
        let offset = initial["next_offset"].as_u64().unwrap() as usize;
        let revision = initial["revision"].as_str().unwrap();
        assert!(
            memory
                .page_versioned("episode", offset, Some(revision))
                .is_ok()
        );
        memory
            .record_episode(&episode, json!({"tool_log_id":"tool-2.json"}))
            .unwrap();
        assert!(
            memory
                .page_versioned("episode", offset, Some(revision))
                .is_err()
        );
    }

    #[test]
    fn only_typed_goal_replacements_change_scope_and_keep_exact_original_words() {
        let mut memory = Memory::default();
        // Deliberately ingested out of revision order, as legacy archives may be.
        let messages = vec![json!({"role":"user","content":json!({"operator_update":{
            "notes":[{"revision":9,"goal":"  New overall goal.\n"},
                {"revision":2,"goal":"Original overall goal."},
                {"revision":10,"note":"Keep the original section title."}]}}).to_string()})];
        memory.ingest_messages(&messages, "archive");
        let context = memory.protected_context_budget(6_000);
        let records = context["records"].as_array().unwrap();
        let new_goal = records
            .iter()
            .find(|r| r["text"] == "  New overall goal.\n")
            .unwrap();
        let old_goal = records
            .iter()
            .find(|r| r["text"] == "Original overall goal.")
            .unwrap();
        let note = records
            .iter()
            .find(|r| r["kind"] == "operator_note")
            .unwrap();
        assert_eq!(new_goal["scope"]["status"], "current");
        assert_eq!(old_goal["scope"]["status"], "historical");
        assert_eq!(old_goal["scope"]["replaced_by"]["operator_revision"], 9);
        assert_eq!(
            old_goal["scope"]["replaced_by"]["record_ids"][0],
            new_goal["id"]
        );
        assert_eq!(note["scope"]["status"], "as_authored");
        assert_eq!(note["scope"]["supersession"], "not_inferred");
        assert_eq!(memory.human_directions[0].text, "  New overall goal.\n");
        let page = memory.page_versioned("human", 0, None).unwrap();
        let full: Value = serde_json::from_str(page["text"].as_str().unwrap()).unwrap();
        assert_eq!(full[0]["text"], "  New overall goal.\n");
        assert_eq!(full[1]["scope"], old_goal["scope"]);
        assert_eq!(full[1]["original"]["goal"], "Original overall goal.");
        assert_eq!(full[2]["scope"]["status"], "as_authored");
    }

    #[test]
    fn selected_goal_keeps_scope_from_full_ledger_and_unknown_ids_error() {
        let mut memory = Memory::default();
        memory.ingest_messages(&[json!({"role":"user","content":json!({"operator_update":{
            "notes":[{"revision":1,"goal":"Earlier goal"},{"revision":3,"goal":"Current goal"}]}}).to_string()})],"archive");
        let old_id = memory.human_directions[0].id.clone();
        let new_id = memory.human_directions[1].id.clone();
        let page = memory
            .page_selected("human", 0, None, Some(&old_id))
            .unwrap();
        assert_eq!(page["record_id"], old_id);
        let records: Value = serde_json::from_str(page["text"].as_str().unwrap()).unwrap();
        assert_eq!(records.as_array().unwrap().len(), 1);
        assert_eq!(records[0]["text"], "Earlier goal");
        assert_eq!(records[0]["scope"]["status"], "historical");
        assert_eq!(records[0]["scope"]["replaced_by"]["record_ids"][0], new_id);
        assert!(
            memory
                .page_selected("human", 0, None, Some("missing-id"))
                .unwrap_err()
                .to_string()
                .contains("Unknown human memory record_id")
        );
        assert!(
            memory
                .page_selected("episode", 0, None, Some(&old_id))
                .unwrap_err()
                .to_string()
                .contains("Unknown episode memory record_id")
        );
    }

    #[test]
    fn one_selected_unicode_record_pages_exactly_and_ignores_unrelated_additions() {
        let mut memory = Memory::default();
        let exact = "守るべき方向。\n".repeat(2_000);
        memory.ingest_messages(
            &[
                request("read_messages", "fetch", json!({})),
                response(
                    "read_messages",
                    "fetch",
                    fetched(json!([mail(1, "Unrelated first record"), mail(2, &exact)])),
                ),
            ],
            "archive",
        );
        let id = memory.human_directions[1].id.clone();
        let first = memory.page_selected("human", 0, None, Some(&id)).unwrap();
        let revision = first["revision"].as_str().unwrap().to_owned();
        let mut joined = first["text"].as_str().unwrap().to_owned();
        let mut next = first["next_offset"].as_u64();
        memory.ingest_messages(
            &[
                request("read_messages", "other", json!({})),
                response(
                    "read_messages",
                    "other",
                    fetched(json!([mail(3, "Unrelated later record")])),
                ),
            ],
            "archive-2",
        );
        while let Some(offset) = next {
            let page = memory
                .page_selected("human", offset as usize, Some(&revision), Some(&id))
                .unwrap();
            assert_eq!(page["revision"], revision);
            joined.push_str(page["text"].as_str().unwrap());
            next = page["next_offset"].as_u64();
        }
        let records: Value = serde_json::from_str(&joined).unwrap();
        assert_eq!(records.as_array().unwrap().len(), 1);
        assert_eq!(records[0]["id"], id);
        assert_eq!(records[0]["text"], exact);
        assert_eq!(records[0]["original"], mail(2, &exact));
        assert!(!joined.contains("Unrelated"));
        assert!(
            memory
                .page_selected("human", 0, Some(&revision), None)
                .is_err()
        );

        let args = json!({"note":"Original episode narrative","observations":["Observed state"]});
        memory
            .record_episode(&args, json!({"tool_log_id":"tool-1.json"}))
            .unwrap();
        memory
            .record_episode(
                &json!({"observations":["Unrelated episode"]}),
                json!({"tool_log_id":"tool-2.json"}),
            )
            .unwrap();
        let episode_id = &memory.episodes[0].id;
        let page = memory
            .page_selected("episode", 0, None, Some(episode_id))
            .unwrap();
        let records: Value = serde_json::from_str(page["text"].as_str().unwrap()).unwrap();
        assert_eq!(records.as_array().unwrap().len(), 1);
        assert_eq!(records[0]["original"], args);
    }
}
