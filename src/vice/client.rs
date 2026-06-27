//! The real `ModelClient` backed by genai (KTD4): one unified client across
//! OpenAI-compatible providers and Anthropic, with the key resolved from the
//! invoker's env var (R9) and an optional custom base URL (R8).
//!
//! This is the only genai-touching file. Live calls are never asserted in CI
//! (the loop is tested against a fake provider, oracle r3); this code only needs
//! to compile and be wired correctly.

use crate::config::Config;
use crate::vice::tools::tool_specs;
use crate::vice::{Convo, ModelClient, ToolCall, Turn, ViceError};

use genai::chat::{ChatMessage, ChatRequest, Tool, ToolResponse};
use genai::resolver::{AuthData, AuthResolver, Endpoint, ServiceTargetResolver};
use genai::Client;

pub struct GenaiClient {
    client: Client,
    model: String,
    tools: Vec<Tool>,
}

impl GenaiClient {
    /// Build a client from config. Resolves the api key from the configured env
    /// var up front (R9) so a missing key fails clearly before any network call.
    pub fn new(cfg: &Config) -> Result<GenaiClient, ViceError> {
        let key = cfg.resolve_api_key().map_err(ViceError::Config)?;

        let tools = tool_specs()
            .into_iter()
            .map(|s| {
                Tool::new(s.name)
                    .with_description(s.description)
                    .with_schema(s.schema)
            })
            .collect();

        // Inject the resolved key regardless of the provider's default env name.
        let auth = AuthResolver::from_resolver_fn(
            move |_iden: genai::ModelIden| -> genai::resolver::Result<Option<AuthData>> {
                Ok(Some(AuthData::from_single(key.clone())))
            },
        );

        let mut builder = Client::builder().with_auth_resolver(auth);

        // Optional custom base URL for OpenAI-compatible providers (DeepSeek/Qwen/local).
        if let Some(base) = cfg.base_url.clone() {
            let st = ServiceTargetResolver::from_resolver_fn(
                move |mut target: genai::ServiceTarget| -> genai::resolver::Result<genai::ServiceTarget> {
                    target.endpoint = Endpoint::from_owned(base.clone());
                    Ok(target)
                },
            );
            builder = builder.with_service_target_resolver(st);
        }

        Ok(GenaiClient {
            client: builder.build(),
            model: cfg.model.clone(),
            tools,
        })
    }

    fn build_request(&self, convo: &Convo) -> ChatRequest {
        let mut req = ChatRequest::new(vec![ChatMessage::system(convo.system.clone())]);
        req = req.append_message(ChatMessage::user(convo.user.clone()));
        for step in &convo.steps {
            let calls: Vec<genai::chat::ToolCall> = step
                .calls
                .iter()
                .map(|c| genai::chat::ToolCall {
                    call_id: c.id.clone(),
                    fn_name: c.name.clone(),
                    fn_arguments: c.args.clone(),
                    thought_signatures: None,
                })
                .collect();
            req = req.append_message(ChatMessage::from(calls));
            for r in &step.results {
                req = req.append_message(ChatMessage::from(ToolResponse::new(
                    r.id.clone(),
                    r.content.clone(),
                )));
            }
        }
        req.with_tools(self.tools.clone())
    }
}

impl ModelClient for GenaiClient {
    async fn next_turn(&self, convo: &Convo) -> Result<Turn, ViceError> {
        let req = self.build_request(convo);
        let resp = self
            .client
            .exec_chat(&self.model, req, None)
            .await
            .map_err(|e| ViceError::Model(e.to_string()))?;

        if resp.tool_calls().is_empty() {
            Ok(Turn::Final(resp.into_first_text().unwrap_or_default()))
        } else {
            let mapped = resp
                .into_tool_calls()
                .into_iter()
                .map(|tc| ToolCall {
                    id: tc.call_id,
                    name: tc.fn_name,
                    args: tc.fn_arguments,
                })
                .collect();
            Ok(Turn::ToolCalls(mapped))
        }
    }

    fn model_id(&self) -> &str {
        &self.model
    }
}
