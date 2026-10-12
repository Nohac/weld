//! Stdio lifecycle for the window client's generated MCP tools.

use crate::windows::WindowClient;
use anyhow::Result;
use rmcp::{
    ErrorData, RoleServer, ServerHandler, ServiceExt,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ListToolsResult,
        PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool, ToolAnnotations,
    },
    service::RequestContext,
};
use std::{path::PathBuf, sync::Arc};
use tokio::sync::Semaphore;

struct WindowMcp {
    client: WindowClient,
    tools: Vec<Tool>,
    calls: Arc<Semaphore>,
}

impl ServerHandler for WindowMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("Inspect windows in the selected Weld session. Window IDs and inventory revisions belong to that running session.")
    }

    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(self.tools.clone()))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.tools.iter().find(|tool| tool.name == name).cloned()
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        if self.get_tool(&request.name).is_none() {
            return Err(ErrorData::invalid_params("unknown window tool", None));
        }
        let Ok(permit) = self.calls.clone().try_acquire_owned() else {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "window tool capacity reached",
            )])
            .into());
        };
        let client = self.client.clone();
        let result = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            client.mcp_call(
                &request.name,
                serde_json::Value::Object(request.arguments.unwrap_or_default()),
            )
        })
        .await
        .map_err(|error| ErrorData::internal_error(error.to_string(), None))?;
        Ok(match result {
            Ok(value) => CallToolResult::structured(value),
            Err(error) => CallToolResult::error(vec![ContentBlock::text(error)]),
        }
        .into())
    }
}

pub fn run(socket: PathBuf) -> Result<()> {
    let mut tools = WindowClient::mcp_tools()
        .into_iter()
        .map(serde_json::from_value)
        .collect::<Result<Vec<Tool>, _>>()?;
    for tool in &mut tools {
        tool.annotations = Some(ToolAnnotations::new().read_only(true));
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let service = WindowMcp {
            client: WindowClient::new(socket),
            tools,
            calls: Arc::new(Semaphore::new(8)),
        }
        .serve(rmcp::transport::stdio())
        .await?;
        service.waiting().await?;
        Ok(())
    })
}
