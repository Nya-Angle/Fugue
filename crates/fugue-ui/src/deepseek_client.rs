//! # deepseek_client
//! DeepSeek SSE streaming client with `new_task` tool calling and sandbox interception.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessagePayload {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone)]
pub enum StreamEvent {
    ContentDelta(String),
    ToolCall {
        name: String,
        arguments_json: String,
    },
    Done,
    Error(String),
}

pub struct DeepSeekConfig {
    pub api_key: String,
    pub endpoint: String,
    pub model: String,
}

impl DeepSeekConfig {
    pub fn load_from_env() -> Result<Self, String> {
        let _ = dotenvy::dotenv();

        let api_key = std::env::var("DEEPSEEK_API_KEY")
            .map_err(|_| "DEEPSEEK_API_KEY not found in environment or .env".to_string())?;

        let base_url = std::env::var("DEEPSEEK_BASE_URL")
            .unwrap_or_else(|_| "https://api.deepseek.com".to_string());

        let endpoint = if base_url.ends_with("/chat/completions") {
            base_url
        } else if base_url.ends_with('/') {
            format!("{}chat/completions", base_url)
        } else {
            format!("{}/chat/completions", base_url)
        };

        Ok(Self {
            api_key,
            endpoint,
            model: "deepseek-chat".to_string(),
        })
    }
}

pub struct DeepSeekClient {
    client: reqwest::Client,
}

impl Default for DeepSeekClient {
    fn default() -> Self {
        Self::new()
    }
}

impl DeepSeekClient {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }

    pub async fn stream_chat(
        &self,
        config: &DeepSeekConfig,
        messages: Vec<ChatMessagePayload>,
        stop_signal: Arc<AtomicBool>,
        tx: mpsc::Sender<StreamEvent>,
    ) {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        if let Ok(val) = HeaderValue::from_str(&format!("Bearer {}", config.api_key.trim())) {
            headers.insert(AUTHORIZATION, val);
        }

        let mut full_messages = vec![ChatMessagePayload {
            role: "system".to_string(),
            content: fugue_omni::OMNI_SYSTEM_PROMPT.to_string(),
        }];
        full_messages.extend(messages);

        let tools_schema = serde_json::json!([
            {
                "type": "function",
                "function": {
                    "name": "new_task",
                    "description": "Fugue Omni sole meta-tool (AXIOM[03]) to compile and sandbox dispatch a new task.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "task_goal": {
                                "type": "string",
                                "description": "High-level goal for the task"
                            },
                            "agent_role": {
                                "type": "string",
                                "enum": ["TaskAgent", "ExecutionAgent"],
                                "description": "Assigned agent role"
                            },
                            "custom_instructions": {
                                "type": "string",
                                "description": "Custom prompt instructions for the task agent"
                            },
                            "token_budget": {
                                "type": "integer",
                                "description": "Token budget for the sub-agent"
                            }
                        },
                        "required": ["task_goal", "agent_role"]
                    }
                }
            }
        ]);

        let request_body = serde_json::json!({
            "model": config.model,
            "messages": full_messages,
            "tools": tools_schema,
            "stream": true
        });

        let response = match self
            .client
            .post(&config.endpoint)
            .headers(headers)
            .json(&request_body)
            .send()
            .await
        {
            Ok(res) => res,
            Err(err) => {
                let _ = tx.send(StreamEvent::Error(format!("Network request failed: {}", err))).await;
                return;
            }
        };

        if !response.status().is_success() {
            let status = response.status();
            let err_text = response.text().await.unwrap_or_default();
            let _ = tx.send(StreamEvent::Error(format!("API error ({}): {}", status, err_text))).await;
            return;
        }

        let mut stream = response.bytes_stream();
        let mut tool_name = String::new();
        let mut tool_args = String::new();
        let mut has_tool_call = false;
        let mut buffer = String::new();

        'stream_loop: while let Some(chunk_res) = stream.next().await {
            if stop_signal.load(Ordering::SeqCst) {
                break 'stream_loop;
            }

            let chunk = match chunk_res {
                Ok(bytes) => bytes,
                Err(err) => {
                    let _ = tx.send(StreamEvent::Error(format!("Stream read error: {}", err))).await;
                    return;
                }
            };

            buffer.push_str(&String::from_utf8_lossy(&chunk));

            while let Some(line_end) = buffer.find('\n') {
                let line = buffer[..line_end].trim().to_string();
                buffer.drain(..=line_end);

                if line.is_empty() || line.starts_with(':') {
                    continue;
                }

                if let Some(data) = line.strip_prefix("data: ") {
                    let data = data.trim();
                    if data == "[DONE]" {
                        break 'stream_loop;
                    }

                    if let Ok(json) = serde_json::from_str::<Value>(data) {
                        if let Some(choices) = json.get("choices").and_then(|c| c.as_array()) {
                            if let Some(choice) = choices.first() {
                                if let Some(delta) = choice.get("delta") {
                                    // Handle text content delta
                                    if let Some(content) = delta.get("content").and_then(|c| c.as_str()) {
                                        if !content.is_empty() {
                                            let _ = tx.send(StreamEvent::ContentDelta(content.to_string())).await;
                                        }
                                    }

                                    // Handle tool calls delta
                                    if let Some(tool_calls) = delta.get("tool_calls").and_then(|tc| tc.as_array()) {
                                        for tc in tool_calls {
                                            has_tool_call = true;
                                            if let Some(func) = tc.get("function") {
                                                if let Some(name) = func.get("name").and_then(|n| n.as_str()) {
                                                    tool_name.push_str(name);
                                                }
                                                if let Some(args) = func.get("arguments").and_then(|a| a.as_str()) {
                                                    tool_args.push_str(args);
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        if has_tool_call && !tool_name.is_empty() {
            let _ = tx.send(StreamEvent::ToolCall {
                name: tool_name,
                arguments_json: tool_args,
            }).await;
        }

        let _ = tx.send(StreamEvent::Done).await;
    }
}
