//! # deepseek_client
//! DeepSeek SSE streaming client with `new_task` tool calling and sandbox interception.

use std::time::Duration;
use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{mpsc, watch};

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
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { client }
    }

    pub async fn stream_chat(
        &self,
        config: &DeepSeekConfig,
        messages: Vec<ChatMessagePayload>,
        mut cancel_rx: watch::Receiver<bool>,
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

        if *cancel_rx.borrow() {
            return;
        }

        let response = tokio::select! {
            biased;
            _ = cancel_rx.changed(), if !*cancel_rx.borrow() => {
                return;
            }
            res = self
                .client
                .post(&config.endpoint)
                .headers(headers)
                .json(&request_body)
                .send() => match res {
                    Ok(res) => res,
                    Err(err) => {
                        let _ = tx.send(StreamEvent::Error(format!("Network request failed: {}", err))).await;
                        return;
                    }
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
        let mut byte_buf: Vec<u8> = Vec::new();
        let mut stream_done = false;

        'stream_loop: loop {
            if *cancel_rx.borrow() {
                return;
            }

            let next_chunk = tokio::select! {
                biased;
                _ = cancel_rx.changed(), if !*cancel_rx.borrow() => {
                    return;
                }
                item = stream.next() => item,
            };

            let Some(chunk_res) = next_chunk else {
                break 'stream_loop;
            };

            let chunk = match chunk_res {
                Ok(bytes) => bytes,
                Err(err) => {
                    let _ = tx.send(StreamEvent::Error(format!("Stream read error: {}", err))).await;
                    return;
                }
            };

            byte_buf.extend_from_slice(&chunk);

            while let Some(line_end) = byte_buf.iter().position(|&b| b == b'\n') {
                let raw_line: Vec<u8> = byte_buf.drain(..=line_end).collect();
                let line = String::from_utf8_lossy(&raw_line);
                let line = line.trim();

                if Self::process_sse_line(
                    line,
                    &mut tool_name,
                    &mut tool_args,
                    &mut has_tool_call,
                    &tx,
                )
                .await
                {
                    stream_done = true;
                    break 'stream_loop;
                }
            }
        }

        if !stream_done && !byte_buf.is_empty() {
            let line = String::from_utf8_lossy(&byte_buf);
            let line = line.trim();
            Self::process_sse_line(
                line,
                &mut tool_name,
                &mut tool_args,
                &mut has_tool_call,
                &tx,
            )
            .await;
        }

        if has_tool_call && !tool_name.is_empty() {
            let _ = tx.send(StreamEvent::ToolCall {
                name: tool_name,
                arguments_json: tool_args,
            }).await;
        }

        let _ = tx.send(StreamEvent::Done).await;
    }

    async fn process_sse_line(
        line: &str,
        tool_name: &mut String,
        tool_args: &mut String,
        has_tool_call: &mut bool,
        tx: &mpsc::Sender<StreamEvent>,
    ) -> bool {
        if line.is_empty() || line.starts_with(':') {
            return false;
        }

        if let Some(data) = line.strip_prefix("data:").map(str::trim) {
            if data == "[DONE]" {
                return true;
            }

            if let Ok(json) = serde_json::from_str::<Value>(data) {
                if let Some(choices) = json.get("choices").and_then(|c| c.as_array()) {
                    if let Some(choice) = choices.first() {
                        if let Some(delta) = choice.get("delta") {
                            if let Some(content) = delta.get("content").and_then(|c| c.as_str()) {
                                if !content.is_empty() {
                                    let _ = tx.send(StreamEvent::ContentDelta(content.to_string())).await;
                                }
                            }

                            if let Some(tool_calls) = delta.get("tool_calls").and_then(|tc| tc.as_array()) {
                                for tc in tool_calls {
                                    *has_tool_call = true;
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

        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn sse_line_parses_content_and_done_without_space() {
        let (tx, mut rx) = mpsc::channel(8);
        let mut tool_name = String::new();
        let mut tool_args = String::new();
        let mut has_tool_call = false;

        let done1 = DeepSeekClient::process_sse_line(
            r#"data:{"choices":[{"delta":{"content":"hello"}}]}"#,
            &mut tool_name,
            &mut tool_args,
            &mut has_tool_call,
            &tx,
        )
        .await;
        assert!(!done1);

        let done2 = DeepSeekClient::process_sse_line(
            "data: [DONE]",
            &mut tool_name,
            &mut tool_args,
            &mut has_tool_call,
            &tx,
        )
        .await;
        assert!(done2);

        match rx.recv().await {
            Some(StreamEvent::ContentDelta(s)) => assert_eq!(s, "hello"),
            other => panic!("unexpected event: {:?}", other),
        }
    }
}
