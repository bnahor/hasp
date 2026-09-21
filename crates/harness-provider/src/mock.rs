//! Deterministic mock provider for tests.

use async_trait::async_trait;
use harness_core::{AssistantTurn, ModelRequest, Provider, ProviderError, Sink};
use std::sync::Mutex;

pub struct MockProvider {
    pub script: Mutex<Vec<AssistantTurn>>,
    pub requests: Mutex<Vec<ModelRequest>>,
    window: usize,
}

impl MockProvider {
    pub fn new(script: Vec<AssistantTurn>) -> Self {
        Self {
            script: Mutex::new(script),
            requests: Mutex::new(Vec::new()),
            window: 200_000,
        }
    }

    pub fn with_window(mut self, window: usize) -> Self {
        self.window = window;
        self
    }
}

#[async_trait]
impl Provider for MockProvider {
    fn name(&self) -> &'static str {
        "mock"
    }

    fn context_window(&self) -> usize {
        self.window
    }

    async fn complete(
        &self,
        req: &ModelRequest,
        _sink: &mut dyn Sink,
    ) -> Result<AssistantTurn, ProviderError> {
        self.requests
            .lock()
            .expect("mock script lock")
            .push(req.clone());
        let mut script = self.script.lock().expect("mock script lock");
        if script.is_empty() {
            return Ok(AssistantTurn {
                blocks: vec![harness_core::ContentBlock::Text {
                    text: "(mock exhausted)".into(),
                }],
                usage: harness_core::Usage::default(),
                stop_reason: harness_core::StopReason::Stop,
            });
        }
        Ok(script.remove(0))
    }
}
