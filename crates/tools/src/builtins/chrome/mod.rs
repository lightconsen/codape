//! Chrome DevTools Protocol tools — browser automation via CDP.
//!
//! These tools connect to Chrome's remote debugging endpoint (port 9222)
//! and provide navigation, JavaScript execution, screenshot capture, and
//! DOM content extraction.
//!
//! All tools are **feature-gated** behind the `"chrome"` flag.

mod client;

pub use client::{ChromeClient, ChromeError};

use crate::context::{ToolError, ToolOutput};
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolInput, ToolTier};
use async_trait::async_trait;
use serde_json::Value;

/// Key for `ToolContext.extra` to access a shared ChromeClient.
pub const CHROME_CLIENT_KEY: &str = "chrome_client";

fn get_client(ctx: &ToolContext<'_>) -> Option<ChromeClient> {
    ctx.extra
        .get(CHROME_CLIENT_KEY)
        .and_then(|v| v.downcast_ref::<ChromeClient>().cloned())
}

/// The `ws_url` a tool was given, if any.
///
/// Every tool accepts it as an optional override. Omitting it drives the tab
/// the session already selected, which is the common case — making the model
/// echo an opaque id back on every call added a round trip per step and failed
/// as soon as it copied the wrong one.
fn optional_ws_url(args: &Value) -> Option<String> {
    args.get("ws_url")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

fn extract_string(args: &Value, key: &str) -> Result<String, ToolError> {
    args.get(key)
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or_else(|| ToolError::InvalidInput {
            message: format!("missing required field: {key}"),
        })
}

/// The largest screenshot forwarded to the model, matching `read_tool`.
const MAX_FORWARDED_IMAGE_BYTES: usize = 5 * 1024 * 1024;

/// The error every tool returns when Chrome is not listening on the CDP port.
fn not_connected() -> ToolError {
    ToolError::InvalidInput {
        message: "Chrome DevTools not connected. Start Chrome with \
                  --remote-debugging-port=9222"
            .into(),
    }
}

/// Map a CDP failure onto a tool error.
fn cdp_error(e: &ChromeError) -> ToolError {
    ToolError::InvalidInput {
        message: e.to_string(),
    }
}

/// List open Chrome tabs.
#[derive(Debug)]
pub struct ChromeListTabsTool;

#[async_trait]
impl Tool for ChromeListTabsTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "chrome_list_tabs".into(),
            tier: ToolTier::FeatureGated,
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {}
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "tabs": { "type": "array" }
                }
            }),
            requires_approval: false,
            is_enabled: true,
            is_read_only: true,
            is_concurrency_safe: true,
            interrupt_behavior: crate::tool::InterruptBehavior::RunToCompletion,
        }
    }

    async fn execute(
        &self,
        _input: ToolInput,
        ctx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let client = get_client(&ctx).unwrap_or_default();
        if !client.is_connected().await {
            return Err(not_connected());
        }
        let tabs = client.list_tabs().await.map_err(|e| cdp_error(&e))?;

        let json = serde_json::to_string_pretty(
            &tabs
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "id": t.id,
                        "title": t.title,
                        "url": t.url,
                    })
                })
                .collect::<Vec<_>>(),
        )
        .unwrap_or_default();

        Ok(ToolOutput {
            output: format!("Open tabs:\n{json}"),
            is_display: true,
            images: Vec::new(),
        })
    }
}

/// Navigate Chrome to a URL.
#[derive(Debug)]
pub struct ChromeNavigateTool;

#[async_trait]
impl Tool for ChromeNavigateTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "chrome_navigate".into(),
            tier: ToolTier::FeatureGated,
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["url"],
                "properties": {
                    "url": { "type": "string", "description": "URL to navigate to" }
                }
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "result": { "type": "string" }
                }
            }),
            requires_approval: false,
            is_enabled: true,
            is_read_only: false,
            is_concurrency_safe: false,
            interrupt_behavior: crate::tool::InterruptBehavior::RunToCompletion,
        }
    }

    async fn execute(
        &self,
        input: ToolInput,
        ctx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let url = extract_string(&input.arguments, "url")?;
        let client = get_client(&ctx).unwrap_or_default();
        if !client.is_connected().await {
            return Err(not_connected());
        }

        let ws_url = client.navigate(&url).await.map_err(|e| cdp_error(&e))?;

        Ok(ToolOutput {
            output: format!("Navigated to: {url}\nTab: {ws_url}"),
            is_display: true,
            images: Vec::new(),
        })
    }
}

/// Execute JavaScript in Chrome.
#[derive(Debug)]
pub struct ChromeExecuteJsTool;

#[async_trait]
impl Tool for ChromeExecuteJsTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "chrome_execute_js".into(),
            tier: ToolTier::FeatureGated,
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["expression"],
                "properties": {
                    "expression": { "type": "string", "description": "JavaScript expression to evaluate" },
                    "ws_url": { "type": "string", "description": "Optional tab to act on. Omit to use the session's current tab." }
                }
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "result": { "type": "string" }
                }
            }),
            requires_approval: true,
            is_enabled: true,
            is_read_only: false,
            is_concurrency_safe: false,
            interrupt_behavior: crate::tool::InterruptBehavior::RunToCompletion,
        }
    }

    async fn execute(
        &self,
        input: ToolInput,
        ctx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let expression = extract_string(&input.arguments, "expression")?;
        let ws_url = optional_ws_url(&input.arguments);
        let client = get_client(&ctx).unwrap_or_default();

        let result = client
            .execute_js(ws_url.as_deref(), &expression)
            .await
            .map_err(|e| cdp_error(&e))?;

        // A page that threw reports `exceptionDetails` instead of a value.
        // Passing the raw result through made a thrown error look like a
        // successful evaluation that returned `undefined`.
        if let Some(details) = result.get("exceptionDetails") {
            let description = details
                .get("exception")
                .and_then(|e| e.get("description"))
                .and_then(Value::as_str)
                .or_else(|| details.get("text").and_then(Value::as_str))
                .unwrap_or("the expression threw");
            return Err(ToolError::InvalidInput {
                message: format!("the expression threw: {description}"),
            });
        }

        let output = serde_json::to_string_pretty(&result.get("result").unwrap_or(&result))
            .unwrap_or_else(|_| result.to_string());

        Ok(ToolOutput {
            output,
            is_display: true,
            images: Vec::new(),
        })
    }
}

/// Capture a screenshot of a Chrome tab.
#[derive(Debug)]
pub struct ChromeScreenshotTool;

#[async_trait]
impl Tool for ChromeScreenshotTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "chrome_screenshot".into(),
            tier: ToolTier::FeatureGated,
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "ws_url": { "type": "string", "description": "Optional tab to capture. Omit to use the session's current tab." }
                }
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "base64_png": { "type": "string" }
                }
            }),
            requires_approval: false,
            is_enabled: true,
            is_read_only: true,
            is_concurrency_safe: false,
            interrupt_behavior: crate::tool::InterruptBehavior::RunToCompletion,
        }
    }

    async fn execute(
        &self,
        input: ToolInput,
        ctx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let ws_url = optional_ws_url(&input.arguments);
        let client = get_client(&ctx).unwrap_or_default();

        let b64 = client
            .screenshot(ws_url.as_deref())
            .await
            .map_err(|e| cdp_error(&e))?;

        // The picture is handed to the model as an image block, not as text.
        // Pasted into the tool result as base64 it was ~2 MB of characters the
        // model could neither see nor afford, so a screenshot answered nothing
        // about the page. `read_tool` forward path is the same one.
        let bytes =
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64.as_bytes())
                .map_err(|e| ToolError::Execution {
                    name: "chrome_screenshot".into(),
                    message: format!("the captured PNG was not valid base64: {e}"),
                })?;

        if bytes.len() > MAX_FORWARDED_IMAGE_BYTES {
            return Ok(ToolOutput {
                output: format!(
                    "[screenshot: PNG, {} bytes] not attached: exceeds the {} MiB image limit",
                    bytes.len(),
                    MAX_FORWARDED_IMAGE_BYTES / (1024 * 1024)
                ),
                is_display: true,
                images: Vec::new(),
            });
        }

        Ok(ToolOutput {
            output: format!("[screenshot: PNG, {} bytes]", bytes.len()),
            is_display: true,
            images: vec![codape_core::types::ImageBlock {
                media_type: "image/png".into(),
                data: b64,
            }],
        })
    }
}

/// Get the HTML content of a Chrome tab.
#[derive(Debug)]
pub struct ChromeGetContentTool;

#[async_trait]
impl Tool for ChromeGetContentTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "chrome_get_content".into(),
            tier: ToolTier::FeatureGated,
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "ws_url": { "type": "string", "description": "Optional tab to read. Omit to use the session's current tab." }
                }
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "content": { "type": "string" }
                }
            }),
            requires_approval: false,
            is_enabled: true,
            is_read_only: true,
            is_concurrency_safe: false,
            interrupt_behavior: crate::tool::InterruptBehavior::RunToCompletion,
        }
    }

    async fn execute(
        &self,
        input: ToolInput,
        ctx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let ws_url = optional_ws_url(&input.arguments);
        let client = get_client(&ctx).unwrap_or_default();

        let content = client
            .get_content(ws_url.as_deref())
            .await
            .map_err(|e| cdp_error(&e))?;

        // Convert HTML to readable text
        let text = crate::builtins::web_fetch_tool::html_to_markdown(&content);

        Ok(ToolOutput {
            output: text,
            is_display: true,
            images: Vec::new(),
        })
    }
}

/// Click an element on the page.
#[derive(Debug)]
pub struct ChromeClickTool;

#[async_trait]
impl Tool for ChromeClickTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "chrome_click".into(),
            tier: ToolTier::FeatureGated,
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["selector"],
                "properties": {
                    "selector": { "type": "string", "description": "CSS selector of the element to click" },
                    "ws_url": { "type": "string", "description": "Optional tab to act on. Omit to use the session's current tab." }
                }
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": { "clicked": { "type": "string" } }
            }),
            requires_approval: true,
            is_enabled: true,
            is_read_only: false,
            is_concurrency_safe: false,
            interrupt_behavior: crate::tool::InterruptBehavior::RunToCompletion,
        }
    }

    async fn execute(
        &self,
        input: ToolInput,
        ctx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let selector = extract_string(&input.arguments, "selector")?;
        let ws_url = optional_ws_url(&input.arguments);
        let client = get_client(&ctx).unwrap_or_default();

        client
            .click(ws_url.as_deref(), &selector)
            .await
            .map_err(|e| cdp_error(&e))?;

        Ok(ToolOutput {
            output: format!("Clicked: {selector}"),
            is_display: true,
            images: Vec::new(),
        })
    }
}

/// Type into a form field.
#[derive(Debug)]
pub struct ChromeTypeTool;

#[async_trait]
impl Tool for ChromeTypeTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "chrome_type".into(),
            tier: ToolTier::FeatureGated,
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["selector", "text"],
                "properties": {
                    "selector": { "type": "string", "description": "CSS selector of the input to type into" },
                    "text": { "type": "string", "description": "Text to insert" },
                    "ws_url": { "type": "string", "description": "Optional tab to act on. Omit to use the session's current tab." }
                }
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": { "typed": { "type": "string" } }
            }),
            requires_approval: true,
            is_enabled: true,
            is_read_only: false,
            is_concurrency_safe: false,
            interrupt_behavior: crate::tool::InterruptBehavior::RunToCompletion,
        }
    }

    async fn execute(
        &self,
        input: ToolInput,
        ctx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let selector = extract_string(&input.arguments, "selector")?;
        let text = extract_string(&input.arguments, "text")?;
        let ws_url = optional_ws_url(&input.arguments);
        let client = get_client(&ctx).unwrap_or_default();

        client
            .type_text(ws_url.as_deref(), &selector, &text)
            .await
            .map_err(|e| cdp_error(&e))?;

        Ok(ToolOutput {
            output: format!("Typed {} characters into {selector}", text.chars().count()),
            is_display: true,
            images: Vec::new(),
        })
    }
}

/// Scroll the page.
#[derive(Debug)]
pub struct ChromeScrollTool;

#[async_trait]
impl Tool for ChromeScrollTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "chrome_scroll".into(),
            tier: ToolTier::FeatureGated,
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["delta_y"],
                "properties": {
                    "delta_y": { "type": "number", "description": "Pixels to scroll; positive scrolls down" },
                    "ws_url": { "type": "string", "description": "Optional tab to act on. Omit to use the session's current tab." }
                }
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": { "scroll_y": { "type": "number" } }
            }),
            requires_approval: false,
            is_enabled: true,
            is_read_only: false,
            is_concurrency_safe: false,
            interrupt_behavior: crate::tool::InterruptBehavior::RunToCompletion,
        }
    }

    async fn execute(
        &self,
        input: ToolInput,
        ctx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let delta_y = input
            .arguments
            .get("delta_y")
            .and_then(Value::as_f64)
            .ok_or_else(|| ToolError::InvalidInput {
                message: "missing required field: delta_y".into(),
            })?;
        let ws_url = optional_ws_url(&input.arguments);
        let client = get_client(&ctx).unwrap_or_default();

        let result = client
            .scroll(ws_url.as_deref(), delta_y)
            .await
            .map_err(|e| cdp_error(&e))?;
        let scroll_y = result
            .get("result")
            .and_then(|r| r.get("value"))
            .and_then(Value::as_f64)
            .unwrap_or_default();

        Ok(ToolOutput {
            output: format!("Scrolled {delta_y}px; page is at y={scroll_y}"),
            is_display: true,
            images: Vec::new(),
        })
    }
}

/// Wait until the page has finished loading.
#[derive(Debug)]
pub struct ChromeWaitForLoadTool;

#[async_trait]
impl Tool for ChromeWaitForLoadTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "chrome_wait_for_load".into(),
            tier: ToolTier::FeatureGated,
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "ws_url": { "type": "string", "description": "Optional tab to wait on. Omit to use the session's current tab." }
                }
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {}
            }),
            requires_approval: false,
            is_enabled: true,
            is_read_only: true,
            is_concurrency_safe: false,
            interrupt_behavior: crate::tool::InterruptBehavior::RunToCompletion,
        }
    }

    async fn execute(
        &self,
        input: ToolInput,
        ctx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let ws_url = optional_ws_url(&input.arguments);
        let client = get_client(&ctx).unwrap_or_default();

        client
            .wait_for_load(ws_url.as_deref())
            .await
            .map_err(|e| cdp_error(&e))?;

        Ok(ToolOutput {
            output: "Page finished loading".into(),
            is_display: true,
            images: Vec::new(),
        })
    }
}
