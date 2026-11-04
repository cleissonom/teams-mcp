use rmcp::model::{Tool, ToolAnnotations};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::normalize;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ApiRequest {
    pub service: String,
    pub path: String,
    pub method: String,
    pub query: Vec<(String, String)>,
    pub body: Option<Value>,
}

pub struct Operation {
    name: String,
    args: Map<String, Value>,
}

impl Operation {
    pub fn parse(name: &str, args: Value) -> Result<Self, String> {
        let allowed = allowed_fields(name).ok_or("Unknown Teams tool")?;
        let args = args
            .as_object()
            .ok_or("Arguments must be an object")?
            .clone();
        if args.keys().any(|key| !allowed.contains(&key.as_str())) {
            return Err("Unexpected argument; consult the tool's input schema".into());
        }
        let operation = Self {
            name: name.into(),
            args,
        };
        operation.validate()?;
        Ok(operation)
    }

    pub fn requests(&self) -> Vec<ApiRequest> {
        let request = match self.name.as_str() {
            "teams_search" => self.search_request(),
            "teams_search_people" => self.people_request(),
            "teams_find_channel" => request(
                "csa",
                "/api/v3/teams/users/me",
                "GET",
                vec![
                    ("isPrefetch".into(), "false".into()),
                    ("enableMembershipSummary".into(), "true".into()),
                ],
                None,
            ),
            "teams_get_person" => request(
                "profile",
                "/beta/users/fetchShortProfile",
                "POST",
                vec![
                    ("isMailAddress".into(), "false".into()),
                    ("enableGuest".into(), "true".into()),
                    ("skypeTeamsInfo".into(), "true".into()),
                ],
                Some(self.args["mris"].clone()),
            ),
            "teams_get_message" => request(
                "chat",
                &format!(
                    "{}/messages/{}",
                    self.conversation_path(false),
                    segment(self.text("messageId"))
                ),
                "GET",
                vec![],
                None,
            ),
            "teams_get_thread" => self.thread_request(),
            _ => conversations_request(),
        };
        vec![request]
    }

    pub fn finish(&self, responses: Vec<Value>, profile: &Value) -> Result<Value, String> {
        if responses.len() != 1 {
            return Err("Expected exactly one Teams API response".into());
        }
        let response = &responses[0];
        match self.name.as_str() {
            "teams_search" => {
                normalize::search(response, self.number("from", 0), self.search_size())
            }
            "teams_search_people" => normalize::people(response, self.number("limit", 10)),
            "teams_find_channel" => {
                normalize::channels(response, self.text("query"), self.number("limit", 10))
            }
            "teams_get_person" => {
                normalize::profiles(response, self.args["mris"].as_array().map_or(0, Vec::len))
            }
            "teams_list_chats" => normalize::chats(response, self.number("limit", 50)),
            "teams_get_message" => normalize::message(response, self.text("conversationId")),
            "teams_get_thread" => normalize::thread(
                response,
                self.text("conversationId"),
                self.number("limit", 50),
            ),
            "teams_get_unread" => normalize::unread(
                response,
                profile,
                self.args.get("conversationId").and_then(Value::as_str),
            ),
            _ => Err("Unknown Teams tool".into()),
        }
    }

    fn validate(&self) -> Result<(), String> {
        for key in [
            "query",
            "conversationId",
            "messageId",
            "threadRootId",
            "cursor",
        ] {
            if let Some(value) = self.args.get(key) {
                let text = value.as_str().ok_or("Text arguments must be strings")?;
                let max = if key == "query" { 4000 } else { 8192 };
                if text.trim().is_empty()
                    || text.len() > max
                    || text.chars().any(char::is_control)
                    || (key != "query" && matches!(text, "." | ".."))
                {
                    return Err("Text arguments must be nonempty, bounded, and contain no control characters".into());
                }
            }
        }
        self.validate_required()?;
        for key in ["limit", "size", "maxResults", "from"] {
            self.validate_number(key)?;
        }
        if self.args.contains_key("mris") {
            self.validate_mris()?;
        }
        if self.args.contains_key("cursor") {
            cursor_query(self.text("cursor"))?;
        }
        Ok(())
    }

    fn validate_required(&self) -> Result<(), String> {
        let required: &[&str] = match self.name.as_str() {
            "teams_search" | "teams_search_people" | "teams_find_channel" => &["query"],
            "teams_get_person" => &["mris"],
            "teams_get_message" => &["conversationId", "messageId"],
            "teams_get_thread" => &["conversationId"],
            _ => &[],
        };
        if required.iter().any(|key| !self.args.contains_key(*key)) {
            return Err("Missing required argument; consult the tool's input schema".into());
        }
        Ok(())
    }

    fn validate_number(&self, key: &str) -> Result<(), String> {
        let Some(value) = self.args.get(key) else {
            return Ok(());
        };
        let number = value
            .as_u64()
            .ok_or("Pagination arguments must be nonnegative integers")?;
        let valid = if key == "from" {
            number <= 1_000_000
        } else {
            (1..=200).contains(&number)
        };
        if !valid {
            return Err("Page size must be 1–200 and offset at most 1000000".into());
        }
        Ok(())
    }

    fn validate_mris(&self) -> Result<(), String> {
        let mris = self.args["mris"]
            .as_array()
            .ok_or("mris must be an array")?;
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

    fn text(&self, key: &str) -> &str {
        self.args.get(key).and_then(Value::as_str).unwrap_or("")
    }
    fn number(&self, key: &str, default: usize) -> usize {
        self.args
            .get(key)
            .and_then(Value::as_u64)
            .map_or(default, |n| n as usize)
    }
    fn search_size(&self) -> usize {
        self.number("size", 25).min(self.number("maxResults", 200))
    }

    fn search_request(&self) -> ApiRequest {
        let query = self.text("query");
        let body = json!({"entityRequests":[{"entityType":"Message", "contentSources":["Teams"], "propertySet":"Optimized", "fields":["Extension_SkypeSpaces_ConversationPost_Extension_FromSkypeInternalId_String", "Extension_SkypeSpaces_ConversationPost_Extension_ThreadType_String", "Extension_SkypeSpaces_ConversationPost_Extension_SkypeGroupId_String"], "query":{"queryString":format!("{query} AND NOT (isClientSoftDeleted:TRUE)"),"displayQueryString":query}, "from":self.number("from",0),"size":self.search_size(),"topResultsCount":0}], "scenario":{"Name":"powerbar","Dimensions":[{"DimensionName":"QueryType","DimensionValue":"Messages"},{"DimensionName":"FormFactor","DimensionValue":"general.web.reactSearch"}]},"timezone":"UTC"});
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
        let body = json!({"EntityRequests":[{"Query":{"QueryString":query,"DisplayQueryString":query},"EntityType":"People","Size":self.number("limit",10),"Fields":["Id","MRI","DisplayName","EmailAddresses","GivenName","Surname","JobTitle","Department","CompanyName"]}]});
        request(
            "substrate",
            "/search/api/v1/suggestions",
            "POST",
            vec![("scenario".into(), "powerbar".into())],
            Some(body),
        )
    }

    fn conversation_path(&self, thread_root: bool) -> String {
        let id = if thread_root && self.args.contains_key("threadRootId") {
            format!(
                "{};messageid={}",
                self.text("conversationId"),
                self.text("threadRootId")
            )
        } else {
            self.text("conversationId").to_owned()
        };
        format!("/v1/users/ME/conversations/{}", segment(&id))
    }

    fn thread_request(&self) -> ApiRequest {
        let mut query = cursor_query(self.text("cursor")).unwrap_or_default();
        query.push((
            "view".into(),
            "msnp24Equivalent|supportsMessageProperties".into(),
        ));
        query.push(("pageSize".into(), self.number("limit", 50).to_string()));
        request(
            "chat",
            &format!("{}/messages", self.conversation_path(true)),
            "GET",
            query,
            None,
        )
    }
}

fn allowed_fields(name: &str) -> Option<&'static [&'static str]> {
    Some(match name {
        "teams_search" => &["query", "from", "size", "maxResults"],
        "teams_search_people" | "teams_find_channel" => &["query", "limit"],
        "teams_get_person" => &["mris"],
        "teams_list_chats" => &["limit"],
        "teams_get_message" => &["conversationId", "messageId"],
        "teams_get_thread" => &["conversationId", "threadRootId", "limit", "cursor"],
        "teams_get_unread" => &["conversationId"],
        _ => return None,
    })
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

fn conversations_request() -> ApiRequest {
    request(
        "chat",
        "/v1/users/ME/conversations",
        "GET",
        vec![
            ("view".into(), "msnp24Equivalent".into()),
            ("pageSize".into(), "200".into()),
        ],
        None,
    )
}

fn segment(value: &str) -> String {
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
    vec![
        definition(
            "teams_search",
            "Search Teams messages. Supports Teams search operators. Returns message IDs and offset pagination.",
            &["query"],
            json!({"query":text_schema(4000),"from":number_schema(0,1000000),"size":number_schema(1,200),"maxResults":number_schema(1,200)}),
        ),
        definition(
            "teams_search_people",
            "Find people by name or email.",
            &["query"],
            json!({"query":text_schema(4000),"limit":number_schema(1,200)}),
        ),
        definition(
            "teams_find_channel",
            "Find matching channels among teams you belong to. Organisation-wide discovery is not included.",
            &["query"],
            json!({"query":text_schema(4000),"limit":number_schema(1,200)}),
        ),
        definition(
            "teams_get_person",
            "Resolve 1–100 Teams MRIs to people profiles.",
            &["mris"],
            json!({"mris":{"type":"array","minItems":1,"maxItems":100,"items":{"type":"string","minLength":3,"maxLength":300}}}),
        ),
        definition(
            "teams_list_chats",
            "List up to 200 recent conversations, including chats, meetings and channels.",
            &[],
            json!({"limit":number_schema(1,200)}),
        ),
        definition(
            "teams_get_message",
            "Read one message by conversation and message ID.",
            &["conversationId", "messageId"],
            json!({"conversationId":text_schema(8192),"messageId":text_schema(8192)}),
        ),
        definition(
            "teams_get_thread",
            "Read a page of conversation messages without marking them read. Use threadRootId for channel replies; pass olderCursor back as cursor to read older messages.",
            &["conversationId"],
            json!({"conversationId":text_schema(8192),"threadRootId":text_schema(8192),"limit":number_schema(1,200),"cursor":text_schema(8192)}),
        ),
        definition(
            "teams_get_unread",
            "Inspect unread status within the 200 most recent conversations. Missing read positions and self-reply gaps are reported as unknown; this does not provide an exact unread message count.",
            &[],
            json!({"conversationId":text_schema(8192)}),
        ),
    ]
}

fn definition(
    name: &'static str,
    description: &'static str,
    required: &[&str],
    properties: Value,
) -> Tool {
    let schema = json!({"type":"object", "properties":properties, "required":required, "additionalProperties":false});
    Tool::new(name, description, schema.as_object().unwrap().clone()).with_annotations(
        ToolAnnotations::new()
            .read_only(true)
            .destructive(false)
            .idempotent(true)
            .open_world(true),
    )
}
fn text_schema(max: usize) -> Value {
    json!({"type":"string", "minLength":1, "maxLength":max})
}
fn number_schema(min: usize, max: usize) -> Value {
    json!({"type":"integer", "minimum":min, "maximum":max})
}
