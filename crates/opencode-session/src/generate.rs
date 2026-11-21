// One-shot small-model requests outside the loop (`LLM.stream` with `small: true`), and the project copy namer
// (`server/routes/instance/httpapi/handlers/project-copy.ts`).
use crate::run::{Hooks, chat_hooks, small_options, system_transform};
use crate::{Ctx, Engine, Error, parse_model, plain_directory, plugins, system};
use opencode_core::config::merge_deep;
use opencode_llm::provider::{Model, Provider};
use opencode_llm::{self as llm, Event, Request, client, retry, shape};
use opencode_storage::{UserMessage, UserModel, UserTime, id, identity, slug};
use opencode_tools::util::{is_js_whitespace, js_trim};
use std::path::Path;

#[derive(Debug, Clone, Default)]
pub struct TextRequest {
    // "provider/model"; None picks `Provider.getSmallModel` of the default model's provider, else the default model.
    pub model: Option<String>,
    // The agent name plugin hooks see.
    pub agent: String,
    // The agent prompt; None or empty sends the provider's default system prompt, as an agent without one does.
    pub prompt: Option<String>,
    // System text after the prompt.
    pub system: Vec<String>,
    // User messages, in order.
    pub messages: Vec<String>,
    // Further attempts after a retryable failure (`maxRetries`).
    pub retries: u32,
}

// `Provider.getSmallModel`: the configured small model (none when it does not resolve), else a plugin's pick, else
// the built-in family priorities.
pub(crate) async fn small_model<'a>(ctx: &'a Ctx, provider_id: &str) -> Option<(&'a Provider, &'a Model)> {
    if ctx.info.small_model_ref().is_some() {
        return ctx.small_model(provider_id);
    }
    let hinted = plugins::small_model(ctx, provider_id).await;
    hinted.as_ref().and_then(|(provider, model)| ctx.model(provider, model).ok()).or_else(|| ctx.small_model(provider_id))
}

// `slugify` of the project copy handler: lowercase, runs of anything but [a-z0-9] become "-", no outer dashes.
pub fn slugify(input: &str) -> String {
    let mut out = String::new();
    for c in js_trim(input).to_lowercase().chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

// What the model's answer becomes: its first three words, slugified; a random slug when it said nothing.
pub fn copy_name(output: &str) -> String {
    let output = js_trim(output);
    if output.is_empty() {
        return slug();
    }
    let words: Vec<&str> = output.split(is_js_whitespace).filter(|word| !word.is_empty()).take(3).collect();
    slugify(&words.join(" "))
}

impl Engine {
    // The small model of the directory's default provider, as (provider id, model id).
    pub async fn small_model(&self, directory: &Path) -> Result<Option<(String, String)>, Error> {
        let ctx = self.instance(&plain_directory(directory.to_path_buf()), None).await?;
        let Some((provider_id, _)) = self.default_model_in(&ctx) else { return Ok(None) };
        Ok(small_model(&ctx, &provider_id).await.map(|(provider, model)| (provider.id.clone(), model.id.clone())))
    }

    // One request with the small-request options and the loop's request preparation (system transform, `chat.params`,
    // `chat.headers`); returns the streamed text. No session or message is stored.
    pub async fn generate_text(&self, directory: &Path, request: TextRequest) -> Result<String, Error> {
        let ctx = self.instance(&plain_directory(directory.to_path_buf()), None).await?;
        let (provider, model) = match &request.model {
            Some(spec) => {
                let (provider_id, model_id) = parse_model(spec)?;
                ctx.model(&provider_id, &model_id)?
            }
            None => {
                let (provider_id, model_id) = self.default_model_in(&ctx).ok_or_else(|| Error::Config("no model configured; pass --model provider/model".into()))?;
                match small_model(&ctx, &provider_id).await {
                    Some(found) => found,
                    None => ctx.model(&provider_id, &model_id)?,
                }
            }
        };
        let session_id = id::session();
        let user = UserMessage {
            id: id::ascending(id::Prefix::Message),
            session_id: session_id.clone(),
            time: UserTime { created: id::now_ms() },
            format: None,
            summary: None,
            agent: request.agent.clone(),
            model: UserModel { provider_id: provider.id.clone(), model_id: model.id.clone(), variant: None },
            system: None,
            tools: None,
        };
        let head = match request.prompt.as_deref().filter(|p| !p.is_empty()) {
            Some(prompt) => prompt.to_string(),
            None => system::provider_prompt(model),
        };
        let joined: Vec<&str> = std::iter::once(head.as_str()).chain(request.system.iter().map(String::as_str)).filter(|s| !s.is_empty()).collect();
        let hooks = Hooks { engine: Some(self), ctx: &ctx };
        let system = system_transform(&hooks, &session_id, model, vec![joined.join("\n")]).await.map_err(Error::Config)?;
        let mut options = small_options(model);
        merge_deep(&mut options, model.options.clone());
        let mut call = Request {
            model: model.api_id.clone(),
            system,
            messages: request.messages.iter().map(|text| llm::Message::User(text.clone())).collect(),
            options,
            ..Default::default()
        };
        let project_id = identity::resolve(&ctx.cwd).id;
        let meta = shape::Session { session_id: &session_id, parent_session_id: None, request_id: Some(&user.id), project_id: Some(&project_id) };
        shape::apply(&mut call, provider, model, &meta);
        chat_hooks(&hooks, &session_id, &request.agent, provider, model, Some(&user), &mut call).await.map_err(Error::Config)?;
        let loader = plugins::PluginAuth::new(ctx.plugins.as_ref());
        let mut attempt = 0;
        loop {
            let mut text = String::new();
            let result = client::stream_via(&self.http, &loader, provider, model, &call, |event| {
                if let Event::TextDelta(delta) = event {
                    text.push_str(&delta);
                }
            })
            .await;
            match result {
                Ok(_) => return Ok(text),
                Err(error) if attempt < request.retries && retry::classify(&error, &provider.id).retryable => attempt += 1,
                Err(error) => return Err(error.into()),
            }
        }
    }

    // `POST /experimental/project/{id}/copy/generate-name`: a random slug without context; otherwise the small model
    // (else the default one) names the task, and any failure falls back to a random slug.
    pub async fn generate_name(&self, directory: &Path, context: Option<&str>) -> String {
        let Some(text) = context.map(js_trim).filter(|text| !text.is_empty()) else { return slug() };
        let request = TextRequest {
            agent: "project-copy-name".into(),
            messages: vec![format!("Generate a short 2-3 word name that describes this task:\n{text}")],
            retries: 2,
            ..Default::default()
        };
        match self.generate_text(directory, request).await {
            Ok(output) => copy_name(&output),
            Err(_) => slug(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_follow_the_handler() {
        assert_eq!(copy_name("  Fix Login Bug now  "), "fix-login-bug");
        assert_eq!(copy_name("Refactor\u{a0}the\tparser!"), "refactor-the-parser");
        assert_eq!(copy_name("!!!"), "");
        assert!(!copy_name(" \n ").is_empty());
        assert_eq!(slugify("--Ünïcode  Names--"), "n-code-names");
    }
}
