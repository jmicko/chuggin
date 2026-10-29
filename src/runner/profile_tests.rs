use super::*;
use crate::self_profile::Variant;

struct Fixture {
    _dir: tempfile::TempDir,
    config: Config,
    state: State,
    art: PathBuf,
}

impl Fixture {
    fn new(variant: Variant) -> Self {
        let dir = tempfile::tempdir().unwrap();
        // Keep the default configuration path covered: legacy configurations
        // omit self_profile and must continue to select the baseline.
        let mut config: Config = serde_json::from_value(json!({
            "repo":dir.path(),"state_dir":dir.path().join(".chuggin"),
            "activity_mode":"chaos","ollama_url":"http://unused","model":"unused",
            "context_tokens":65536,"output_tokens":1024,"implementation_calls":4,
            "checks":[],"retry_seconds":0
        }))
        .unwrap();
        assert_eq!(config.self_profile, Variant::Baseline);
        config.self_profile = variant;
        fs::create_dir(&config.state_dir).unwrap();
        let art = config.state_dir.join("cycle-000001");
        fs::create_dir(&art).unwrap();
        let state = State {
            activity_mode: config.activity_mode,
            working_workspace: dir.path().into(),
            working_ref: "files-synthetic-profile-observation".into(),
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

    fn update(&self, variant: Variant, patch: Value) -> (Value, Value) {
        let args = json!({
            "expected_revision":crate::self_profile::revision(&self.config.state_dir, variant).unwrap(),
            "reason":"A model-chosen, provisional preference after a synthetic observation",
            "evidence":["Synthetic observation in this test"],
            "patch":patch
        });
        let result = crate::self_profile::call(
            &self.config.state_dir,
            variant,
            "update_self_profile",
            &args,
        )
        .unwrap();
        (args, result)
    }

    fn handoff(&self, session: &mut Conversation) {
        // Ordinary narration forces a complete capacity handoff instead of the
        // cheaper old-tool-payload path, which leaves the live prefix intact.
        for index in 0..80 {
            session
                .messages
                .push(json!({"role":"assistant", "content":format!(
                    "Earlier synthetic observation {index}: {}", "historical detail ".repeat(180)
                )}));
        }
        refresh_conversation(
            &self.config,
            &self.state,
            session,
            &self.art,
            "Provider capacity handoff",
            true,
        )
        .unwrap();
    }
}

fn snapshots(session: &Conversation) -> Vec<Value> {
    session
        .messages
        .iter()
        .filter_map(|message| {
            let content: Value = serde_json::from_str(message["content"].as_str()?).ok()?;
            (content["kind"] == "model_authored_self_profile").then_some(content)
        })
        .collect()
}

fn profile_tools(session: &Conversation) -> Vec<&str> {
    session
        .tools
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tool| tool["function"]["name"].as_str())
        .filter(|name| matches!(*name, "read_self_profile" | "update_self_profile"))
        .collect()
}

fn only_snapshot(session: &Conversation) -> Value {
    let snapshots = snapshots(session);
    assert_eq!(
        snapshots.len(),
        1,
        "Only the active profile should be presented"
    );
    snapshots.into_iter().next().unwrap()
}

fn latest_snapshot(session: &Conversation) -> Value {
    snapshots(session).into_iter().last().unwrap()
}

fn profile(snapshot: &Value) -> &Value {
    &snapshot["profile"]["profile"]
}

#[test]
fn baseline_keeps_the_existing_conversation_and_tool_surface() {
    let fixture = Fixture::new(Variant::Baseline);
    let mut session = fixture.session();
    let original = serde_json::to_value(&session).unwrap();

    sync_self_profile(&fixture.config, &mut session).unwrap();

    assert_eq!(serde_json::to_value(&session).unwrap(), original);
    assert!(snapshots(&session).is_empty());
    assert!(profile_tools(&session).is_empty());
    assert!(crate::self_profile::schemas(Variant::Baseline).is_empty());
    assert!(session.memory.human_directions.is_empty());
}

#[test]
fn enabled_variants_begin_blank_without_assigning_a_goal() {
    for variant in [Variant::Personality, Variant::SelfDirected] {
        let mut fixture = Fixture::new(variant);
        fixture.config.goal = "STALE_GOAL_IS_NOT_A_SELF_PROFILE_ASSIGNMENT".into();
        let mut session = fixture.session();
        sync_self_profile(&fixture.config, &mut session).unwrap();

        let snapshot = only_snapshot(&session);
        assert_eq!(snapshot["profile"]["authorship"], "model");
        assert_eq!(snapshot["revision"], 0);
        assert_eq!(profile(&snapshot)["tendencies"], json!([]));
        if variant == Variant::SelfDirected {
            for field in ["interests", "open_questions", "intentions", "reflections"] {
                assert_eq!(profile(&snapshot)[field], json!([]), "{field} starts blank");
            }
        }
        assert_eq!(
            profile_tools(&session),
            vec!["read_self_profile", "update_self_profile"]
        );
        assert!(fixture.state.current_task.is_none());
        assert!(session.memory.human_directions.is_empty());
        let live = serde_json::to_string(&session.messages).unwrap();
        assert!(!live.contains("STALE_GOAL_IS_NOT_A_SELF_PROFILE_ASSIGNMENT"));
        assert!(!live.contains("main_goal"));
        assert!(live.contains("There is no assigned project goal"));
    }
}

#[test]
fn profile_updates_publish_once_and_routine_cycles_do_not_request_reflection() {
    let fixture = Fixture::new(Variant::SelfDirected);
    let mut session = fixture.session();
    fixture.update(
        Variant::SelfDirected,
        json!({
            "tendencies":["Prefer retaining two plausible explanations until evidence differs"],
            "interests":["Comparing amber samples"],
            "open_questions":["Does the apparent color change persist in daylight?"],
            "intentions":["Optionally compare the samples when the lighting changes"]
        }),
    );

    sync_self_profile(&fixture.config, &mut session).unwrap();
    let snapshot = latest_snapshot(&session);
    assert_eq!(snapshot["revision"], 1);
    assert_eq!(snapshot["profile"]["authorship"], "model");
    assert_eq!(
        profile(&snapshot)["open_questions"][0],
        "Does the apparent color change persist in daylight?"
    );
    let original = serde_json::to_value(&session).unwrap();
    for _ in 0..12 {
        sync_self_profile(&fixture.config, &mut session).unwrap();
        assert_eq!(serde_json::to_value(&session).unwrap(), original);
    }
    save_conversation(&fixture.config, &session).unwrap();
    let mut resumed = fixture.session();
    sync_self_profile(&fixture.config, &mut resumed).unwrap();
    assert_eq!(latest_snapshot(&resumed), snapshot);
    assert_eq!(resumed.messages, session.messages);
    assert!(resumed.memory.human_directions.is_empty());
}

#[test]
fn cold_conversation_rehydrates_the_profile_as_model_authored_optional_context() {
    let fixture = Fixture::new(Variant::SelfDirected);
    let initial = fixture.session();
    assert_eq!(only_snapshot(&initial)["revision"], 0);
    fixture.update(
        Variant::SelfDirected,
        json!({
            "tendencies":["MODEL_AUTHORED_AMBER_PREFERENCE"],
            "intentions":["Optionally return to the amber comparison"]
        }),
    );
    fs::remove_file(fixture.config.state_dir.join("conversation.json")).unwrap();

    let mut resumed = fixture.session();
    sync_self_profile(&fixture.config, &mut resumed).unwrap();

    let snapshot = only_snapshot(&resumed);
    assert_eq!(snapshot["revision"], 1);
    assert_eq!(snapshot["profile"]["authorship"], "model");
    assert_eq!(
        profile(&snapshot)["tendencies"][0],
        "MODEL_AUTHORED_AMBER_PREFERENCE"
    );
    assert!(resumed.memory.human_directions.is_empty());
    assert!(fixture.state.current_task.is_none());
    assert!(fixture.state.goal.is_empty());
    assert!(
        !serde_json::to_string(&resumed.messages)
            .unwrap()
            .contains("main_goal")
    );
}

#[test]
fn capacity_handoff_preserves_profile_without_promoting_it_to_human_authority() {
    let mut fixture = Fixture::new(Variant::SelfDirected);
    let exact_human_note = "Preserve both amber samples; an open question may remain open.";
    save(
        &fixture.config.state_dir.join("operator-handoff.json"),
        &json!({"revision":1,"notes":[{"revision":1,"note":exact_human_note}]}),
    )
    .unwrap();
    let mut session = fixture.session();
    sync_operator(&fixture.config, &mut fixture.state, &mut session).unwrap();
    let (args, result) = fixture.update(
        Variant::SelfDirected,
        json!({
            "tendencies":["MODEL_AUTHORED_PROFILE_MUST_NOT_BECOME_HUMAN_DIRECTION"],
            "open_questions":["Could a different light explain the amber color?"]
        }),
    );
    session
        .messages
        .push(json!({"role":"assistant","content":"","tool_calls":[{
            "id":"synthetic-profile-update","type":"function","function":{
                "name":"update_self_profile","arguments":args
            }
        }]}));
    session.messages.push(json!({
        "role":"tool","tool_name":"update_self_profile","tool_call_id":"synthetic-profile-update",
        "content":json!({"ok":true,"result":result}).to_string()
    }));
    sync_self_profile(&fixture.config, &mut session).unwrap();
    let profile = latest_snapshot(&session);

    fixture.handoff(&mut session);
    sync_self_profile(&fixture.config, &mut session).unwrap();

    assert_eq!(only_snapshot(&session), profile);
    assert_eq!(session.memory.human_directions.len(), 1);
    assert_eq!(session.memory.human_directions[0].text, exact_human_note);
    assert_eq!(session.memory.human_directions[0].kind, "operator_note");
    assert!(
        !serde_json::to_string(&session.memory.human_directions)
            .unwrap()
            .contains("MODEL_AUTHORED_PROFILE_MUST_NOT_BECOME_HUMAN_DIRECTION")
    );
    assert_eq!(only_snapshot(&fixture.session()), profile);
    let stats: Value =
        serde_json::from_slice(&fs::read(fixture.art.join("context-compaction-0.json")).unwrap())
            .unwrap();
    assert_eq!(stats["kind"], "Capacity");
    assert!(fixture.state.current_task.is_none());
}

#[test]
fn switching_variants_removes_old_profile_context_and_baseline_tools() {
    let mut fixture = Fixture::new(Variant::SelfDirected);
    fixture.update(
        Variant::SelfDirected,
        json!({"intentions":["SELF_DIRECTED_ONLY_OPTIONAL_INTENTION"]}),
    );
    fixture.update(
        Variant::Personality,
        json!({"tendencies":["PERSONALITY_ONLY_MODEL_TENDENCY"]}),
    );
    let mut session = fixture.session();
    sync_self_profile(&fixture.config, &mut session).unwrap();
    assert_eq!(only_snapshot(&session)["variant"], "self_directed");
    save_conversation(&fixture.config, &session).unwrap();

    fixture.config.self_profile = Variant::Personality;
    let mut session = fixture.session();
    sync_self_profile(&fixture.config, &mut session).unwrap();
    assert_eq!(only_snapshot(&session)["variant"], "personality");
    let live = serde_json::to_string(&session.messages).unwrap();
    assert!(live.contains("PERSONALITY_ONLY_MODEL_TENDENCY"));
    assert!(!live.contains("SELF_DIRECTED_ONLY_OPTIONAL_INTENTION"));
    save_conversation(&fixture.config, &session).unwrap();

    fixture.config.self_profile = Variant::Baseline;
    let mut session = fixture.session();
    sync_self_profile(&fixture.config, &mut session).unwrap();
    assert!(snapshots(&session).is_empty());
    assert!(profile_tools(&session).is_empty());
    let live = serde_json::to_string(&session.messages).unwrap();
    assert!(!live.contains("PERSONALITY_ONLY_MODEL_TENDENCY"));
    assert!(!live.contains("SELF_DIRECTED_ONLY_OPTIONAL_INTENTION"));
    save_conversation(&fixture.config, &session).unwrap();

    fixture.config.self_profile = Variant::SelfDirected;
    let restored = fixture.session();
    assert_eq!(
        profile(&only_snapshot(&restored))["intentions"][0],
        "SELF_DIRECTED_ONLY_OPTIONAL_INTENTION"
    );
    assert!(restored.memory.human_directions.is_empty());
}
