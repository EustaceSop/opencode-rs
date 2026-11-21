use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::id::{self, IdError, Prefix};
use crate::jsnum;

pub type JsonMap = serde_json::Map<String, Value>;

pub const GLOBAL_PROJECT_ID: &str = "global";
pub const PARENT_TITLE_PREFIX: &str = "New session - ";
pub const CHILD_TITLE_PREFIX: &str = "Child session - ";

const ADJECTIVES: [&str; 29] = [
    "brave", "calm", "clever", "cosmic", "crisp", "curious", "eager", "gentle", "glowing", "happy",
    "hidden", "jolly", "kind", "lucky", "mighty", "misty", "neon", "nimble", "playful", "proud",
    "quick", "quiet", "shiny", "silent", "stellar", "sunny", "swift", "tidy", "witty",
];

const NOUNS: [&str; 31] = [
    "cabin", "cactus", "canyon", "circuit", "comet", "eagle", "engine", "falcon", "forest",
    "garden", "harbor", "island", "knight", "lagoon", "meadow", "moon", "mountain", "nebula",
    "orchid", "otter", "panda", "pixel", "planet", "river", "rocket", "sailor", "squid", "star",
    "tiger", "wizard", "wolf",
];

pub fn slug() -> String {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).expect("system random source unavailable");
    let adjective =
        u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize % ADJECTIVES.len();
    let noun = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize % NOUNS.len();
    format!("{}-{}", ADJECTIVES[adjective], NOUNS[noun])
}

// Equivalent of `new Date(ms).toISOString()`.
pub fn iso_timestamp(ms: u64) -> String {
    let millis = ms % 1000;
    let seconds = ms / 1000;
    let days = (seconds / 86_400) as i64;
    let rem = seconds % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

pub fn default_title(child: bool, now_ms: u64) -> String {
    let prefix = if child {
        CHILD_TITLE_PREFIX
    } else {
        PARENT_TITLE_PREFIX
    };
    format!("{prefix}{}", iso_timestamp(now_ms))
}

pub fn is_default_title(title: &str) -> bool {
    let Some(rest) = title
        .strip_prefix(PARENT_TITLE_PREFIX)
        .or_else(|| title.strip_prefix(CHILD_TITLE_PREFIX))
    else {
        return false;
    };
    let pattern = b"dddd-dd-ddTdd:dd:dd.dddZ";
    rest.len() == pattern.len()
        && rest
            .bytes()
            .zip(pattern)
            .all(|(byte, expected)| match expected {
                b'd' => byte.is_ascii_digit(),
                literal => byte == *literal,
            })
}

// `Todo.Info`: status and priority are free strings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Todo {
    pub content: String,
    pub status: String,
    pub priority: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileDiffStatus {
    Added,
    Deleted,
    Modified,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileDiff {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patch: Option<String>,
    #[serde(serialize_with = "jsnum::ser")]
    pub additions: f64,
    #[serde(serialize_with = "jsnum::ser")]
    pub deletions: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<FileDiffStatus>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionAction {
    Allow,
    Deny,
    Ask,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionRule {
    pub permission: String,
    pub pattern: String,
    pub action: PermissionAction,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct TokenCache {
    #[serde(serialize_with = "jsnum::ser")]
    pub read: f64,
    #[serde(serialize_with = "jsnum::ser")]
    pub write: f64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Tokens {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "jsnum::ser_opt"
    )]
    pub total: Option<f64>,
    #[serde(serialize_with = "jsnum::ser")]
    pub input: f64,
    #[serde(serialize_with = "jsnum::ser")]
    pub output: f64,
    #[serde(serialize_with = "jsnum::ser")]
    pub reasoning: f64,
    pub cache: TokenCache,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionTokens {
    #[serde(serialize_with = "jsnum::ser")]
    pub input: f64,
    #[serde(serialize_with = "jsnum::ser")]
    pub output: f64,
    #[serde(serialize_with = "jsnum::ser")]
    pub reasoning: f64,
    pub cache: TokenCache,
}

impl SessionTokens {
    // `tokens.total` is ignored, as in the original projector.
    pub fn apply(&mut self, tokens: &Tokens, sign: f64) {
        self.input += tokens.input * sign;
        self.output += tokens.output * sign;
        self.reasoning += tokens.reasoning * sign;
        self.cache.read += tokens.cache.read * sign;
        self.cache.write += tokens.cache.write * sign;
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSummary {
    #[serde(serialize_with = "jsnum::ser")]
    pub additions: f64,
    #[serde(serialize_with = "jsnum::ser")]
    pub deletions: f64,
    #[serde(serialize_with = "jsnum::ser")]
    pub files: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diffs: Option<Vec<FileDiff>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionShare {
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRevert {
    #[serde(rename = "messageID")]
    pub message_id: String,
    #[serde(rename = "partID", default, skip_serializing_if = "Option::is_none")]
    pub part_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionModel {
    pub id: String,
    #[serde(rename = "providerID")]
    pub provider_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SessionTime {
    pub created: u64,
    pub updated: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compacting: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "jsnum::ser_opt"
    )]
    pub archived: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub slug: String,
    #[serde(rename = "projectID")]
    pub project_id: String,
    #[serde(
        rename = "workspaceID",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub workspace_id: Option<String>,
    pub directory: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(rename = "parentID", default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<SessionSummary>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "jsnum::ser_opt"
    )]
    pub cost: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<SessionTokens>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub share: Option<SessionShare>,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<SessionModel>,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<JsonMap>,
    pub time: SessionTime,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission: Option<Vec<PermissionRule>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revert: Option<SessionRevert>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct NewSession {
    pub id: Option<String>,
    pub project_id: String,
    pub directory: String,
    pub version: String,
    pub path: Option<String>,
    pub parent_id: Option<String>,
    pub workspace_id: Option<String>,
    pub title: Option<String>,
    pub agent: Option<String>,
    pub model: Option<SessionModel>,
    pub metadata: Option<JsonMap>,
    pub permission: Option<Vec<PermissionRule>>,
}

impl Session {
    pub fn new(input: NewSession) -> Result<Session, IdError> {
        let now = id::now_ms();
        let title = input
            .title
            .unwrap_or_else(|| default_title(input.parent_id.is_some(), now));
        Ok(Session {
            id: id::generate_or(Prefix::Session, input.id)?,
            slug: slug(),
            project_id: input.project_id,
            workspace_id: input.workspace_id,
            directory: input.directory,
            path: input.path,
            parent_id: input.parent_id,
            summary: None,
            cost: Some(0.0),
            tokens: Some(SessionTokens::default()),
            share: None,
            title,
            agent: input.agent,
            model: input.model,
            version: input.version,
            metadata: input.metadata,
            time: SessionTime {
                created: now,
                updated: now,
                compacting: None,
                archived: None,
            },
            permission: input.permission,
            revert: None,
        })
    }

    pub fn is_child(&self) -> bool {
        self.parent_id.is_some()
    }

    pub fn has_default_title(&self) -> bool {
        is_default_title(&self.title)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectIcon {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r#override: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectCommands {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectTime {
    pub created: u64,
    pub updated: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initialized: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Vcs {
    Git,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub worktree: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vcs: Option<Vcs>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<ProjectIcon>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commands: Option<ProjectCommands>,
    pub time: ProjectTime,
    pub sandboxes: Vec<String>,
}

fn default_retry_count() -> u64 {
    2
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum OutputFormat {
    #[serde(rename = "text")]
    Text,
    #[serde(rename = "json_schema")]
    JsonSchema {
        schema: JsonMap,
        #[serde(rename = "retryCount", default = "default_retry_count")]
        retry_count: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserTime {
    pub created: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserSummary {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    pub diffs: Vec<FileDiff>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserModel {
    #[serde(rename = "providerID")]
    pub provider_id: String,
    #[serde(rename = "modelID")]
    pub model_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserMessage {
    pub id: String,
    #[serde(rename = "sessionID")]
    pub session_id: String,
    pub time: UserTime,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<OutputFormat>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<UserSummary>,
    pub agent: String,
    pub model: UserModel,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<BTreeMap<String, bool>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiErrorData {
    pub message: String,
    #[serde(
        rename = "statusCode",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub status_code: Option<u64>,
    #[serde(rename = "isRetryable")]
    pub is_retryable: bool,
    #[serde(
        rename = "responseHeaders",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub response_headers: Option<BTreeMap<String, String>>,
    #[serde(
        rename = "responseBody",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub response_body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApiErrorName {
    #[default]
    #[serde(rename = "APIError")]
    ApiError,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiError {
    pub name: ApiErrorName,
    pub data: ApiErrorData,
}

impl From<ApiErrorData> for ApiError {
    fn from(data: ApiErrorData) -> Self {
        ApiError {
            name: ApiErrorName::ApiError,
            data,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "name", content = "data")]
pub enum AssistantError {
    ProviderAuthError {
        #[serde(rename = "providerID")]
        provider_id: String,
        message: String,
    },
    UnknownError {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        r#ref: Option<String>,
    },
    MessageOutputLengthError {},
    MessageAbortedError {
        message: String,
    },
    StructuredOutputError {
        message: String,
        retries: u64,
    },
    ContextOverflowError {
        message: String,
        #[serde(
            rename = "responseBody",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        response_body: Option<String>,
    },
    ContentFilterError {
        message: String,
    },
    #[serde(rename = "APIError")]
    ApiError(ApiErrorData),
}

impl AssistantError {
    pub fn aborted() -> Self {
        AssistantError::MessageAbortedError {
            message: "Aborted".to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssistantTime {
    pub created: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessagePath {
    pub cwd: String,
    pub root: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssistantMessage {
    pub id: String,
    #[serde(rename = "sessionID")]
    pub session_id: String,
    pub time: AssistantTime,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<AssistantError>,
    #[serde(rename = "parentID")]
    pub parent_id: String,
    #[serde(rename = "modelID")]
    pub model_id: String,
    #[serde(rename = "providerID")]
    pub provider_id: String,
    pub mode: String,
    pub agent: String,
    pub path: MessagePath,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<bool>,
    #[serde(serialize_with = "jsnum::ser")]
    pub cost: f64,
    pub tokens: Tokens,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase")]
#[allow(clippy::large_enum_variant)]
pub enum Message {
    User(UserMessage),
    Assistant(AssistantMessage),
}

impl Message {
    pub fn id(&self) -> &str {
        match self {
            Message::User(message) => &message.id,
            Message::Assistant(message) => &message.id,
        }
    }

    pub fn session_id(&self) -> &str {
        match self {
            Message::User(message) => &message.session_id,
            Message::Assistant(message) => &message.session_id,
        }
    }

    pub fn time_created(&self) -> u64 {
        match self {
            Message::User(message) => message.time.created,
            Message::Assistant(message) => message.time.created,
        }
    }

    pub fn role(&self) -> &'static str {
        match self {
            Message::User(_) => "user",
            Message::Assistant(_) => "assistant",
        }
    }

    pub fn as_user(&self) -> Option<&UserMessage> {
        match self {
            Message::User(message) => Some(message),
            Message::Assistant(_) => None,
        }
    }

    pub fn as_assistant(&self) -> Option<&AssistantMessage> {
        match self {
            Message::Assistant(message) => Some(message),
            Message::User(_) => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageWithParts {
    pub info: Message,
    pub parts: Vec<Part>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartTime {
    pub start: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TextPart {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synthetic: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignored: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time: Option<PartTime>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<JsonMap>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReasoningPart {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<JsonMap>,
    pub time: PartTime,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceText {
    pub value: String,
    #[serde(serialize_with = "jsnum::ser")]
    pub start: f64,
    #[serde(serialize_with = "jsnum::ser")]
    pub end: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    pub line: u64,
    pub character: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum FilePartSource {
    File {
        text: SourceText,
        path: String,
    },
    Symbol {
        text: SourceText,
        path: String,
        range: Range,
        name: String,
        kind: u64,
    },
    Resource {
        text: SourceText,
        #[serde(rename = "clientName")]
        client_name: String,
        uri: String,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FilePart {
    pub mime: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<FilePartSource>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileKind {
    #[default]
    File,
}

// A complete FilePart object (ids + `type: "file"`), as nested in tool attachments.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileAttachment {
    pub id: String,
    #[serde(rename = "sessionID")]
    pub session_id: String,
    #[serde(rename = "messageID")]
    pub message_id: String,
    #[serde(rename = "type")]
    pub kind: FileKind,
    #[serde(flatten)]
    pub file: FilePart,
}

impl From<FileAttachment> for Part {
    fn from(attachment: FileAttachment) -> Self {
        Part {
            id: attachment.id,
            session_id: attachment.session_id,
            message_id: attachment.message_id,
            body: PartBody::File(attachment.file),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSource {
    pub value: String,
    pub start: u64,
    pub end: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentPart {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<AgentSource>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionPart {
    pub auto: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overflow: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tail_start_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubtaskModel {
    #[serde(rename = "providerID")]
    pub provider_id: String,
    #[serde(rename = "modelID")]
    pub model_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubtaskPart {
    pub prompt: String,
    pub description: String,
    pub agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<SubtaskModel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryTime {
    pub created: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryPart {
    pub attempt: u64,
    pub error: ApiError,
    pub time: RetryTime,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepStartPart {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StepFinishPart {
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<String>,
    #[serde(serialize_with = "jsnum::ser")]
    pub cost: f64,
    pub tokens: Tokens,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotPart {
    pub snapshot: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PatchPart {
    pub hash: String,
    pub files: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolTimeRunning {
    pub start: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolTimeCompleted {
    pub start: u64,
    pub end: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compacted: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolTimeError {
    pub start: u64,
    pub end: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolStatePending {
    pub input: JsonMap,
    pub raw: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolStateRunning {
    pub input: JsonMap,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<JsonMap>,
    pub time: ToolTimeRunning,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolStateCompleted {
    pub input: JsonMap,
    pub output: String,
    pub title: String,
    pub metadata: JsonMap,
    pub time: ToolTimeCompleted,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachments: Option<Vec<FileAttachment>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolStateError {
    pub input: JsonMap,
    pub error: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<JsonMap>,
    pub time: ToolTimeError,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum ToolState {
    Pending(ToolStatePending),
    Running(ToolStateRunning),
    Completed(ToolStateCompleted),
    Error(ToolStateError),
}

impl ToolState {
    pub fn pending(input: JsonMap, raw: impl Into<String>) -> Self {
        ToolState::Pending(ToolStatePending {
            input,
            raw: raw.into(),
        })
    }

    pub fn status(&self) -> &'static str {
        match self {
            ToolState::Pending(_) => "pending",
            ToolState::Running(_) => "running",
            ToolState::Completed(_) => "completed",
            ToolState::Error(_) => "error",
        }
    }

    pub fn input(&self) -> &JsonMap {
        match self {
            ToolState::Pending(state) => &state.input,
            ToolState::Running(state) => &state.input,
            ToolState::Completed(state) => &state.input,
            ToolState::Error(state) => &state.input,
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, ToolState::Completed(_) | ToolState::Error(_))
    }

    pub fn start(&self) -> Option<u64> {
        match self {
            ToolState::Pending(_) => None,
            ToolState::Running(state) => Some(state.time.start),
            ToolState::Completed(state) => Some(state.time.start),
            ToolState::Error(state) => Some(state.time.start),
        }
    }

    pub fn to_running(&self, input: JsonMap, start: u64) -> Self {
        ToolState::Running(ToolStateRunning {
            input,
            title: None,
            metadata: None,
            time: ToolTimeRunning {
                start: self.start().unwrap_or(start),
            },
        })
    }

    pub fn to_completed(
        &self,
        output: impl Into<String>,
        title: impl Into<String>,
        metadata: JsonMap,
        attachments: Option<Vec<FileAttachment>>,
        end: u64,
    ) -> Self {
        ToolState::Completed(ToolStateCompleted {
            input: self.input().clone(),
            output: output.into(),
            title: title.into(),
            metadata,
            time: ToolTimeCompleted {
                start: self.start().unwrap_or(end),
                end,
                compacted: None,
            },
            attachments,
        })
    }

    pub fn to_error(&self, error: impl Into<String>, metadata: Option<JsonMap>, end: u64) -> Self {
        ToolState::Error(ToolStateError {
            input: self.input().clone(),
            error: error.into(),
            metadata,
            time: ToolTimeError {
                start: self.start().unwrap_or(end),
                end,
            },
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolPart {
    #[serde(rename = "callID")]
    pub call_id: String,
    pub tool: String,
    pub state: ToolState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<JsonMap>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum PartBody {
    Text(TextPart),
    Subtask(SubtaskPart),
    Reasoning(ReasoningPart),
    File(FilePart),
    Tool(ToolPart),
    StepStart(StepStartPart),
    StepFinish(StepFinishPart),
    Snapshot(SnapshotPart),
    Patch(PatchPart),
    Agent(AgentPart),
    Retry(RetryPart),
    Compaction(CompactionPart),
}

impl PartBody {
    pub fn kind(&self) -> &'static str {
        match self {
            PartBody::Text(_) => "text",
            PartBody::Subtask(_) => "subtask",
            PartBody::Reasoning(_) => "reasoning",
            PartBody::File(_) => "file",
            PartBody::Tool(_) => "tool",
            PartBody::StepStart(_) => "step-start",
            PartBody::StepFinish(_) => "step-finish",
            PartBody::Snapshot(_) => "snapshot",
            PartBody::Patch(_) => "patch",
            PartBody::Agent(_) => "agent",
            PartBody::Retry(_) => "retry",
            PartBody::Compaction(_) => "compaction",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Part {
    pub id: String,
    #[serde(rename = "sessionID")]
    pub session_id: String,
    #[serde(rename = "messageID")]
    pub message_id: String,
    #[serde(flatten)]
    pub body: PartBody,
}

impl Part {
    // Generates a fresh ascending `prt_` id.
    pub fn new(
        session_id: impl Into<String>,
        message_id: impl Into<String>,
        body: PartBody,
    ) -> Self {
        Part {
            id: id::part(),
            session_id: session_id.into(),
            message_id: message_id.into(),
            body,
        }
    }

    pub fn kind(&self) -> &'static str {
        self.body.kind()
    }

    pub fn usage(&self) -> Option<(f64, &Tokens)> {
        match &self.body {
            PartBody::StepFinish(step) => Some((step.cost, &step.tokens)),
            _ => None,
        }
    }
}
