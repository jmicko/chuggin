//! Operator conversations are durable and independent of the autonomous loop.
use crate::operator::{Controller, id, read_json};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
#[derive(Default)]
pub struct Chats {
    running: Arc<Mutex<BTreeMap<String, Arc<AtomicBool>>>>,
}
impl Chats {
    pub fn any_running(&self) -> bool {
        !self.running.lock().unwrap().is_empty()
    }
    pub fn send(&self, controller: Arc<Controller>, session: &str, message: &str) -> Result<Value> {
        controller.touch(session)?;
        ensure!(
            !message.trim().is_empty() && message.len() <= 100000,
            "Message must contain 1–100000 bytes"
        );
        let mut running = self.running.lock().unwrap();
        ensure!(
            !running.contains_key(session),
            "Chat is still responding; cancel or wait before sending another message"
        );
        let stop = Arc::new(AtomicBool::new(false));

        let path = history_path(&controller, session)?;
        let mut history = read_json(&path).unwrap_or(json!({"messages":[],"busy":false}));
        history["messages"]
            .as_array_mut()
            .context("Invalid chat history")?
            .push(json!({"role":"user","content":message}));
        history["busy"] = json!(true);
        history["error"] = Value::Null;
        crate::setup::save(&path, &history)?;
        running.insert(session.into(), stop.clone());
        let session = session.to_owned();
        let active = self.running.clone();
        std::thread::spawn(move || {
            crate::events::set_actor(&session);
            let result = turn(&controller, &session, &path, &stop);
            let mut history = read_json(&path).unwrap_or(json!({"messages":[]}));
            history["busy"] = json!(false);
            if let Err(e) = result {
                history["error"] = json!(format!("{e:#}"));
            }
            let _ = crate::setup::save(&path, &history);
            active.lock().unwrap().remove(&session);
        });
        Ok(json!({"started":true}))
    }
    pub fn status(&self, controller: &Controller, session: &str) -> Result<Value> {
        controller.touch(session)?;
        let mut h =
            read_json(&history_path(controller, session)?).unwrap_or(json!({"messages":[]}));
        h["busy"] = json!(self.running.lock().unwrap().contains_key(session));
        if let Some(messages) = h["messages"].as_array_mut() {
            let total = messages.len();
            if total > 60 {
                messages.drain(..total - 60);
            }
            for m in messages {
                if let Some(text) = m["content"].as_str() {
                    m["content"] = json!(crate::project::excerpt(text, 12000));
                }
                if let Some(calls) = m["tool_calls"].as_array_mut() {
                    for c in calls {
                        c["function"]["arguments"] = Value::Null;
                    }
                }
            }
            h["total_messages"] = json!(total);
        }
        Ok(h)
    }
    pub fn heartbeat(&self, controller: &Controller) {
        for session in self.running.lock().unwrap().keys() {
            let _ = controller.touch(session);
        }
    }
    pub fn cancel(&self, session: &str) {
        if let Some(stop) = self.running.lock().unwrap().get(session) {
            stop.store(true, Ordering::SeqCst);
        }
    }
}
fn history_path(c: &Controller, session: &str) -> Result<std::path::PathBuf> {
    crate::operator::valid_id(session)?;
    Ok(c.config()?
        .state_dir
        .join("operator/sessions")
        .join(session)
        .join("chat.json"))
}
fn turn(
    controller: &Arc<Controller>,
    session: &str,
    path: &std::path::Path,
    stop: &Arc<AtomicBool>,
) -> Result<()> {
    let c = controller.config()?;
    let raw = read_json(&controller.path)?;
    let name = raw["chat_model"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or(&c.model);
    let mut model = crate::model::Model::new(
        &c.ollama_url,
        name,
        c.context_tokens,
        c.output_tokens,
        stop.clone(),
    )?;
    model.use_chat_settings(
        &controller.path,
        &c.state_dir.join("operator/sessions").join(session),
    );
    let art = c
        .state_dir
        .join("operator/sessions")
        .join(session)
        .join(format!("turn-{}", id()));
    fs::create_dir_all(&art)?;
    model.trace_to(&art);
    let mut record = read_json(path)?;
    let messages = record["messages"]
        .as_array_mut()
        .context("Invalid history")?;
    if !messages.first().is_some_and(|m| m["role"] == "system") {
        messages.insert(0,json!({"role":"system","content":"You are Chuggin's interactive project assistant. Respond to this user's messages, inspect real files and saved history, and use project control tools only for their requested direction. The autonomous loop is a separate conversation. Questions do not require pausing. Before editing or running a command, acquire begin_edit and wait until granted; retain ownership throughout your edits and checks, then end_edit with a factual summary. Never edit Chuggin runtime/config files through shell. Preserve user files and staging. Current files and observed check results take precedence over historical claims. Read before editing; fetch details through tools instead of assuming the entire loop transcript is in context. Do not silently change the overall goal or resume/stop the loop to answer a question. Answer when the user's request is handled; don't continue autonomously after your answer. A temporary nudge is distinct from the overall goal. Don't claim commands succeeded until their results are observed."}));
    }
    // Repair an interrupted batch without executing possibly completed side effects again.
    crate::runner::repair_pending_tools(messages);
    messages.push(json!({"role":"user","chuggin_context":true,"content":json!({"current_project":controller.status()?,"instruction":"This is live project context, not a request to start the loop. Continue answering the preceding user message."}).to_string()}));
    crate::setup::save(path, &record)?;
    let mut pressure = false;
    let result = (|| -> Result<()> {
        loop {
            ensure!(
                !stop.load(Ordering::SeqCst),
                "Chat cancelled; completed work retained"
            );
            controller.touch(session)?;
            let mut history = read_json(path)?;
            if pressure {
                refresh_context(&mut history);
                crate::setup::save(path, &history)?;
            }
            let context = context_messages(&history);
            let response = model.chat(&context, Some(crate::operator::schemas()), false)?;
            let _ = model.take_completed_messages();
            pressure = model.context_pressure();
            let messages = history["messages"]
                .as_array_mut()
                .context("Invalid chat messages")?;
            let calls = response["tool_calls"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            messages.push(response);
            crate::setup::save(path, &history)?;
            if calls.is_empty() {
                break;
            }
            for call in calls {
                ensure!(
                    !stop.load(Ordering::SeqCst),
                    "Chat cancelled; pending tools will not replay"
                );
                let name = call["function"]["name"]
                    .as_str()
                    .context("Missing tool name")?;
                let args = call["function"]["arguments"].clone();
                if [
                    "edit_file",
                    "write_file",
                    "run_command",
                    "run_checks",
                    "save_checkpoint",
                    "commit_changes",
                    "restore_checkpoint",
                ]
                .contains(&name)
                {
                    while controller.begin_edit(session)?["granted"] != true {
                        ensure!(
                            !stop.load(Ordering::SeqCst),
                            "Chat cancelled while waiting for editing"
                        );
                        crate::events::log(
                            "Waiting for the loop or its command before editing".into(),
                        );
                        std::thread::sleep(Duration::from_millis(500));
                    }
                }
                let op = id();
                crate::events::send(crate::events::Event::Tool(name.into()));
                let result = controller
                    .call(session, name, args, &op)
                    .unwrap_or_else(|e| json!({"status":"failed","error":format!("{e:#}")}));
                let mut history = read_json(path)?;
                let mut reply =
                    json!({"role":"tool","tool_name":name,"content":result.to_string()});
                if let Some(id) = call.get("id") {
                    reply["tool_call_id"] = id.clone();
                }
                history["messages"].as_array_mut().unwrap().push(reply);
                crate::setup::save(path, &history)?;
            }
        }
        Ok(())
    })();
    controller.finish_chat(session);
    result
}

// Preserve the full transcript on disk; only the provider's working context is shortened.
fn refresh_context(record: &mut Value) {
    let Some(messages) = record["messages"].as_array() else {
        return;
    };
    let minimum = messages.len().saturating_sub(10).max(1);
    let start = messages
        .iter()
        .enumerate()
        .skip(minimum)
        .find(|(_, m)| m["role"] == "assistant")
        .map(|(i, _)| i)
        .unwrap_or(messages.len());
    record["context_start"] = json!(start);
}
fn context_messages(record: &Value) -> Vec<Value> {
    let messages = record["messages"].as_array().unwrap();
    let start = record["context_start"].as_u64().unwrap_or(0) as usize;
    let mut result = if start > 0 {
        let mut head = vec![messages[0].clone()];
        if let Some(user) = messages
            .iter()
            .rfind(|m| m["role"] == "user" && m["chuggin_context"] != true)
        {
            head.push(user.clone());
        }
        head.push(json!({"role":"user","content":"Earlier conversation is retained in this operator session's chat.json and can be retrieved with read_history. Continue the current user request using these recent results and real files. Re-read details when needed; no actions have been undone."}));
        head.extend(messages.iter().skip(start).cloned());
        head
    } else {
        messages.clone()
    };
    for m in &mut result {
        if let Some(o) = m.as_object_mut() {
            o.remove("chuggin_context");
        }
    }
    result
}
