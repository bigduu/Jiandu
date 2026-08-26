//! Independent JSON-RPC 2.0 + Streamable HTTP client used by conformance tests.
//!
//! The current session-mode server frames each POST response as one bounded SSE
//! event. This driver deliberately implements no GET stream, reconnect, resume,
//! `Last-Event-ID`, or keepalive lifecycle.

use reqwest::{Client, RequestBuilder, Response, StatusCode};
use serde_json::{Value, json};

const ACCEPT: &str = "application/json, text/event-stream";
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const PROTOCOL_VERSION: &str = "2025-11-25";
const SESSION_HEADER: &str = "mcp-session-id";
const VERSION_HEADER: &str = "mcp-protocol-version";

pub(crate) struct RawHttpDriver {
    client: Client,
    endpoint: String,
    token: String,
    session_id: String,
    next_id: u64,
    initialize: Value,
}

impl RawHttpDriver {
    pub(crate) async fn connect(endpoint: &str, token: &str) -> Result<Self, String> {
        let client = Client::new();
        let response = client
            .post(endpoint)
            .bearer_auth(token)
            .header("accept", ACCEPT)
            .header("content-type", "application/json")
            .json(&json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {
                        "name": "jiandu-raw-conformance",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                }
            }))
            .send()
            .await
            .map_err(|_| "raw initialize request failed".to_owned())?;
        if response.status() != StatusCode::OK {
            return Err(format!(
                "raw initialize returned HTTP {}",
                response.status().as_u16()
            ));
        }
        let session_id = response
            .headers()
            .get(SESSION_HEADER)
            .and_then(|value| value.to_str().ok())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| "raw initialize omitted Mcp-Session-Id".to_owned())?
            .to_owned();
        let envelope = decode_response(response).await?;
        let initialize = response_result(&envelope, 1)?.clone();

        let driver = Self {
            client,
            endpoint: endpoint.to_owned(),
            token: token.to_owned(),
            session_id,
            next_id: 2,
            initialize,
        };
        let initialized = driver
            .authenticated(driver.client.post(&driver.endpoint))
            .json(&json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized"
            }))
            .send()
            .await
            .map_err(|_| "raw initialized notification failed".to_owned())?;
        if initialized.status() != StatusCode::ACCEPTED {
            return Err(format!(
                "raw initialized notification returned HTTP {}",
                initialized.status().as_u16()
            ));
        }
        Ok(driver)
    }

    pub(crate) fn initialize_result(&self) -> &Value {
        &self.initialize
    }

    pub(crate) async fn list_tools(&mut self) -> Result<Value, String> {
        self.request("tools/list", json!({})).await
    }

    pub(crate) async fn list_resources(&mut self) -> Result<Value, String> {
        self.request("resources/list", json!({})).await
    }

    pub(crate) async fn list_resource_templates(&mut self) -> Result<Value, String> {
        self.request("resources/templates/list", json!({})).await
    }

    pub(crate) async fn read_resource(&mut self, uri: &str) -> Result<Value, String> {
        self.request("resources/read", json!({ "uri": uri })).await
    }

    pub(crate) async fn read_resource_error(&mut self, uri: &str) -> Result<Value, String> {
        self.request_error("resources/read", json!({ "uri": uri }))
            .await
    }

    pub(crate) async fn call_tool(
        &mut self,
        name: &str,
        arguments: Value,
    ) -> Result<Value, String> {
        self.request(
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        )
        .await
    }

    /// Submit one tool call, wait until the server has accepted the response,
    /// then drop its unread body. The resilience suite independently observes
    /// the durable mutation before stopping the daemon, so this models a lost
    /// application acknowledgement without relying on timing or test hooks.
    pub(crate) async fn call_tool_and_drop_response(
        &mut self,
        name: &str,
        arguments: Value,
    ) -> Result<(), String> {
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or_else(|| "raw request ID exhausted".to_owned())?;
        let response = self
            .authenticated(self.client.post(&self.endpoint))
            .json(&json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "tools/call",
                "params": { "name": name, "arguments": arguments }
            }))
            .send()
            .await
            .map_err(|_| "raw MCP request failed before response headers".to_owned())?;
        if response.status() != StatusCode::OK {
            return Err(format!(
                "raw MCP request returned HTTP {}",
                response.status().as_u16()
            ));
        }
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| "raw MCP response omitted content type".to_owned())?;
        if !content_type.starts_with("application/json")
            && !content_type.starts_with("text/event-stream")
        {
            return Err("raw MCP response used an unsupported content type".to_owned());
        }
        drop(response);
        Ok(())
    }

    pub(crate) async fn close(self) -> Result<(), String> {
        let response = self
            .authenticated(self.client.delete(&self.endpoint))
            .send()
            .await
            .map_err(|_| "raw session close failed".to_owned())?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(format!(
                "raw session close returned HTTP {}",
                response.status().as_u16()
            ))
        }
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let (envelope, id) = self.request_envelope(method, params).await?;
        response_result(&envelope, id).cloned()
    }

    async fn request_error(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let (envelope, id) = self.request_envelope(method, params).await?;
        response_error(&envelope, id).cloned()
    }

    async fn request_envelope(
        &mut self,
        method: &str,
        params: Value,
    ) -> Result<(Value, u64), String> {
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or_else(|| "raw request ID exhausted".to_owned())?;
        let response = self
            .authenticated(self.client.post(&self.endpoint))
            .json(&json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": method,
                "params": params
            }))
            .send()
            .await
            .map_err(|_| "raw MCP request failed".to_owned())?;
        if response.status() != StatusCode::OK {
            return Err(format!(
                "raw MCP request returned HTTP {}",
                response.status().as_u16()
            ));
        }
        let envelope = decode_response(response).await?;
        Ok((envelope, id))
    }

    fn authenticated(&self, request: RequestBuilder) -> RequestBuilder {
        request
            .bearer_auth(&self.token)
            .header("accept", ACCEPT)
            .header("content-type", "application/json")
            .header(SESSION_HEADER, &self.session_id)
            .header(VERSION_HEADER, PROTOCOL_VERSION)
    }
}

async fn decode_response(mut response: Response) -> Result<Value, String> {
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| "raw MCP response omitted content type".to_owned())?
        .to_owned();
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "raw MCP response body unavailable".to_owned())?
    {
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err("raw MCP response exceeded the fixture bound".to_owned());
        }
        body.extend_from_slice(&chunk);
    }

    if content_type.starts_with("application/json") {
        return serde_json::from_slice(&body)
            .map_err(|_| "raw MCP response contained invalid JSON".to_owned());
    }
    if !content_type.starts_with("text/event-stream") {
        return Err("raw MCP response used an unsupported content type".to_owned());
    }
    decode_single_response_event(&body)
}

fn decode_single_response_event(body: &[u8]) -> Result<Value, String> {
    let text =
        std::str::from_utf8(body).map_err(|_| "raw MCP event stream was not UTF-8".to_owned())?;
    let mut response = None;
    let mut priming_events = 0_u8;
    for event in text.split("\n\n").filter(|event| !event.is_empty()) {
        let mut data = Vec::new();
        for line in event.lines() {
            if let Some(value) = line.strip_prefix("data:") {
                data.push(value.strip_prefix(' ').unwrap_or(value));
            } else if !line.starts_with("id:") && !line.starts_with("retry:") {
                return Err("raw MCP event stream contained an unsupported field".to_owned());
            }
        }
        let payload = data.join("\n");
        if payload.is_empty() {
            priming_events = priming_events.saturating_add(1);
            if priming_events > 1 {
                return Err("raw MCP event stream contained multiple priming events".to_owned());
            }
            continue;
        }
        if response.is_some() {
            return Err("raw MCP POST returned multiple JSON-RPC response events".to_owned());
        }
        response = Some(
            serde_json::from_str(&payload)
                .map_err(|_| "raw MCP response event contained invalid JSON".to_owned())?,
        );
    }
    response.ok_or_else(|| "raw MCP POST omitted its JSON-RPC response event".to_owned())
}

fn response_result(envelope: &Value, id: u64) -> Result<&Value, String> {
    validate_response_envelope(envelope, id)?;
    if envelope.get("error").is_some() {
        return Err("raw MCP response contained a JSON-RPC error".to_owned());
    }
    envelope
        .get("result")
        .ok_or_else(|| "raw MCP response omitted result".to_owned())
}

fn response_error(envelope: &Value, id: u64) -> Result<&Value, String> {
    validate_response_envelope(envelope, id)?;
    if envelope.get("result").is_some() {
        return Err("raw MCP response unexpectedly succeeded".to_owned());
    }
    let error = envelope
        .get("error")
        .ok_or_else(|| "raw MCP response omitted error".to_owned())?;
    if !error.is_object() {
        return Err("raw MCP response used an invalid error member".to_owned());
    }
    Ok(error)
}

fn validate_response_envelope(envelope: &Value, id: u64) -> Result<(), String> {
    if envelope.get("jsonrpc") != Some(&Value::String("2.0".to_owned()))
        || envelope.get("id") != Some(&Value::Number(id.into()))
    {
        return Err("raw MCP response had a mismatched JSON-RPC envelope".to_owned());
    }
    Ok(())
}
