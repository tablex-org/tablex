//! A scripted `ChatProvider` for tests and UI scenarios: it replays a list of turns (text,
//! reasoning, tool calls, failures, or a wait that only a gate or a cancel ends), records what it
//! was asked, and never touches the network.

use engine_ai::{
    ApiFailure, ChatProvider, Message, ModelInfo, ProviderError, StopReason, StreamEvent,
    TurnRequest, TurnResponse, Usage,
};
use serde_json::{json, Value};
use std::collections::VecDeque;
#[cfg(test)]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicUsize, Ordering};
#[cfg(test)]
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub enum Step {
    /// A finished turn: reasoning pieces, text pieces, tool calls `(id, tool, input)`.
    Say {
        reasoning: Vec<String>,
        text: Vec<String>,
        tools: Vec<(&'static str, &'static str, Value)>,
    },
    Fail(ProviderError),
    /// Text first, then waits for the sender (or the cancel).
    #[cfg(test)]
    Wait {
        text: String,
        gate: Receiver<()>,
    },
}

pub fn say(text: &str) -> Step {
    Step::Say {
        reasoning: Vec::new(),
        text: vec![text.to_owned()],
        tools: Vec::new(),
    }
}

#[cfg(test)]
pub fn call(id: &'static str, tool: &'static str, input: Value) -> Step {
    Step::Say {
        reasoning: Vec::new(),
        text: Vec::new(),
        tools: vec![(id, tool, input)],
    }
}

pub fn failure(message: &str) -> ApiFailure {
    ApiFailure {
        status: Some(429),
        message: message.to_owned(),
        request_id: None,
        retry_after: None,
    }
}

#[derive(Default)]
pub struct Scripted {
    pub steps: Mutex<VecDeque<Step>>,
    pub calls: AtomicUsize,
    pub active: AtomicUsize,
    pub max_active: AtomicUsize,
    #[cfg(test)]
    pub saw_cancel: AtomicBool,
    /// The messages of every request, in order.
    pub requests: Mutex<Vec<Vec<Message>>>,
    /// Every request as a whole (model, effort, system blocks, tools, messages, history form), as
    /// its `Debug` text: two requests that read the same are the same request.
    pub request_text: Mutex<Vec<String>>,
    /// What it answers once the script is used up (`None`: it gives up, as a cancel).
    pub fallback: Mutex<Option<String>>,
    /// Pause after each streamed text piece (a model that takes its time).
    pub pace: Mutex<Duration>,
}

impl Scripted {
    pub fn new(steps: Vec<Step>) -> Arc<Self> {
        Arc::new(Self {
            steps: Mutex::new(steps.into()),
            ..Self::default()
        })
    }

    /// Answers `text` to everything after the script (a scenario someone types into).
    pub fn with_fallback(self: Arc<Self>, text: &str) -> Arc<Self> {
        *self.fallback.lock().unwrap() = Some(text.to_owned());
        self
    }

    #[cfg(test)]
    pub fn push(&self, step: Step) {
        self.steps.lock().unwrap().push_back(step);
    }

    #[cfg(test)]
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl ChatProvider for Scripted {
    fn stream_turn(
        &self,
        request: &TurnRequest<'_>,
        on_event: &mut dyn FnMut(StreamEvent<'_>),
        // Only a `Wait` step (tests) watches for the cancel.
        #[cfg(test)] cancel: &engine_agent::CancelToken,
        #[cfg(not(test))] _cancel: &engine_agent::CancelToken,
    ) -> Result<TurnResponse, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_active.fetch_max(now, Ordering::SeqCst);
        self.requests
            .lock()
            .unwrap()
            .push(request.messages.to_vec());
        self.request_text
            .lock()
            .unwrap()
            .push(format!("{request:?}"));
        let step = self.steps.lock().unwrap().pop_front();
        let result = (|| {
            let step = match (step, self.fallback.lock().unwrap().clone()) {
                (Some(step), _) => step,
                (None, Some(text)) => say(&text),
                (None, None) => return Err(ProviderError::Cancelled),
            };
            match step {
                Step::Fail(error) => Err(error),
                #[cfg(test)]
                Step::Wait { text, gate } => {
                    on_event(StreamEvent::TextDelta(&text));
                    loop {
                        if cancel.is_cancelled() {
                            self.saw_cancel.store(true, Ordering::SeqCst);
                            return Err(ProviderError::Cancelled);
                        }
                        match gate.recv_timeout(Duration::from_millis(2)) {
                            Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                        }
                    }
                    Ok(response(
                        vec![json!({"type": "text", "text": text})],
                        StopReason::EndTurn,
                    ))
                }
                Step::Say {
                    reasoning,
                    text,
                    tools,
                } => {
                    let mut content = Vec::new();
                    for piece in &reasoning {
                        on_event(StreamEvent::ThinkingDelta(piece));
                    }
                    if !reasoning.is_empty() {
                        content.push(json!({"type": "thinking", "thinking": reasoning.concat(), "signature": "sig"}));
                    }
                    let pace = *self.pace.lock().unwrap();
                    for piece in &text {
                        on_event(StreamEvent::TextDelta(piece));
                        if !pace.is_zero() {
                            std::thread::sleep(pace);
                        }
                    }
                    if !text.is_empty() {
                        content.push(json!({"type": "text", "text": text.concat()}));
                    }
                    for (id, tool, input) in &tools {
                        on_event(StreamEvent::ToolCallStarted { id, name: tool });
                        on_event(StreamEvent::ToolInputReady {
                            id,
                            name: tool,
                            input,
                            valid: true,
                        });
                        content.push(
                            json!({"type": "tool_use", "id": id, "name": tool, "input": input}),
                        );
                    }
                    let stop = if tools.is_empty() {
                        StopReason::EndTurn
                    } else {
                        StopReason::ToolUse
                    };
                    Ok(response(content, stop))
                }
            }
        })();
        self.active.fetch_sub(1, Ordering::SeqCst);
        result
    }

    fn list_models(&self, _: &engine_agent::CancelToken) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(Vec::new())
    }
}

pub fn response(content: Vec<Value>, stop_reason: StopReason) -> TurnResponse {
    TurnResponse {
        content,
        stop_reason,
        usage: Usage {
            input_tokens: 10,
            output_tokens: 5,
            ..Usage::default()
        },
        model: None,
        served_by_fallback: false,
        fallback_to: None,
        thinking_dropped: 0,
        invalid_tool_inputs: Vec::new(),
        request_id: None,
    }
}
