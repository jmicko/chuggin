//! MCP is an adapter to the same local operator service, not another runner.
use anyhow::Result;
use rmcp::{ErrorData, RoleServer, ServerHandler, ServiceExt, model::*, service::RequestContext};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
#[derive(Clone)]
struct Server {
    client: crate::engine::Client,
    sessions: Arc<Mutex<BTreeSet<String>>>,
}
fn tools() -> Vec<Tool> {
    let mut entries = Vec::new();
    entries.push(serde_json::from_value(json!({"name":"open_operator_session","description":"Open or resume a project operator session. Does not start the loop. Use the returned session_id on tools; retain editing control around any external native edits or commands.","inputSchema":{"type":"object","properties":{"session_id":{"type":"string"}}}})).unwrap());
    for value in crate::operator::schemas().as_array().unwrap() {
        let f = &value["function"];
        let mut schema = f["parameters"].clone();
        if !schema["required"].is_array() {
            schema["required"] = json!([]);
        }
        schema["properties"]["session_id"] = json!({"type":"string"});
        schema["properties"]["operation_id"] = json!({"type":"string","description":"Unique ID for this intended action. Reuse the same ID and arguments when retrying a disconnected request."});
        schema["required"]
            .as_array_mut()
            .unwrap()
            .push(json!("session_id"));
        if crate::operator::is_mutation(f["name"].as_str().unwrap()) {
            schema["required"]
                .as_array_mut()
                .unwrap()
                .push(json!("operation_id"));
        }
        entries.push(
            serde_json::from_value(
                json!({"name":f["name"],"description":f["description"],"inputSchema":schema}),
            )
            .unwrap(),
        );
    }
    entries
}
impl ServerHandler for Server {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        )
    }
    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        Ok(ListResourcesResult { resources:vec![serde_json::from_value(json!({"uri":"chuggin://project/status","name":"Project status","description":"Current goal, loop task, holds, schedule and command activity","mimeType":"application/json"})).unwrap()],..Default::default() })
    }
    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        if request.uri != "chuggin://project/status" {
            return Err(ErrorData::invalid_params(
                "Unknown resource; use history tools for paged artifacts",
                None,
            ));
        }
        let client = self.client.clone();
        let result =
            tokio::task::spawn_blocking(move || client.request(json!({"action":"status"})))
                .await
                .map_err(|e| ErrorData::internal_error(e.to_string(), None))?
                .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        Ok(ReadResourceResult::new(vec![
            ResourceContents::text(result.to_string(), request.uri)
                .with_mime_type("application/json"),
        ])
        .into())
    }
    fn get_tool(&self, name: &str) -> Option<Tool> {
        tools().into_iter().find(|t| t.name == name)
    }
    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult {
            tools: tools(),
            ..Default::default()
        })
    }
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let server = self.clone();
        let result=tokio::task::spawn_blocking(move||->Result<Value>{
            let mut args=Value::Object(request.arguments.unwrap_or_default());
            if request.name=="open_operator_session" {
                let session=server.client.open_session(args["session_id"].as_str())?;server.sessions.lock().unwrap().insert(session.clone());return Ok(json!({"session_id":session,"instruction":"Inspect freely. Request begin_edit before editing or native commands; retain ownership until all writers finish, then end_edit. Connecting does not start the loop."}));
            }
            let session=crate::operator::text(&args,"session_id")?.to_owned();
            anyhow::ensure!(server.sessions.lock().unwrap().contains(&session),"Open this operator session on this client before using its tools");
            let operation=args["operation_id"].as_str().map(str::to_owned).unwrap_or_else(crate::operator::id);
            args.as_object_mut().unwrap().remove("session_id");args.as_object_mut().unwrap().remove("operation_id");
            server.client.call(&session,&request.name,args,&operation)
        }).await;
        let response = match result {
            Ok(Ok(v)) if v["status"] != "failed" && v["status"] != "uncertain" => {
                CallToolResult::success(vec![ContentBlock::text(v.to_string())])
            }
            Ok(Ok(v)) => CallToolResult::error(vec![ContentBlock::text(v.to_string())]),
            Ok(Err(e)) => CallToolResult::error(vec![ContentBlock::text(format!("{e:#}"))]),
            Err(e) => CallToolResult::error(vec![ContentBlock::text(e.to_string())]),
        };
        Ok(response.into())
    }
}
pub fn serve(path: &Path) -> Result<()> {
    let client = crate::engine::Client::connect(path)?;
    let sessions = Arc::new(Mutex::new(BTreeSet::<String>::new()));
    let active = Arc::new(AtomicBool::new(true));
    let heartbeat = active.clone();
    let clients = sessions.clone();
    let connection = client.clone();
    std::thread::spawn(move || {
        while heartbeat.load(Ordering::SeqCst) {
            let _ = connection.request(json!({"action":"status"}));
            for session in clients.lock().unwrap().clone() {
                let _ = connection.request(json!({"action":"touch","session_id":session}));
            }
            std::thread::sleep(Duration::from_secs(10));
        }
    });
    let result = tokio::runtime::Runtime::new()?.block_on(async move {
        let server = Server { client, sessions }
            .serve(rmcp::transport::stdio())
            .await?;
        server.waiting().await?;
        Ok(())
    });
    active.store(false, Ordering::SeqCst);
    result
}
