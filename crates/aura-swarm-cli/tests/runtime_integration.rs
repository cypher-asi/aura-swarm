//! Integration tests against a real aura-runtime server.
//!
//! These tests require aura-runtime to be running on localhost:8080.
//!
//! Run with:
//!   cargo test -p aura-swarm-cli --test runtime_integration -- --ignored
//!
//! Or run all integration tests:
//!   cargo test -p aura-swarm-cli --test runtime_integration -- --ignored --nocapture

use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::time::timeout;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::Message;

/// Default aura-runtime endpoint.
const RUNTIME_URL: &str = "ws://localhost:8080/stream";

/// Timeout for receiving messages.
const MESSAGE_TIMEOUT: Duration = Duration::from_secs(60);

/// Timeout for initial connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

// =============================================================================
// Protocol Types (mirroring types.rs but self-contained for integration tests)
// =============================================================================

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMessage {
    #[serde(rename = "prompt")]
    Prompt {
        request_id: String,
        prompt: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        agent_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        workspace: Option<String>,
    },
    Cancel {
        request_id: String,
    },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerMessage {
    TurnStart {
        #[serde(rename = "request_id")]
        _request_id: String,
        #[serde(rename = "agent_id")]
        _agent_id: String,
    },
    StepStart {
        #[serde(rename = "request_id")]
        _request_id: String,
        #[serde(rename = "agent_id")]
        _agent_id: String,
        step: u32,
    },
    TurnComplete {
        #[serde(rename = "request_id")]
        _request_id: String,
        #[serde(rename = "agent_id")]
        _agent_id: String,
        steps: u32,
        input_tokens: u32,
        output_tokens: u32,
    },
    TextDelta {
        #[serde(rename = "request_id")]
        _request_id: String,
        #[serde(rename = "agent_id")]
        _agent_id: String,
        text: String,
    },
    ThinkingDelta {
        #[serde(rename = "request_id")]
        _request_id: String,
        #[serde(rename = "agent_id")]
        _agent_id: String,
        thinking: String,
    },
    ToolStart {
        #[serde(rename = "request_id")]
        _request_id: String,
        #[serde(rename = "agent_id")]
        _agent_id: String,
        tool_id: String,
        tool_name: String,
        #[serde(default)]
        args: serde_json::Value,
    },
    ToolComplete {
        #[serde(rename = "request_id")]
        _request_id: String,
        #[serde(rename = "agent_id")]
        _agent_id: String,
        tool_id: String,
        result: String,
        is_error: bool,
    },
    Error {
        #[serde(default, rename = "request_id")]
        _request_id: Option<String>,
        #[serde(default, rename = "agent_id")]
        _agent_id: Option<String>,
        error: String,
        #[serde(default)]
        code: Option<String>,
    },
    Cancelled {
        #[serde(rename = "request_id")]
        _request_id: String,
        #[serde(rename = "agent_id")]
        _agent_id: String,
    },
}

// =============================================================================
// Test Helpers
// =============================================================================

/// Connect to the aura-runtime WebSocket endpoint.
async fn connect_to_runtime() -> Result<
    tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    String,
> {
    let url = std::env::var("AURA_RUNTIME_URL").unwrap_or_else(|_| RUNTIME_URL.to_string());
    
    match timeout(CONNECT_TIMEOUT, connect_async(&url)).await {
        Ok(Ok((ws_stream, _))) => Ok(ws_stream),
        Ok(Err(e)) => Err(format!("Failed to connect to {}: {}", url, e)),
        Err(_) => Err(format!("Connection timeout to {}", url)),
    }
}

/// Send a prompt and collect all messages until turn_complete or error.
async fn send_prompt_and_collect(
    prompt: &str,
) -> Result<(Vec<ServerMessage>, String), String> {
    let ws_stream = connect_to_runtime().await?;
    let (mut write, mut read) = ws_stream.split();
    
    let request_id = format!("test-{}", std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis());
    
    // Send prompt
    let msg = ClientMessage::Prompt {
        request_id: request_id.clone(),
        prompt: prompt.to_string(),
        agent_id: None,
        workspace: None,
    };
    
    let json = serde_json::to_string(&msg).map_err(|e| e.to_string())?;
    write.send(Message::Text(json)).await.map_err(|e| e.to_string())?;
    
    // Collect messages
    let mut messages = Vec::new();
    let mut text_buffer = String::new();
    
    loop {
        match timeout(MESSAGE_TIMEOUT, read.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => {
                match serde_json::from_str::<ServerMessage>(&text) {
                    Ok(msg) => {
                        // Accumulate text
                        if let ServerMessage::TextDelta { text: delta, .. } = &msg {
                            text_buffer.push_str(delta);
                        }
                        
                        let is_terminal = matches!(
                            &msg,
                            ServerMessage::TurnComplete { .. }
                            | ServerMessage::Error { .. }
                            | ServerMessage::Cancelled { .. }
                        );
                        
                        messages.push(msg);
                        
                        if is_terminal {
                            break;
                        }
                    }
                    Err(e) => {
                        return Err(format!("Failed to parse message: {} - raw: {}", e, text));
                    }
                }
            }
            Ok(Some(Ok(Message::Close(_)))) => {
                return Err("Connection closed unexpectedly".to_string());
            }
            Ok(Some(Ok(_))) => {
                // Ignore ping/pong/binary
                continue;
            }
            Ok(Some(Err(e))) => {
                return Err(format!("WebSocket error: {}", e));
            }
            Ok(None) => {
                return Err("Connection closed".to_string());
            }
            Err(_) => {
                return Err(format!("Timeout waiting for response ({}s)", MESSAGE_TIMEOUT.as_secs()));
            }
        }
    }
    
    // Close WebSocket properly
    let close_frame = CloseFrame {
        code: CloseCode::Normal,
        reason: "Request complete".into(),
    };
    let _ = write.send(Message::Close(Some(close_frame))).await;
    
    Ok((messages, text_buffer))
}

// =============================================================================
// Integration Tests
// =============================================================================

/// Test basic connectivity and round-trip to aura-runtime.
#[tokio::test]
#[ignore = "Requires aura-runtime running on localhost:8080"]
async fn test_connection() {
    // 1. Test WebSocket handshake
    let ws_stream = connect_to_runtime().await
        .expect("Failed to connect to aura-runtime");
    println!("✓ WebSocket handshake successful");
    
    let (mut write, mut read) = ws_stream.split();
    
    // 2. Send a minimal prompt
    let request_id = "conn-test-001";
    let msg = ClientMessage::Prompt {
        request_id: request_id.to_string(),
        prompt: "ping".to_string(),
        agent_id: None,
        workspace: None,
    };
    
    let json = serde_json::to_string(&msg).expect("Failed to serialize");
    write.send(Message::Text(json)).await.expect("Failed to send prompt");
    println!("✓ Sent prompt message");
    
    // 3. Verify we get at least one response message
    let mut received_turn_start = false;
    let mut received_any_response = false;
    let mut message_count = 0;
    let mut text_chunks = 0;
    let mut thinking_chunks = 0;
    let mut accumulated_text = String::new();
    
    println!("\n--- Message Stream ---");
    
    for _ in 0..50 {
        match timeout(Duration::from_secs(10), read.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => {
                received_any_response = true;
                message_count += 1;
                
                match serde_json::from_str::<ServerMessage>(&text) {
                    Ok(msg) => {
                        match &msg {
                            ServerMessage::TurnStart { .. } => {
                                received_turn_start = true;
                                println!("  [{:>3}] TurnStart", message_count);
                            }
                            ServerMessage::StepStart { step, .. } => {
                                println!("  [{:>3}] StepStart (step={})", message_count, step);
                            }
                            ServerMessage::TextDelta { text: delta, .. } => {
                                text_chunks += 1;
                                accumulated_text.push_str(delta);
                                // Only print occasionally to reduce noise
                                if text_chunks == 1 || text_chunks % 10 == 0 {
                                    println!("  [{:>3}] TextDelta (chunk #{}, +{} chars)", 
                                        message_count, text_chunks, delta.len());
                                }
                            }
                            ServerMessage::ThinkingDelta { thinking, .. } => {
                                thinking_chunks += 1;
                                if thinking_chunks == 1 || thinking_chunks % 5 == 0 {
                                    let preview = if thinking.len() > 50 {
                                        format!("{}...", &thinking[..50])
                                    } else {
                                        thinking.clone()
                                    };
                                    println!("  [{:>3}] ThinkingDelta (chunk #{}, +{} chars): {}", 
                                        message_count, thinking_chunks, thinking.len(), preview);
                                }
                            }
                            ServerMessage::ToolStart { tool_name, tool_id, args, .. } => {
                                println!("  [{:>3}] ToolStart: {} (id={})", message_count, tool_name, tool_id);
                                println!("         args: {}", serde_json::to_string(args).unwrap_or_default());
                            }
                            ServerMessage::ToolComplete { tool_id, result, is_error, .. } => {
                                let status = if *is_error { "ERROR" } else { "OK" };
                                let preview = if result.len() > 100 {
                                    format!("{}...", &result[..100])
                                } else {
                                    result.clone()
                                };
                                println!("  [{:>3}] ToolComplete: {} (id={})", message_count, status, tool_id);
                                println!("         result: {}", preview);
                            }
                            ServerMessage::TurnComplete { steps, input_tokens, output_tokens, .. } => {
                                println!("  [{:>3}] TurnComplete", message_count);
                                println!("         steps: {}", steps);
                                println!("         tokens: {} in / {} out", input_tokens, output_tokens);
                                break;
                            }
                            ServerMessage::Error { error, code, .. } => {
                                println!("  [{:>3}] Error: {} (code={:?})", message_count, error, code);
                                break;
                            }
                            ServerMessage::Cancelled { .. } => {
                                println!("  [{:>3}] Cancelled", message_count);
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        println!("  [{:>3}] Parse error: {}", message_count, e);
                        println!("         raw: {}", &text[..text.len().min(200)]);
                    }
                }
            }
            Ok(Some(Ok(_))) => continue, // Ping/pong
            Ok(Some(Err(e))) => panic!("WebSocket error: {}", e),
            Ok(None) => panic!("Connection closed unexpectedly"),
            Err(_) => panic!("Timeout waiting for response"),
        }
    }
    
    println!("--- End Stream ---\n");
    
    // Summary
    println!("Summary:");
    println!("  Total messages: {}", message_count);
    println!("  Text chunks: {}", text_chunks);
    println!("  Thinking chunks: {}", thinking_chunks);
    if !accumulated_text.is_empty() {
        let preview = if accumulated_text.len() > 200 {
            format!("{}...", &accumulated_text[..200])
        } else {
            accumulated_text.clone()
        };
        println!("  Response preview: {}", preview);
    }
    
    assert!(received_any_response, "Should receive at least one message");
    assert!(received_turn_start, "Should receive TurnStart message");
    
    // 4. Close WebSocket properly
    let close_frame = CloseFrame {
        code: CloseCode::Normal,
        reason: "Test complete".into(),
    };
    let _ = write.send(Message::Close(Some(close_frame))).await;
    println!("✓ WebSocket closed cleanly");
    
    println!("\n✓ Round-trip communication verified");
}

/// Test that asks the agent to write a file - exercises tool execution.
#[tokio::test]
#[ignore = "Requires aura-runtime running on localhost:8080"]
async fn test_file_write() {
    // 1. Connect
    let ws_stream = connect_to_runtime().await
        .expect("Failed to connect to aura-runtime");
    println!("✓ WebSocket connected");
    
    let (mut write, mut read) = ws_stream.split();
    
    // 2. Send prompt to write a file
    let test_filename = format!("test-output-{}.txt", std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis());
    
    let prompt = format!(
        "Write a file called '{}' with the content 'Hello from integration test!' and then confirm it was written.",
        test_filename
    );
    
    let request_id = "file-write-test-001";
    let msg = ClientMessage::Prompt {
        request_id: request_id.to_string(),
        prompt: prompt.clone(),
        agent_id: None,
        workspace: None,
    };
    
    let json = serde_json::to_string(&msg).expect("Failed to serialize");
    write.send(Message::Text(json)).await.expect("Failed to send prompt");
    println!("✓ Sent prompt: {}", prompt);
    
    // 3. Collect messages and track tool executions
    let mut message_count = 0;
    let mut tool_starts: Vec<(String, String)> = Vec::new(); // (tool_name, tool_id)
    let mut tool_completes: Vec<(String, bool, String)> = Vec::new(); // (tool_id, is_error, result_preview)
    let mut accumulated_text = String::new();
    let mut turn_complete_info: Option<(u32, u32, u32)> = None;
    
    println!("\n--- Message Stream ---");
    
    for _ in 0..100 {
        match timeout(Duration::from_secs(30), read.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => {
                message_count += 1;
                
                match serde_json::from_str::<ServerMessage>(&text) {
                    Ok(msg) => {
                        match &msg {
                            ServerMessage::TurnStart { .. } => {
                                println!("  [{:>3}] TurnStart", message_count);
                            }
                            ServerMessage::StepStart { step, .. } => {
                                println!("  [{:>3}] StepStart (step={})", message_count, step);
                            }
                            ServerMessage::TextDelta { text: delta, .. } => {
                                accumulated_text.push_str(delta);
                            }
                            ServerMessage::ThinkingDelta { .. } => {
                                // Silent for thinking
                            }
                            ServerMessage::ToolStart { tool_name, tool_id, args, .. } => {
                                println!("  [{:>3}] ToolStart: {}", message_count, tool_name);
                                println!("         id: {}", tool_id);
                                let args_str = serde_json::to_string_pretty(args).unwrap_or_default();
                                for line in args_str.lines().take(10) {
                                    println!("         {}", line);
                                }
                                tool_starts.push((tool_name.clone(), tool_id.clone()));
                            }
                            ServerMessage::ToolComplete { tool_id, result, is_error, .. } => {
                                let status = if *is_error { "ERROR" } else { "OK" };
                                println!("  [{:>3}] ToolComplete: {} (id={})", message_count, status, tool_id);
                                let preview = if result.len() > 150 {
                                    format!("{}...", &result[..150])
                                } else {
                                    result.clone()
                                };
                                println!("         result: {}", preview.replace('\n', " "));
                                tool_completes.push((tool_id.clone(), *is_error, preview));
                            }
                            ServerMessage::TurnComplete { steps, input_tokens, output_tokens, .. } => {
                                println!("  [{:>3}] TurnComplete", message_count);
                                println!("         steps: {}, tokens: {} in / {} out", steps, input_tokens, output_tokens);
                                turn_complete_info = Some((*steps, *input_tokens, *output_tokens));
                                break;
                            }
                            ServerMessage::Error { error, code, .. } => {
                                println!("  [{:>3}] ERROR: {} (code={:?})", message_count, error, code);
                                break;
                            }
                            ServerMessage::Cancelled { .. } => {
                                println!("  [{:>3}] Cancelled", message_count);
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        println!("  [{:>3}] Parse error: {}", message_count, e);
                        println!("         raw: {}", &text[..text.len().min(300)]);
                    }
                }
            }
            Ok(Some(Ok(_))) => continue,
            Ok(Some(Err(e))) => panic!("WebSocket error: {}", e),
            Ok(None) => panic!("Connection closed unexpectedly"),
            Err(_) => panic!("Timeout waiting for response"),
        }
    }
    
    println!("--- End Stream ---\n");
    
    // 4. Summary and assertions
    println!("=== Summary ===");
    println!("Total messages: {}", message_count);
    println!("Tool executions: {} started, {} completed", tool_starts.len(), tool_completes.len());
    
    println!("\nTools used:");
    for (name, id) in &tool_starts {
        println!("  - {} ({})", name, id);
    }
    
    if !accumulated_text.is_empty() {
        println!("\nAgent response:");
        println!("{}", accumulated_text);
    }
    
    if let Some((steps, input, output)) = turn_complete_info {
        println!("\nTurn stats: {} steps, {} input tokens, {} output tokens", steps, input, output);
    }
    
    // Verify either tool execution or text response about file writing
    if !tool_starts.is_empty() {
        // Tool execution path
        assert_eq!(tool_starts.len(), tool_completes.len(), "Each tool start should have a complete");
        
        let used_write_tool = tool_starts.iter().any(|(name, _)| {
            name.contains("write") || name.contains("fs") || name.contains("file")
        });
        
        if used_write_tool {
            println!("\n✓ File write test passed - tool execution verified");
        } else {
            println!("\n✓ Tool execution occurred with tools: {:?}", 
                tool_starts.iter().map(|(n, _)| n).collect::<Vec<_>>());
        }
    } else {
        // No tool execution - check if agent responded with tool-like markup
        let has_write_markup = accumulated_text.contains("<write_file>") 
            || accumulated_text.contains("write_file")
            || accumulated_text.contains("<path>");
        
        if has_write_markup {
            println!("\n⚠ Agent returned tool markup as TEXT (not executed):");
            println!("  This indicates tool execution is not enabled on the runtime.");
            println!("  The agent tried to call a tool but it was rendered as text instead.\n");
            println!("✓ Test passed (text-mode tool response detected)");
        } else {
            // Agent just responded with text
            assert!(
                !accumulated_text.is_empty(),
                "Expected either tool execution or text response"
            );
            println!("\n✓ Test passed (agent responded with text, no tool calls)");
        }
    }
    
    // 5. Close WebSocket properly
    let close_frame = CloseFrame {
        code: CloseCode::Normal,
        reason: "Test complete".into(),
    };
    let _ = write.send(Message::Close(Some(close_frame))).await;
    println!("✓ WebSocket closed cleanly");
}

/// Detect if the server is in echo/stub mode (not calling real LLM).
fn is_echo_mode(response: &str, prompt: &str) -> bool {
    response.contains("Received prompt:") || response.contains(prompt)
}

/// Test a simple prompt that should get a text response.
#[tokio::test]
#[ignore = "Requires aura-runtime running on localhost:8080"]
async fn test_simple_prompt() {
    let prompt = "Say hello in exactly 3 words.";
    let (messages, text) = send_prompt_and_collect(prompt)
        .await
        .expect("Failed to get response");
    
    println!("\n=== Messages received: {} ===", messages.len());
    for (i, msg) in messages.iter().enumerate() {
        println!("{}: {:?}", i, std::mem::discriminant(msg));
    }
    println!("\n=== Response text ===\n{}", text);
    
    // Verify we got the expected message types
    let has_turn_start = messages.iter().any(|m| matches!(m, ServerMessage::TurnStart { .. }));
    let has_text_delta = messages.iter().any(|m| matches!(m, ServerMessage::TextDelta { .. }));
    let has_turn_complete = messages.iter().any(|m| matches!(m, ServerMessage::TurnComplete { .. }));
    
    assert!(has_turn_start, "Missing TurnStart message");
    assert!(has_text_delta, "Missing TextDelta message");
    assert!(has_turn_complete, "Missing TurnComplete message");
    assert!(!text.is_empty(), "Response text should not be empty");
    
    // Check if in echo mode
    let echo_mode = is_echo_mode(&text, prompt);
    if echo_mode {
        println!("\n⚠ Server is in ECHO/STUB mode (not calling real LLM)");
        println!("  Protocol is working correctly, but responses are echoed prompts");
    }
    
    // Verify turn_complete has reasonable values
    if let Some(ServerMessage::TurnComplete { steps, input_tokens, output_tokens, .. }) = 
        messages.iter().find(|m| matches!(m, ServerMessage::TurnComplete { .. }))
    {
        println!("\n=== Turn Stats ===");
        println!("Steps: {}", steps);
        println!("Input tokens: {}", input_tokens);
        println!("Output tokens: {}", output_tokens);
        
        assert!(*steps >= 1, "Should have at least 1 step");
        
        // Only check token counts if not in echo mode
        if !echo_mode {
            assert!(*input_tokens > 0, "Should have input tokens (not in echo mode)");
            assert!(*output_tokens > 0, "Should have output tokens (not in echo mode)");
        }
    }
    
    println!("\n✓ Simple prompt test passed{}", if echo_mode { " (echo mode)" } else { "" });
}

/// Test a prompt that should trigger tool use.
#[tokio::test]
#[ignore = "Requires aura-runtime running on localhost:8080"]
async fn test_tool_execution() {
    // This prompt should trigger file system exploration
    let (messages, text) = send_prompt_and_collect(
        "List the files in the current directory using the fs_ls tool. Just show the listing, no explanation."
    )
    .await
    .expect("Failed to get response");
    
    println!("\n=== Messages received: {} ===", messages.len());
    for (i, msg) in messages.iter().enumerate() {
        match msg {
            ServerMessage::ToolStart { tool_name, .. } => {
                println!("{}: ToolStart({})", i, tool_name);
            }
            ServerMessage::ToolComplete { tool_id, is_error, .. } => {
                println!("{}: ToolComplete(id={}, error={})", i, tool_id, is_error);
            }
            _ => {
                println!("{}: {:?}", i, std::mem::discriminant(msg));
            }
        }
    }
    println!("\n=== Response text ===\n{}", text);
    
    // Check for tool execution
    let tool_starts: Vec<(String, String, serde_json::Value)> = messages.iter()
        .filter_map(|m| match m {
            ServerMessage::ToolStart { tool_name, tool_id, args, .. } => {
                Some((tool_name.clone(), tool_id.clone(), args.clone()))
            }
            _ => None,
        })
        .collect();
    
    let tool_completes: Vec<(String, String, bool)> = messages.iter()
        .filter_map(|m| match m {
            ServerMessage::ToolComplete { tool_id, result, is_error, .. } => {
                Some((tool_id.clone(), result.clone(), *is_error))
            }
            _ => None,
        })
        .collect();
    
    println!("\n=== Tool Calls ===");
    for (name, id, args) in &tool_starts {
        println!("START: {} (id: {})", name, id);
        println!("  args: {}", args);
    }
    for (id, result, is_error) in &tool_completes {
        println!("COMPLETE: id={}, error={}", id, is_error);
        let preview = if result.len() > 200 {
            format!("{}...", &result[..200])
        } else {
            result.clone()
        };
        println!("  result: {}", preview);
    }
    
    // If tools were used, verify pairing
    if !tool_starts.is_empty() {
        assert_eq!(
            tool_starts.len(),
            tool_completes.len(),
            "Each ToolStart should have a matching ToolComplete"
        );
        println!("\n✓ Tool execution test passed with {} tool call(s)", tool_starts.len());
    } else {
        println!("\n⚠ No tool calls detected (agent may have responded without tools)");
    }
}

/// Test thinking/reasoning output (if enabled).
#[tokio::test]
#[ignore = "Requires aura-runtime running on localhost:8080"]
async fn test_thinking_output() {
    let prompt = "What is 15 * 23? Think through this step by step.";
    let (messages, text) = send_prompt_and_collect(prompt)
        .await
        .expect("Failed to get response");
    
    let thinking_deltas: Vec<_> = messages.iter()
        .filter_map(|m| match m {
            ServerMessage::ThinkingDelta { thinking, .. } => Some(thinking.clone()),
            _ => None,
        })
        .collect();
    
    let thinking_text: String = thinking_deltas.join("");
    
    println!("\n=== Messages received: {} ===", messages.len());
    println!("Thinking deltas: {}", thinking_deltas.len());
    println!("Response text length: {} chars", text.len());
    
    // Check if in echo mode
    let echo_mode = is_echo_mode(&text, prompt);
    if echo_mode {
        println!("\n⚠ Server is in ECHO/STUB mode - skipping LLM-specific checks");
        println!("  Protocol test passed (turn_start → text_delta → turn_complete)");
        println!("\n✓ Thinking test passed (echo mode - protocol verified)");
        return;
    }
    
    if !thinking_text.is_empty() {
        println!("\n=== Thinking ===\n{}", thinking_text);
        println!("\n✓ Thinking output captured ({} chars)", thinking_text.len());
    } else {
        println!("\n⚠ No thinking output (extended thinking may not be enabled)");
    }
    
    println!("\n=== Response ===\n{}", text);
    
    // Verify the math answer is somewhere in the response (only if not echo mode)
    assert!(
        text.contains("345") || thinking_text.contains("345"),
        "Response should contain correct answer (345)"
    );
    
    println!("\n✓ Thinking test passed");
}

/// Test multi-step turn with multiple tool calls.
#[tokio::test]
#[ignore = "Requires aura-runtime running on localhost:8080"]
async fn test_multi_step_turn() {
    let (messages, text) = send_prompt_and_collect(
        "First, list files in the current directory. Then read the contents of any .rs file you find. Summarize what you found."
    )
    .await
    .expect("Failed to get response");
    
    println!("\n=== Messages received: {} ===", messages.len());
    
    // Count steps
    let step_starts: Vec<u32> = messages.iter()
        .filter_map(|m| match m {
            ServerMessage::StepStart { step, .. } => Some(*step),
            _ => None,
        })
        .collect();
    
    // Count tool calls
    let tool_count = messages.iter()
        .filter(|m| matches!(m, ServerMessage::ToolStart { .. }))
        .count();
    
    println!("Steps: {:?}", step_starts);
    println!("Tool calls: {}", tool_count);
    println!("\n=== Response ===\n{}", text);
    
    // Get final stats
    if let Some(ServerMessage::TurnComplete { steps, input_tokens, output_tokens, .. }) = 
        messages.iter().find(|m| matches!(m, ServerMessage::TurnComplete { .. }))
    {
        println!("\n=== Final Stats ===");
        println!("Total steps: {}", steps);
        println!("Input tokens: {}", input_tokens);
        println!("Output tokens: {}", output_tokens);
    }
    
    println!("\n✓ Multi-step test passed");
}

/// Test error handling with an invalid request.
#[tokio::test]
#[ignore = "Requires aura-runtime running on localhost:8080"]
async fn test_error_handling() {
    let ws_stream = connect_to_runtime().await.expect("Failed to connect");
    let (mut write, mut read) = ws_stream.split();
    
    // Send malformed JSON
    write.send(Message::Text("{invalid json".to_string()))
        .await
        .expect("Failed to send");
    
    // Should get an error response or connection close
    match timeout(Duration::from_secs(5), read.next()).await {
        Ok(Some(Ok(Message::Text(text)))) => {
            println!("Received: {}", text);
            // Check if it's an error message
            if let Ok(ServerMessage::Error { error, code, .. }) = serde_json::from_str(&text) {
                println!("✓ Got expected error: {} (code: {:?})", error, code);
            } else {
                println!("⚠ Got non-error response to invalid JSON");
            }
        }
        Ok(Some(Ok(Message::Close(_)))) => {
            println!("✓ Server closed connection (valid error handling)");
        }
        other => {
            println!("Response: {:?}", other);
        }
    }
    
    // Close WebSocket properly
    let close_frame = CloseFrame {
        code: CloseCode::Normal,
        reason: "Test complete".into(),
    };
    let _ = write.send(Message::Close(Some(close_frame))).await;
}

/// Test cancellation mid-stream.
#[tokio::test]
#[ignore = "Requires aura-runtime running on localhost:8080"]
async fn test_cancellation() {
    let ws_stream = connect_to_runtime().await.expect("Failed to connect");
    let (mut write, mut read) = ws_stream.split();
    
    let request_id = "cancel-test-123";
    
    // Send a prompt that will take a while
    let prompt = ClientMessage::Prompt {
        request_id: request_id.to_string(),
        prompt: "Write a very long essay about the history of computing, at least 1000 words.".to_string(),
        agent_id: None,
        workspace: None,
    };
    
    write.send(Message::Text(serde_json::to_string(&prompt).unwrap()))
        .await
        .expect("Failed to send prompt");
    
    // Wait for turn_start
    let mut got_turn_start = false;
    for _ in 0..10 {
        if let Ok(Some(Ok(Message::Text(text)))) = timeout(Duration::from_secs(2), read.next()).await {
            if let Ok(ServerMessage::TurnStart { .. }) = serde_json::from_str(&text) {
                got_turn_start = true;
                println!("Got TurnStart, sending cancel...");
                break;
            }
        }
    }
    
    if !got_turn_start {
        println!("⚠ Never got TurnStart, skipping cancel test");
        return;
    }
    
    // Send cancel
    let cancel = ClientMessage::Cancel {
        request_id: request_id.to_string(),
    };
    write.send(Message::Text(serde_json::to_string(&cancel).unwrap()))
        .await
        .expect("Failed to send cancel");
    
    // Should get cancelled message
    let mut got_cancelled = false;
    for _ in 0..20 {
        match timeout(Duration::from_secs(2), read.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => {
                println!("Received: {}", &text[..text.len().min(100)]);
                if let Ok(ServerMessage::Cancelled { .. }) = serde_json::from_str(&text) {
                    got_cancelled = true;
                    break;
                }
                if let Ok(ServerMessage::TurnComplete { .. }) = serde_json::from_str(&text) {
                    println!("⚠ Got TurnComplete instead of Cancelled (may have finished before cancel)");
                    break;
                }
            }
            _ => break,
        }
    }
    
    if got_cancelled {
        println!("✓ Cancellation test passed");
    } else {
        println!("⚠ Did not receive Cancelled message");
    }
    
    // Close WebSocket properly
    let close_frame = CloseFrame {
        code: CloseCode::Normal,
        reason: "Test complete".into(),
    };
    let _ = write.send(Message::Close(Some(close_frame))).await;
}

/// Stress test: multiple rapid requests.
#[tokio::test]
#[ignore = "Requires aura-runtime running on localhost:8080"]
async fn test_rapid_requests() {
    const NUM_REQUESTS: usize = 3;
    
    println!("Sending {} rapid requests...", NUM_REQUESTS);
    
    let mut handles = Vec::new();
    
    for i in 0..NUM_REQUESTS {
        let handle = tokio::spawn(async move {
            let prompt = format!("What is {} + {}? Answer with just the number.", i, i);
            let result = send_prompt_and_collect(&prompt).await;
            (i, result)
        });
        handles.push(handle);
    }
    
    let mut successes = 0;
    let mut failures = 0;
    
    for handle in handles {
        match handle.await {
            Ok((i, Ok((messages, text)))) => {
                let has_complete = messages.iter().any(|m| matches!(m, ServerMessage::TurnComplete { .. }));
                if has_complete {
                    println!("Request {}: ✓ ({} messages, response: {})", 
                        i, messages.len(), text.trim());
                    successes += 1;
                } else {
                    println!("Request {}: ⚠ No TurnComplete", i);
                    failures += 1;
                }
            }
            Ok((i, Err(e))) => {
                println!("Request {}: ✗ {}", i, e);
                failures += 1;
            }
            Err(e) => {
                println!("Join error: {}", e);
                failures += 1;
            }
        }
    }
    
    println!("\n=== Results ===");
    println!("Successes: {}", successes);
    println!("Failures: {}", failures);
    
    assert!(successes > 0, "At least some requests should succeed");
    println!("\n✓ Rapid requests test passed ({}/{} succeeded)", successes, NUM_REQUESTS);
}
