use rmcp::model::{Tool, ToolAnnotations};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::normalize::{self, Options};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ApiRequest {
    pub service: String,
    pub path: String,
    pub method: String,
    pub query: Vec<(String, String)>,
    pub body: Option<Value>,
}

/// Argument kinds. Each tool's JSON schema and validation are derived from these.
#[derive(Clone, Copy)]
enum Field {
    Query,
    Id,
    Cursor,
    Count { max: u64, default: u64 },
    Bound,
    Offset,
    Flag,
    Mris,
    Day,
    Time,
    Sender,
    Person,
    Link,
    Chars,
}

struct Spec {
    name: &'static str,
    description: &'static str,
    fields: &'static [(&'static str, Field)],
    required: &'static [&'static str],
}

const fn count(max: u64, default: u64) -> Field {
    Field::Count { max, default }
}

const SPECS: &[Spec] = &[
    Spec {
        name: "teams_search",
        description: "Search Teams messages. Supports Teams search operators; sender, after and before add filters. Returns message IDs, links and offset pagination.",
        fields: &[
            ("query", Field::Query),
            ("from", Field::Offset),
            ("size", count(200, 10)),
            ("maxResults", Field::Bound),
            ("sender", Field::Sender),
            ("after", Field::Day),
            ("before", Field::Day),
        ],
        required: &["query"],
    },
    Spec {
        name: "teams_search_people",
        description: "Find people by name or email.",
        fields: &[("query", Field::Query), ("limit", count(200, 10))],
        required: &["query"],
    },
    Spec {
        name: "teams_find_channel",
        description: "Find matching channels among teams you belong to. Organisation-wide discovery is not included.",
        fields: &[("query", Field::Query), ("limit", count(200, 10))],
        required: &["query"],
    },
    Spec {
        name: "teams_get_person",
        description: "Resolve 1–100 Teams MRIs to people profiles.",
        fields: &[("mris", Field::Mris)],
        required: &["mris"],
    },
    Spec {
        name: "teams_list_chats",
        description: "List recent conversations (up to 200), including chats, meetings and channels.",
        fields: &[("limit", count(200, 20))],
        required: &[],
    },
    Spec {
        name: "teams_get_message",
        description: "Read one message by conversation and message ID.",
        fields: &[
            ("conversationId", Field::Id),
            ("messageId", Field::Id),
            ("maxContentChars", Field::Chars),
        ],
        required: &["conversationId", "messageId"],
    },
    Spec {
        name: "teams_get_thread",
        description: "Read a page of conversation messages without marking them read. Use threadRootId for channel replies; pass olderCursor back as cursor to read older messages.",
        fields: &[
            ("conversationId", Field::Id),
            ("threadRootId", Field::Id),
            ("limit", count(200, 20)),
            ("cursor", Field::Cursor),
            ("since", Field::Time),
            ("until", Field::Time),
            ("maxContentChars", Field::Chars),
        ],
        required: &["conversationId"],
    },
    Spec {
        name: "teams_get_unread",
        description: "Inspect unread status within the 200 most recent conversations. includeMessages also returns up to messageLimit unread messages and their count for up to maxConversations conversations.",
        fields: &[
            ("conversationId", Field::Id),
            ("includeMessages", Field::Flag),
            ("messageLimit", count(50, 10)),
            ("maxConversations", count(20, 10)),
        ],
        required: &[],
    },
    Spec {
        name: "teams_get_activity",
        description: "Read the activity feed: mentions, replies and reactions. mentionsOnly limits it to mentions.",
        fields: &[
            ("limit", count(200, 20)),
            ("mentionsOnly", Field::Flag),
            ("cursor", Field::Cursor),
        ],
        required: &[],
    },
    Spec {
        name: "teams_get_followed_threads",
        description: "List threads you follow.",
        fields: &[("limit", count(200, 20))],
        required: &[],
    },
    Spec {
        name: "teams_get_favorites",
        description: "List pinned (favorite) conversations.",
        fields: &[],
        required: &[],
    },
    Spec {
        name: "teams_get_chat",
        description: "Find the 1:1 chat conversation ID for a person (MRI or user ID) among recent conversations.",
        fields: &[("person", Field::Person)],
        required: &["person"],
    },
    Spec {
        name: "teams_list_teams",
        description: "List the teams you belong to and their channels.",
        fields: &[("limit", count(200, 50))],
        required: &[],
    },
    Spec {
        name: "teams_get_frequent_contacts",
        description: "List people you chat with most, from recent conversations.",
        fields: &[("limit", count(50, 10))],
        required: &[],
    },
    Spec {
        name: "teams_get_shared_files",
        description: "List files and links shared in one page of a conversation's messages. Pass olderCursor back as cursor for older ones.",
        fields: &[
            ("conversationId", Field::Id),
            ("threadRootId", Field::Id),
            ("limit", count(200, 100)),
            ("cursor", Field::Cursor),
        ],
        required: &["conversationId"],
    },
    Spec {
        name: "teams_open_link",
        description: "Read the message behind a Teams message link (https://teams.microsoft.com/l/message/...).",
        fields: &[("url", Field::Link), ("maxContentChars", Field::Chars)],
        required: &["url"],
    },
];

const MESSAGE_VIEW: &str = "msnp24Equivalent|supportsMessageProperties";

pub struct Operation {
    spec: &'static Spec,
    args: Map<String, Value>,
}

impl Operation {
    pub fn parse(name: &str, args: Value) -> Result<Self, String> {
        let spec = SPECS
            .iter()
            .find(|spec| spec.name == name)
            .ok_or("Unknown Teams tool")?;
        let args = args
            .as_object()
            .ok_or("Arguments must be an object")?
            .clone();
        if args
            .keys()
            .any(|key| !spec.fields.iter().any(|(field, _)| field == key))
        {
            return Err("Unexpected argument; consult the tool's input schema".into());
        }
        if spec.required.iter().any(|key| !args.contains_key(*key)) {
            return Err("Missing required argument; consult the tool's input schema".into());
        }
        for (key, field) in spec.fields {
            if let Some(value) = args.get(*key) {
                validate(*field, value)?;
            }
        }
        Ok(Self { spec, args })
    }

    /// The first request. Its result is checked for readiness and authentication.
    pub fn requests(&self) -> Vec<ApiRequest> {
        let request = match self.spec.name {
            "teams_search" => self.search_request(),
            "teams_search_people" => self.people_request(),
            "teams_find_channel" | "teams_list_teams" => teams_request(),
            "teams_get_person" => profiles_request(self.args["mris"].clone()),
            "teams_get_message" => {
                self.message_request(self.text("conversationId"), self.text("messageId"))
            }
            "teams_open_link" => {
                let (conversation, message, _) = parse_link(self.text("url")).unwrap_or_default();
                self.message_request(&conversation, &message)
            }
            "teams_get_thread" | "teams_get_shared_files" => self.thread_request(),
            "teams_get_activity" => {
                let feed = if self.flag("mentionsOnly") {
                    "48:mentions"
                } else {
                    "48:notifications"
                };
                self.feed_request(feed)
            }
            "teams_get_followed_threads" => self.feed_request("48:threads"),
            "teams_get_favorites" => request(
                "csa",
                "/api/v1/teams/users/me/conversationFolders",
                "GET",
                vec![
                    (
                        "supportsAdditionalSystemGeneratedFolders".into(),
                        "true".into(),
                    ),
                    ("supportsSliceItems".into(), "true".into()),
                ],
                None,
            ),
            "teams_list_chats" => conversations_request(self.number("limit")),
            _ => conversations_request(200),
        };
        vec![request]
    }

    /// Dependent reads, sent together in one bridge batch after the first response.
    pub fn follow_up(
        &self,
        responses: &[Value],
        profile: &Value,
    ) -> Result<Vec<ApiRequest>, String> {
        let Some(first) = responses.first() else {
            return Ok(Vec::new());
        };
        Ok(match self.spec.name {
            "teams_get_unread" if self.flag("includeMessages") => normalize::unread_targets(
                first,
                profile,
                self.args.get("conversationId").and_then(Value::as_str),
            )?
            .into_iter()
            .take(self.number("maxConversations"))
            .map(|(conversation, read)| {
                let mut query = message_query(self.number("messageLimit"));
                query.push(("startTime".into(), (read + 1).to_string()));
                request(
                    "chat",
                    &format!("{}/messages", conversation_path(conversation)),
                    "GET",
                    query,
                    None,
                )
            })
            .collect(),
            "teams_get_favorites" if normalize::favorite_ids(first)?.next().is_some() => {
                vec![conversations_request(200)]
            }
            "teams_get_frequent_contacts" => {
                let mris = normalize::contact_candidates(first, profile, self.number("limit"))?;
                if mris.is_empty() {
                    Vec::new()
                } else {
                    vec![profiles_request(json!(mris))]
                }
            }
            _ => Vec::new(),
        })
    }

    pub fn finish(&self, responses: Vec<Value>, profile: &Value) -> Result<Value, String> {
        let (first, rest) = responses
            .split_first()
            .ok_or("Expected a Teams API response")?;
        let options = Options {
            max_chars: self
                .args
                .get("maxContentChars")
                .and_then(Value::as_u64)
                .map(|n| n as usize),
            since: self
                .args
                .get("since")
                .and_then(Value::as_str)
                .and_then(epoch_millis),
            until: self
                .args
                .get("until")
                .and_then(Value::as_str)
                .and_then(epoch_millis),
        };
        let result = match self.spec.name {
            "teams_search" => normalize::search(first, self.number("from"), self.search_size()),
            "teams_search_people" => normalize::people(first, self.number("limit")),
            "teams_find_channel" => {
                normalize::channels(first, self.text("query"), self.number("limit"))
            }
            "teams_get_person" => {
                normalize::profiles(first, self.args["mris"].as_array().map_or(0, Vec::len))
            }
            "teams_list_chats" => normalize::chats(first, self.number("limit")),
            "teams_get_message" => {
                normalize::message(first, self.text("conversationId"), &options, true)
            }
            "teams_open_link" => {
                let (conversation, _, _) =
                    parse_link(self.text("url")).ok_or("Invalid Teams link")?;
                normalize::message(first, &conversation, &options, true)
            }
            "teams_get_thread" => normalize::thread(
                first,
                self.text("conversationId"),
                self.number("limit"),
                &options,
            ),
            "teams_get_unread" => normalize::unread(
                first,
                profile,
                self.args.get("conversationId").and_then(Value::as_str),
                rest,
                self.number("messageLimit"),
            ),
            "teams_get_activity" | "teams_get_followed_threads" => {
                normalize::activity(first, self.number("limit"))
            }
            "teams_get_favorites" => normalize::favorites(first, rest.first()),
            "teams_get_chat" => normalize::one_on_one(first, profile, self.text("person")),
            "teams_list_teams" => normalize::teams(first, self.number("limit")),
            "teams_get_frequent_contacts" => {
                normalize::contacts(first, profile, self.number("limit"), rest.first())
            }
            "teams_get_shared_files" => normalize::shared_files(first, self.text("conversationId")),
            _ => Err("Unknown Teams tool".into()),
        }?;
        Ok(normalize::compact(result))
    }

    fn text(&self, key: &str) -> &str {
        self.args.get(key).and_then(Value::as_str).unwrap_or("")
    }

    fn flag(&self, key: &str) -> bool {
        self.args.get(key).and_then(Value::as_bool).unwrap_or(false)
    }

    /// A validated integer argument, or the schema default for the tool.
    fn number(&self, key: &str) -> usize {
        let default = self
            .spec
            .fields
            .iter()
            .find_map(|(name, field)| match field {
                Field::Count { default, .. } if *name == key => Some(*default),
                Field::Bound if *name == key => Some(200),
                _ => None,
            })
            .unwrap_or(0);
        self.args
            .get(key)
            .and_then(Value::as_u64)
            .unwrap_or(default) as usize
    }

    fn search_size(&self) -> usize {
        self.number("size").min(self.number("maxResults"))
    }

    fn search_request(&self) -> ApiRequest {
        let query = self.text("query");
        let mut filters = String::new();
        if let Some(sender) = self.args.get("sender").and_then(Value::as_str) {
            filters.push_str(&format!(" AND from:\"{sender}\""));
        }
        for (key, operator) in [("after", ">="), ("before", "<=")] {
            if let Some(day) = self.args.get(key).and_then(Value::as_str) {
                filters.push_str(&format!(" AND sent{operator}{day}"));
            }
        }
        let body = json!({"entityRequests":[{"entityType":"Message", "contentSources":["Teams"], "propertySet":"Optimized", "fields":["Extension_SkypeSpaces_ConversationPost_Extension_FromSkypeInternalId_String", "Extension_SkypeSpaces_ConversationPost_Extension_ThreadType_String", "Extension_SkypeSpaces_ConversationPost_Extension_SkypeGroupId_String"], "query":{"queryString":format!("{query}{filters} AND NOT (isClientSoftDeleted:TRUE)"),"displayQueryString":query}, "from":self.number("from"),"size":self.search_size(),"topResultsCount":0}], "scenario":{"Name":"powerbar","Dimensions":[{"DimensionName":"QueryType","DimensionValue":"Messages"},{"DimensionName":"FormFactor","DimensionValue":"general.web.reactSearch"}]},"timezone":"UTC"});
        request(
            "substrate",
            "/searchservice/api/v2/query",
            "POST",
            vec![],
            Some(body),
        )
    }

    fn people_request(&self) -> ApiRequest {
        let query = self.text("query");
        let body = json!({"EntityRequests":[{"Query":{"QueryString":query,"DisplayQueryString":query},"EntityType":"People","Size":self.number("limit"),"Fields":["Id","MRI","DisplayName","EmailAddresses","GivenName","Surname","JobTitle","Department","CompanyName"]}]});
        request(
            "substrate",
            "/search/api/v1/suggestions",
            "POST",
            vec![("scenario".into(), "powerbar".into())],
            Some(body),
        )
    }

    fn message_request(&self, conversation: &str, message: &str) -> ApiRequest {
        request(
            "chat",
            &format!(
                "{}/messages/{}",
                conversation_path(conversation),
                segment(message)
            ),
            "GET",
            vec![],
            None,
        )
    }

    fn thread_request(&self) -> ApiRequest {
        let id = match self.args.get("threadRootId").and_then(Value::as_str) {
            Some(root) => format!("{};messageid={root}", self.text("conversationId")),
            None => self.text("conversationId").to_owned(),
        };
        let mut query = cursor_query(self.text("cursor")).unwrap_or_default();
        query.extend(message_query(self.number("limit")));
        if let Some(since) = self
            .args
            .get("since")
            .and_then(Value::as_str)
            .and_then(epoch_millis)
        {
            query.retain(|(key, _)| key != "startTime");
            query.push(("startTime".into(), since.to_string()));
        }
        request(
            "chat",
            &format!("{}/messages", conversation_path(&id)),
            "GET",
            query,
            None,
        )
    }

    fn feed_request(&self, feed: &str) -> ApiRequest {
        let mut query = cursor_query(self.text("cursor")).unwrap_or_default();
        query.extend(message_query(self.number("limit")));
        request(
            "chat",
            &format!("{}/messages", conversation_path(feed)),
            "GET",
            query,
            None,
        )
    }
}

fn validate(field: Field, value: &Value) -> Result<(), String> {
    let text = || -> Result<&str, String> {
        let text = value.as_str().ok_or("Text arguments must be strings")?;
        if text.trim().is_empty() || text.chars().any(char::is_control) {
            return Err(
                "Text arguments must be nonempty, bounded, and contain no control characters"
                    .into(),
            );
        }
        Ok(text)
    };
    let bounded = |max: usize| -> Result<&str, String> {
        let text = text()?;
        if text.len() > max {
            return Err(
                "Text arguments must be nonempty, bounded, and contain no control characters"
                    .into(),
            );
        }
        Ok(text)
    };
    match field {
        Field::Query => bounded(4000).map(drop),
        Field::Id => match bounded(8192)? {
            "." | ".." => Err(
                "Text arguments must be nonempty, bounded, and contain no control characters"
                    .into(),
            ),
            _ => Ok(()),
        },
        Field::Cursor => cursor_query(bounded(8192)?).map(drop),
        Field::Count { max, .. } => integer(value, 1, max),
        Field::Bound => integer(value, 1, 200),
        Field::Offset => integer(value, 0, 1_000_000),
        Field::Chars => integer(value, 50, 100_000),
        Field::Flag => value
            .as_bool()
            .map(drop)
            .ok_or_else(|| "Flags must be true or false".into()),
        Field::Mris => validate_mris(value),
        Field::Day => match bounded(10)? {
            day if chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d").is_ok() => Ok(()),
            _ => Err("Dates must use YYYY-MM-DD".into()),
        },
        Field::Time => epoch_millis(bounded(40)?)
            .map(drop)
            .ok_or_else(|| "Times must use YYYY-MM-DD or ISO 8601".into()),
        Field::Sender => match bounded(200)? {
            sender if sender.contains(['"', '\\']) => {
                Err("sender must not contain quotes or backslashes".into())
            }
            _ => Ok(()),
        },
        Field::Person => person_id(bounded(300)?)
            .map(drop)
            .ok_or_else(|| "person must be an 8:orgid: MRI or a user ID".into()),
        Field::Link => parse_link(bounded(8192)?)
            .map(drop)
            .ok_or_else(|| "url must be a https://teams.microsoft.com/l/message/... link".into()),
    }
}

fn integer(value: &Value, min: u64, max: u64) -> Result<(), String> {
    let number = value
        .as_u64()
        .ok_or("Numeric arguments must be nonnegative integers")?;
    if !(min..=max).contains(&number) {
        return Err(format!("Numeric argument must be {min}–{max}"));
    }
    Ok(())
}

fn validate_mris(value: &Value) -> Result<(), String> {
    let mris = value.as_array().ok_or("mris must be an array")?;
    if mris.is_empty() || mris.len() > 100 {
        return Err("Provide 1–100 MRIs".into());
    }
    if mris.iter().any(|mri| {
        mri.as_str().is_none_or(|mri| {
            !mri.starts_with("8:") || mri.len() > 300 || mri.chars().any(char::is_control)
        })
    }) {
        return Err("Each MRI must start with 8: and contain at most 300 characters".into());
    }
    Ok(())
}

/// Milliseconds since the epoch for `YYYY-MM-DD` (UTC midnight) or an RFC 3339 time.
pub(crate) fn epoch_millis(value: &str) -> Option<i64> {
    if let Ok(time) = chrono::DateTime::parse_from_rfc3339(value) {
        return Some(time.timestamp_millis());
    }
    let day = chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d").ok()?;
    Some(day.and_hms_opt(0, 0, 0)?.and_utc().timestamp_millis())
}

/// A lower-case user ID from `8:orgid:<id>` or `<id>`.
pub(crate) fn person_id(value: &str) -> Option<String> {
    let id = value.strip_prefix("8:orgid:").unwrap_or(value);
    let valid = id.len() == 36
        && id.char_indices().all(|(index, c)| {
            if [8, 13, 18, 23].contains(&index) {
                c == '-'
            } else {
                c.is_ascii_hexdigit()
            }
        });
    valid.then(|| id.to_ascii_lowercase())
}

/// Conversation ID, message ID and optional parent message ID from a Teams message link.
pub(crate) fn parse_link(value: &str) -> Option<(String, String, Option<String>)> {
    let url = url::Url::parse(value).ok()?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || !matches!(
            url.host_str(),
            Some("teams.microsoft.com" | "teams.cloud.microsoft")
        )
    {
        return None;
    }
    let mut segments = url.path_segments()?;
    if segments.next()? != "l" || segments.next()? != "message" {
        return None;
    }
    let conversation = url::form_urlencoded::parse(format!("c={}", segments.next()?).as_bytes())
        .next()?
        .1
        .into_owned();
    let message = segments.next()?.to_owned();
    let usable = |text: &str| {
        !text.is_empty()
            && text.len() <= 8192
            && !text.chars().any(char::is_control)
            && !matches!(text, "." | "..")
    };
    if !usable(&conversation) || !message.bytes().all(|b| b.is_ascii_digit()) || message.is_empty()
    {
        return None;
    }
    let parent = url
        .query_pairs()
        .find(|(key, _)| key == "parentMessageId")
        .map(|(_, value)| value.into_owned())
        .filter(|parent| {
            !parent.is_empty() && parent.bytes().all(|b| b.is_ascii_digit()) && *parent != message
        });
    Some((conversation, message, parent))
}

fn request(
    service: &str,
    path: &str,
    method: &str,
    query: Vec<(String, String)>,
    body: Option<Value>,
) -> ApiRequest {
    ApiRequest {
        service: service.into(),
        path: path.into(),
        method: method.into(),
        query,
        body,
    }
}

fn conversations_request(page_size: usize) -> ApiRequest {
    request(
        "chat",
        "/v1/users/ME/conversations",
        "GET",
        vec![
            ("view".into(), "msnp24Equivalent".into()),
            ("pageSize".into(), page_size.to_string()),
        ],
        None,
    )
}

fn teams_request() -> ApiRequest {
    request(
        "csa",
        "/api/v3/teams/users/me",
        "GET",
        vec![
            ("isPrefetch".into(), "false".into()),
            ("enableMembershipSummary".into(), "true".into()),
        ],
        None,
    )
}

fn profiles_request(mris: Value) -> ApiRequest {
    request(
        "profile",
        "/beta/users/fetchShortProfile",
        "POST",
        vec![
            ("isMailAddress".into(), "false".into()),
            ("enableGuest".into(), "true".into()),
            ("skypeTeamsInfo".into(), "true".into()),
        ],
        Some(mris),
    )
}

fn message_query(page_size: usize) -> Vec<(String, String)> {
    vec![
        ("view".into(), MESSAGE_VIEW.into()),
        ("pageSize".into(), page_size.to_string()),
    ]
}

fn conversation_path(id: &str) -> String {
    format!("/v1/users/ME/conversations/{}", segment(id))
}

pub(crate) fn segment(value: &str) -> String {
    value
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

pub(crate) fn cursor_query(cursor: &str) -> Result<Vec<(String, String)>, String> {
    if cursor.contains("://") || cursor.contains(['#', '?', '\r', '\n']) {
        return Err("Cursor must contain query parameters only".into());
    }
    let pairs: Vec<_> = url::form_urlencoded::parse(cursor.as_bytes())
        .filter(|(key, _)| !matches!(key.as_ref(), "view" | "pageSize"))
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    if pairs.len() > 20 {
        return Err("Cursor contains too many parameters".into());
    }
    Ok(pairs)
}

pub fn tools() -> Vec<Tool> {
    SPECS.iter().map(definition).collect()
}

fn definition(spec: &Spec) -> Tool {
    let properties: Map<String, Value> = spec
        .fields
        .iter()
        .map(|(name, field)| ((*name).to_owned(), schema(*field)))
        .collect();
    let schema = json!({"type":"object", "properties":properties, "required":spec.required, "additionalProperties":false});
    Tool::new(
        spec.name,
        spec.description,
        schema.as_object().cloned().unwrap_or_default(),
    )
    .with_annotations(
        ToolAnnotations::new()
            .read_only(true)
            .destructive(false)
            .idempotent(true)
            .open_world(true),
    )
}

fn schema(field: Field) -> Value {
    let text = |max: usize| json!({"type":"string", "minLength":1, "maxLength":max});
    let number = |min: u64, max: u64| json!({"type":"integer", "minimum":min, "maximum":max});
    match field {
        Field::Query => text(4000),
        Field::Id | Field::Cursor => text(8192),
        Field::Count { max, default } => {
            let mut schema = number(1, max);
            schema["default"] = json!(default);
            schema
        }
        Field::Bound => number(1, 200),
        Field::Offset => number(0, 1_000_000),
        Field::Chars => number(50, 100_000),
        Field::Flag => json!({"type":"boolean","default":false}),
        Field::Mris => {
            json!({"type":"array","minItems":1,"maxItems":100,"items":{"type":"string","minLength":3,"maxLength":300}})
        }
        Field::Day => json!({"type":"string","format":"date"}),
        Field::Time => {
            json!({"type":"string","minLength":10,"maxLength":40,"description":"YYYY-MM-DD or ISO 8601"})
        }
        Field::Sender => text(200),
        Field::Person => text(300),
        Field::Link => text(8192),
    }
}
