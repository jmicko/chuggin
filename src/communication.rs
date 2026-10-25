//! Host-side mailbox/presence integration. Bodies are retrieved explicitly;
//! notification metadata never starts inference or injects a user's text.
use anyhow::Result;
use serde_json::{Value, json};
use std::path::Path;

pub fn presence() -> crate::presence::Snapshot {
    crate::presence::snapshot(
        &crate::setup::settings()
            .map(|s| s.presence)
            .unwrap_or_default(),
    )
}

pub fn public_presence() -> Value {
    let p = presence();
    json!({"status":p.status,"automatic":p.automatic,"idle_minutes":p.idle_seconds.map(|s|s/60),"locked":p.locked,"availability_is_advisory":true})
}

pub fn status(state: &Path) -> Result<Value> {
    Ok(json!({"mailbox":crate::messenger::status(state)?,"user_presence":public_presence()}))
}

pub fn model_tool(state: &Path, name: &str, args: &Value, operation: &str) -> Result<Value> {
    let mut args = args.clone();
    args["_operation_id"] = json!(operation);
    let mut result = match name {
        "read_messages" => crate::messenger::model_read(state, &args)?,
        "send_message" => crate::messenger::model_send(state, &args)?,
        _ => anyhow::bail!("Unknown messenger tool"),
    };
    result["user_presence"] = public_presence();
    if name == "send_message" && result["changed"] == true {
        crate::events::log(format!(
            "New message #{} from the model · M opens Messages",
            result["message_id"]
        ));
    }
    Ok(result)
}

pub fn operator_schemas() -> Vec<Value> {
    vec![
        json!({"type":"function","function":{"name":"messenger_status","description":"Check mailbox counts and advisory human presence without reading bodies or starting inference.","parameters":{"type":"object","properties":{}}}}),
        json!({"type":"function","function":{"name":"message_loop","description":"Send a message to the looping model on this user's behalf. It is queued in its mailbox; the model must fetch it explicitly. Sending does not resume the loop, replace its goal or create a nudge.","parameters":{"type":"object","properties":{"body":{"type":"string"},"reply_to":{"type":"integer","minimum":1}},"required":["body"]}}}),
        json!({"type":"function","function":{"name":"read_messenger","description":"Read the user's messenger history in pages. Does not mark messages read on the human's behalf or start inference.","parameters":{"type":"object","properties":{"before_id":{"type":"integer","minimum":1},"limit":{"type":"integer","minimum":1,"maximum":100}}}}}),
    ]
}
