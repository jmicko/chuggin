use super::*;

struct Fixture {
    _dir: tempfile::TempDir,
    config: Config,
    state: State,
    art: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config: Config = serde_json::from_value(json!({
            "repo":dir.path(),"state_dir":dir.path().join(".chuggin"),
            "activity_mode":"chaos","ollama_url":"http://unused","model":"unused",
            "context_tokens":65536,"output_tokens":1024,"implementation_calls":4,
            "checks":[],"retry_seconds":0
        }))
        .unwrap();
        fs::create_dir(&config.state_dir).unwrap();
        let art = config.state_dir.join("cycle-000001");
        fs::create_dir(&art).unwrap();
        let state = State {
            activity_mode: config.activity_mode,
            working_workspace: dir.path().into(),
            working_ref: "files-synthetic-observation".into(),
            cycle: 1,
            ..State::default()
        };
        Self {
            _dir: dir,
            config,
            state,
            art,
        }
    }

    fn session(&self) -> Conversation {
        load_conversation(&self.config, &self.state).unwrap()
    }

    fn refresh(&self, session: &mut Conversation, capacity: bool) {
        refresh_conversation(
            &self.config,
            &self.state,
            session,
            &self.art,
            if capacity {
                "Provider capacity handoff"
            } else {
                "Repeated narration continued after feedback"
            },
            capacity,
        )
        .unwrap();
    }

    fn archive(&self) -> Value {
        serde_json::from_slice(
            &fs::read(self.art.join("conversation-before-refresh-0.json")).unwrap(),
        )
        .unwrap()
    }

    fn stats(&self) -> Value {
        serde_json::from_slice(&fs::read(self.art.join("context-compaction-0.json")).unwrap())
            .unwrap()
    }
}

fn request(name: &str, id: &str, args: Value) -> Value {
    json!({"role":"assistant","content":"","tool_calls":[{
        "id":id,"type":"function","function":{"name":name,"arguments":args}
    }]})
}

fn reply(name: &str, id: &str, result: Value) -> Value {
    json!({"role":"tool","tool_name":name,"tool_call_id":id,
        "content":json!({"ok":true,"result":result}).to_string()})
}

fn old_narration(session: &mut Conversation) {
    for index in 0..80 {
        session.messages.push(json!({"role":"assistant",
            "content":format!("Earlier observation {index}: {}", "historical detail ".repeat(180))}));
    }
}

fn memory_records(session: &Conversation, section: &str) -> (Value, Vec<Value>) {
    let mut offset = 0;
    let mut text = String::new();
    let first = memory_page(session, &json!({"section":section})).unwrap();
    loop {
        let page = memory_page(
            session,
            &json!({"section":section,"offset":offset,"revision":first["revision"]}),
        )
        .unwrap();
        text.push_str(page["text"].as_str().unwrap());
        let Some(next) = page["next_offset"].as_u64() else {
            break;
        };
        assert!(next > offset);
        offset = next;
    }
    (first, serde_json::from_str(&text).unwrap())
}

fn decoded_messages(session: &Conversation) -> Vec<Value> {
    session
        .messages
        .iter()
        .filter_map(|message| serde_json::from_str(message["content"].as_str()?).ok())
        .collect()
}

#[test]
fn capacity_handoff_backfills_old_operator_words_when_revision_blocks_replay() {
    let mut fixture = Fixture::new();
    let exact = "  Preserve the notebook's original names.\nQuestions may remain open.  ";
    save(
        &fixture.config.state_dir.join("operator-handoff.json"),
        &json!({"revision":7,"notes":[{
            "revision":7,"note":exact,"summary":"Harness action summary is not human text"
        }]}),
    )
    .unwrap();
    let mut session = fixture.session();
    sync_operator(&fixture.config, &mut fixture.state, &mut session).unwrap();
    assert_eq!(session.operator_revision, 7);

    // An older release could have archived the delivered update without saving
    // dedicated memory. Its revision still prevents the operator from replaying it.
    let old_cycle = fixture.config.state_dir.join("cycle-000000");
    fs::create_dir(&old_cycle).unwrap();
    save(
        &old_cycle.join("conversation-before-refresh-0.json"),
        &session,
    )
    .unwrap();
    session.messages.truncate(2);
    session.memory = crate::context_memory::Memory::default();
    old_narration(&mut session);
    let before_sync = session.messages.clone();
    sync_operator(&fixture.config, &mut fixture.state, &mut session).unwrap();
    assert_eq!(session.messages, before_sync);
    assert!(session.memory.is_empty());

    fixture.refresh(&mut session, true);
    let (_, records) = memory_records(&session, "human");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["text"], exact);
    assert_eq!(records[0]["kind"], "operator_note");
    assert_eq!(records[0]["sources"][0]["operator_revision"], 7);
    assert_eq!(
        records[0]["sources"][0]["source_id"],
        "cycle-000000/conversation-before-refresh-0.json"
    );
    let memory = decoded_messages(&session)
        .into_iter()
        .find(|value| value.get("human_direction_memory").is_some())
        .unwrap();
    assert_eq!(
        memory["human_direction_memory"]["records"][0]["text"],
        exact
    );
    let count = session.messages.len();
    sync_operator(&fixture.config, &mut fixture.state, &mut session).unwrap();
    assert_eq!(session.messages.len(), count);
    let resumed = load_conversation(&fixture.config, &fixture.state).unwrap();
    assert_eq!(memory_records(&resumed, "human").1[0]["text"], exact);
}

#[test]
fn explicitly_fetched_mail_survives_handoff_without_opening_unread_mail() {
    let fixture = Fixture::new();
    let mut session = fixture.session();
    let exact = "Keep the amber sample; you may explore its color without selecting a task.";
    let sent = crate::messenger::human_send(&fixture.config.state_dir, exact, None).unwrap();
    let fetched = crate::messenger::model_read(
        &fixture.config.state_dir,
        &json!({"_operation_id":"portable-context-test-fetch"}),
    )
    .unwrap();
    assert_eq!(fetched["receipt"], "fetched");
    session
        .messages
        .push(request("read_messages", "fetched-mail", json!({})));
    session.messages.push(reply(
        "read_messages",
        "fetched-mail",
        json!(fetched.to_string()),
    ));
    let unread = "UNFETCHED_PRIVATE_MAIL_MUST_STAY_OUT_OF_CONTEXT";
    crate::messenger::human_send(&fixture.config.state_dir, unread, None).unwrap();
    assert_eq!(
        crate::messenger::status(&fixture.config.state_dir)
            .unwrap()
            .unread_to_model,
        1
    );
    old_narration(&mut session);

    fixture.refresh(&mut session, true);
    let (_, records) = memory_records(&session, "human");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["text"], exact);
    assert_eq!(records[0]["kind"], "mailbox");
    assert!(
        records[0]["sources"]
            .as_array()
            .unwrap()
            .iter()
            .any(|source| {
                source["mailbox_message_id"] == sent.message.id
                    && source["tool_call_id"] == "fetched-mail"
            })
    );
    assert!(!serde_json::to_string(&session).unwrap().contains(unread));
    assert_eq!(
        crate::messenger::status(&fixture.config.state_dir)
            .unwrap()
            .unread_to_model,
        1,
        "Compaction must neither inspect nor mark unread human mail fetched"
    );
    let resumed = memory_records(
        &load_conversation(&fixture.config, &fixture.state).unwrap(),
        "human",
    )
    .1;
    assert_eq!(resumed.len(), records.len());
    for field in ["id", "kind", "text", "original"] {
        assert_eq!(resumed[0][field], records[0][field]);
    }
    for source in records[0]["sources"].as_array().unwrap() {
        assert!(resumed[0]["sources"].as_array().unwrap().contains(source));
    }
}

#[test]
fn repetition_retains_structured_observations_and_questions_with_model_provenance() {
    let fixture = Fixture::new();
    let mut session = fixture.session();
    let private_narration = "REPETITIVE_PRIVATE_MODEL_NOTE";
    let args = json!({
        "text":private_narration,
        "observations":[{"claim":"The amber sample darkened under the lamp",
            "evidence":{"source_id":"cycle-000000/conversation-before-refresh-0.json","message_index":4}}],
        "decisions":[{"choice":"Keep both samples","reason":"The comparison remains useful"}],
        "reasons":["The lighting differed between observations"],
        "open_questions":["Does the apparent color change persist in daylight?"],
        "attempts":[{"action":"Compared samples","outcome":"Inconclusive"}]
    });
    session
        .messages
        .push(request("save_progress_note", "episode-note", args.clone()));
    session
        .messages
        .push(reply("save_progress_note", "episode-note", json!("Saved")));
    session.note.set_note(private_narration).unwrap();
    for _ in 0..16 {
        session
            .messages
            .push(json!({"role":"assistant","content":private_narration}));
    }

    fixture.refresh(&mut session, false);
    let (page, records) = memory_records(&session, "episode");
    assert_eq!(page["authorship"], "model");
    assert_eq!(page["verification"], "unverified");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["original"], args);
    assert_eq!(records[0]["observations"], args["observations"]);
    assert_eq!(records[0]["open_questions"], args["open_questions"]);
    assert_eq!(records[0]["sources"][0]["tool_call_id"], "episode-note");
    assert!(memory_records(&session, "human").1.is_empty());
    let memory = decoded_messages(&session)
        .into_iter()
        .find(|value| value.get("episode_memory").is_some())
        .unwrap();
    assert_eq!(memory["episode_memory"]["authorship"], "model");
    assert_eq!(
        memory["episode_memory"]["verification"],
        "unverified_claims"
    );
    assert_eq!(
        memory["episode_memory"]["records"][0]["open_questions"],
        args["open_questions"]
    );
    assert!(
        !serde_json::to_string(&session.messages)
            .unwrap()
            .contains(private_narration)
    );
    assert!(fixture.archive().to_string().contains(private_narration));
}

#[test]
fn capacity_handoff_budgets_complete_recent_exchanges_and_archives_exact_original() {
    let fixture = Fixture::new();
    let mut session = fixture.session();
    old_narration(&mut session);
    for index in 0..12 {
        let id = format!("recent-{index}");
        session.messages.push(request(
            "read_file",
            &id,
            json!({"path":"sample.txt","start_line":index + 1}),
        ));
        session.messages.push(reply(
            "read_file",
            &id,
            json!({"text":format!("Observed sample {index}")}),
        ));
    }
    let newest = session.messages[session.messages.len() - 2..].to_vec();
    let original = serde_json::to_value(&session).unwrap();
    let budget = handoff_budget(&fixture.config, &session);
    assert!(
        budget > 1024,
        "Fixture must allow a useful recent exchange window"
    );

    fixture.refresh(&mut session, true);
    assert_eq!(fixture.archive(), original);
    assert_eq!(
        crate::history::messages(
            &fixture.config.state_dir,
            "cycle-000001/conversation-before-refresh-0.json"
        )
        .unwrap(),
        original["messages"].as_array().unwrap().clone()
    );
    assert!(
        session
            .messages
            .windows(newest.len())
            .any(|window| window == newest)
    );
    let mut expected = Vec::<Value>::new();
    for message in &session.messages {
        if message["role"] == "tool" {
            let id = message["tool_call_id"].clone();
            let position = expected
                .iter()
                .position(|candidate| *candidate == id)
                .expect("Every retained result needs its original call");
            expected.remove(position);
        } else {
            assert!(
                expected.is_empty(),
                "A completed batch must remain contiguous and complete"
            );
            if let Some(calls) = message["tool_calls"].as_array() {
                expected.extend(calls.iter().map(|call| call["id"].clone()));
            }
        }
    }
    assert!(expected.is_empty());
    let stats = fixture.stats();
    assert_eq!(stats["kind"], "Capacity");
    assert_eq!(stats["selection"]["budget_exceeded"], false);
    assert!(stats["selection"]["estimated_tokens"].as_u64().unwrap() <= (budget * 3 / 5) as u64);
    assert!(stats["selection"]["archived_messages"].as_u64().unwrap() > 0);
}

#[test]
fn large_old_tool_result_is_cleared_before_healthy_recent_conversation() {
    let fixture = Fixture::new();
    let mut session = fixture.session();
    session.messages.push(request(
        "read_file",
        "old-large",
        json!({"path":"large.txt"}),
    ));
    let old_index = session.messages.len();
    let mut old_reply = reply(
        "read_file",
        "old-large",
        json!({"text":"x".repeat(250_000)}),
    );
    old_reply["_chuggin_responses"] =
        json!({"items":[{"type":"function_call_output","output":"OLD_NATIVE_PAYLOAD"}]});
    session.messages.push(old_reply);
    for index in 0..20 {
        session.messages.push(json!({"role":"assistant",
            "content":format!("Later independent observation {index}: {}", "observed texture ".repeat(150))}));
    }
    session.messages.push(
        json!({"role":"assistant","content":"The new sample has a distinct useful texture."}),
    );
    session.messages.push(request(
        "read_file",
        "new-small",
        json!({"path":"current.txt"}),
    ));
    session.messages.push(reply(
        "read_file",
        "new-small",
        json!({"text":"Current observed texture"}),
    ));
    session.messages.push(request(
        "read_file",
        "still-pending",
        json!({"path":"next.txt"}),
    ));
    let original = serde_json::to_value(&session).unwrap();
    let before = session.messages.clone();

    fixture.refresh(&mut session, true);
    assert_eq!(fixture.archive(), original);
    assert_eq!(session.messages.len(), before.len() + 1);
    for (index, message) in before.iter().enumerate() {
        if index != old_index {
            assert_eq!(
                &session.messages[index], message,
                "Healthy context changed at message {index}"
            );
        }
    }
    assert!(
        session.messages[old_index]
            .get("_chuggin_responses")
            .is_none()
    );
    let reference: Value =
        serde_json::from_str(session.messages[old_index]["content"].as_str().unwrap()).unwrap();
    assert_eq!(
        reference["source_id"],
        "cycle-000001/conversation-before-refresh-0.json"
    );
    assert_eq!(reference["message_index"], old_index);
    assert_eq!(fixture.stats()["kind"], "tool_results");
    assert_eq!(fixture.stats()["selection"]["shortened_tool_results"], 1);
}

#[test]
fn short_standby_repetition_hands_off_early_without_assigning_a_goal() {
    let mut fixture = Fixture::new();
    fixture.config.goal = "STALE_GOAL_MUST_NOT_BECOME_AN_ASSIGNMENT".into();
    let mut session = fixture.session();
    let standby = "All complete. Standing by.";
    let mut notices = 0;
    let mut responses = 0;
    while responses < 32 && !session.prose_watch.pending_refresh {
        session
            .messages
            .push(json!({"role":"assistant","content":standby}));
        observe_prose(&mut session, &fixture.art, standby, false, false, false).unwrap();
        responses += 1;
        if session.prose_watch.take_notice() {
            notices += 1;
            session.messages.push(json!({"role":"user","content":crate::prompts::reasoning_recovery(fixture.config.activity_mode)}));
        }
    }
    assert_eq!(notices, 1);
    assert!(responses <= 16);
    assert!(session.prose_watch.pending_refresh);
    fixture.refresh(&mut session, false);
    session.prose_watch.progress();
    let context = serde_json::to_string(&session.messages).unwrap();
    assert!(!context.contains(standby));
    assert!(!context.contains("STALE_GOAL_MUST_NOT_BECOME_AN_ASSIGNMENT"));
    assert!(!context.contains("main_goal"));
    assert!(context.contains("There is no assigned goal or requirement to choose a task"));
    assert!(fixture.state.current_task.is_none());
    assert!(memory_records(&session, "human").1.is_empty());
    assert_eq!(fixture.stats()["kind"], "Repetition");
    assert!(
        fixture.stats()["selection"]["quarantined_narration"]
            .as_u64()
            .unwrap()
            >= 16
    );
    assert_eq!(
        fixture.archive()["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["content"] == standby)
            .count(),
        responses
    );
}

/// Opt in with CHUGGIN_CONTEXT_REPLAY_ROOT pointing at a saved project or its
/// state directory. Only allowlisted immutable archives are opened. No provider,
/// live conversation, mailbox, command, or project file is executed or changed.
#[test]
#[ignore = "requires CHUGGIN_CONTEXT_REPLAY_ROOT with saved repeated-output archives"]
fn replay_saved_context_archives_read_only() {
    let root = PathBuf::from(
        std::env::var_os("CHUGGIN_CONTEXT_REPLAY_ROOT")
            .expect("Set CHUGGIN_CONTEXT_REPLAY_ROOT to an archived project or state directory"),
    );
    let state = if root.join("conversation.json").exists() {
        root
    } else {
        root.join(".chuggin")
    };
    let sources: Vec<_> = crate::history::sources(&state)
        .unwrap()
        .into_iter()
        .filter(|source| source != "current")
        .collect();
    assert!(!sources.is_empty(), "Replay needs immutable loop archives");
    let mut memory = crate::context_memory::Memory::default();
    let mut recovered_archives = 0;
    let mut short_recovered_archives = 0;
    for source in &sources {
        let original_bytes = fs::read(state.join(source)).unwrap();
        let messages = crate::history::messages(&state, source).unwrap();
        memory.ingest_messages(&messages, source);
        let mut prose = crate::prose_watch::ProseWatch::default();
        let mut actions = crate::action_watch::ActionWatch::default();
        let mut pending = Vec::<Value>::new();
        let (mut assistant_count, mut tool_count, mut notices) = (0, 0, 0);
        let mut first_refresh = None;
        let mut first_short_refresh = None;
        for message in &messages {
            match message["role"].as_str() {
                Some("assistant") => {
                    assistant_count += 1;
                    let calls = message["tool_calls"].as_array();
                    let has_calls = calls.is_some_and(|calls| !calls.is_empty());
                    if let Some(calls) = calls {
                        tool_count += calls.len();
                        pending.extend(calls.iter().cloned());
                    }
                    prose.observe_response(
                        message["content"].as_str().unwrap_or(""),
                        false,
                        has_calls,
                    );
                    if prose.take_notice() {
                        notices += 1;
                    }
                    if prose.pending_refresh {
                        first_refresh.get_or_insert(assistant_count);
                        if !has_calls
                            && message["content"]
                                .as_str()
                                .is_some_and(|text| text.chars().count() <= 180)
                        {
                            first_short_refresh.get_or_insert(assistant_count);
                        }
                        prose.progress();
                    }
                }
                Some("tool") => {
                    let position = pending.iter().position(|call| {
                        if let Some(id) = message.get("tool_call_id") {
                            call.get("id") == Some(id)
                        } else {
                            call["function"]["name"] == message["tool_name"]
                        }
                    });
                    if let Some(position) = position {
                        let call = pending.remove(position);
                        let args = call["function"]["arguments"]
                            .as_str()
                            .and_then(|text| serde_json::from_str(text).ok())
                            .unwrap_or_else(|| call["function"]["arguments"].clone());
                        let result = message["content"]
                            .as_str()
                            .and_then(|text| serde_json::from_str(text).ok())
                            .unwrap_or(Value::Null);
                        let revision = actions.evidence_revision;
                        actions.observe(
                            call["function"]["name"].as_str().unwrap_or(""),
                            &args,
                            &result,
                        );
                        if actions.evidence_revision != revision {
                            prose.short_progress();
                        }
                    }
                }
                Some("user") => {
                    if let Some(content) = message["content"]
                        .as_str()
                        .and_then(|text| serde_json::from_str::<Value>(text).ok())
                        && (content.get("operator_update").is_some()
                            || content.get("priority_update").is_some())
                    {
                        prose.progress();
                    }
                }
                _ => {}
            }
        }
        let compacted = crate::context_compaction::compact(
            &messages,
            source,
            crate::context_compaction::Reason::Repetition,
            8_000,
        );
        assert!(
            compacted.stats["estimated_tokens"].as_u64().unwrap() <= 8_000
                || compacted.stats["pending_groups"].as_u64().unwrap() > 0
        );
        if first_refresh.is_some() {
            recovered_archives += 1;
            assert!(compacted.stats["quarantined_narration"].as_u64().unwrap() > 0);
        }
        if first_short_refresh.is_some() {
            short_recovered_archives += 1;
        }
        assert_eq!(fs::read(state.join(source)).unwrap(), original_bytes);
        println!(
            "{source}: {assistant_count} assistant responses, {tool_count} tool calls, {notices} notices, first refresh {first_refresh:?}, first short refresh {first_short_refresh:?}, {} quarantined narration records, {} retained tool replies",
            compacted.stats["quarantined_narration"],
            compacted
                .recent
                .iter()
                .filter(|message| message["role"] == "tool")
                .count()
        );
    }
    assert!(
        recovered_archives > 0,
        "Fixture should exercise repeated-output recovery"
    );
    assert!(
        short_recovered_archives > 0,
        "Fixture should exercise the short-output detector"
    );
    for record in &memory.human_directions {
        let authored = match record.kind.as_str() {
            "operator_note" => &record.original["note"],
            "operator_goal" => &record.original["goal"],
            "mailbox" => &record.original["body"],
            other => panic!("Unexpected human record kind {other}"),
        };
        assert_eq!(authored.as_str(), Some(record.text.as_str()));
        assert!(
            record
                .sources
                .iter()
                .all(|source| source["source_id"] != "current")
        );
    }
    println!(
        "Read-only replay: {} archives, {recovered_archives} with earlier repetition recovery ({short_recovered_archives} short-output recoveries), {} delivered human directions, {} structured model episodes; no mailbox bodies fetched",
        sources.len(),
        memory.human_directions.len(),
        memory.episodes.len()
    );
}
