use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Map, Value, json};
use std::collections::HashMap;

/// Per-call options for message output.
#[derive(Default)]
pub struct Options {
    pub max_chars: Option<usize>,
    pub since: Option<i64>,
    pub until: Option<i64>,
}

fn array<'a>(value: &'a Value, key: &str) -> Result<&'a Vec<Value>, String> {
    value
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("Teams API response has an incompatible {key} shape"))
}
fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}
fn require_id(value: &Value, key: &str) -> Result<(), String> {
    if text(value, key).is_empty() {
        return Err("Teams API response item is missing its identifier".into());
    }
    Ok(())
}
fn first_text<'a>(value: &'a Value, keys: &[&str]) -> &'a str {
    keys.iter()
        .map(|key| text(value, key))
        .find(|text| !text.is_empty())
        .unwrap_or("")
}
/// Teams stores some message properties as JSON strings.
fn embedded(value: &Value) -> Value {
    match value {
        Value::String(text) => serde_json::from_str(text).unwrap_or(Value::Null),
        other => other.clone(),
    }
}
/// A numeric or string identifier as text.
fn id_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) if !text.is_empty() => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}
fn truthy(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(flag) => Some(*flag),
        Value::String(text) => text.parse::<bool>().ok().or(match text.as_str() {
            "True" => Some(true),
            "False" => Some(false),
            _ => None,
        }),
        _ => None,
    }
}
/// `8:orgid:<id>` from a Teams contact URL or MRI.
fn mri(value: &str) -> &str {
    value
        .rsplit_once("/contacts/")
        .map_or(value, |(_, mri)| mri)
}
fn arrival(message: &Value) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(first_text(
        message,
        &["originalarrivaltime", "composetime"],
    ))
    .ok()
    .map(|time| time.timestamp_millis())
}
/// A batch item's data when its request succeeded.
fn batch_data(item: Option<&Value>) -> Option<&Value> {
    item.filter(|item| item["ok"] == true)
        .map(|item| &item["data"])
}

/// Removes null and empty-string fields recursively to keep tool output small.
pub fn compact(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .filter(|(_, value)| !value.is_null() && value.as_str() != Some(""))
                .map(|(key, value)| (key, compact(value)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.into_iter().map(compact).collect()),
        other => other,
    }
}

pub fn web_url(conversation: &str, message: &str, root: Option<&str>) -> String {
    let mut url = format!(
        "https://teams.microsoft.com/l/message/{}/{message}",
        crate::operations::segment(conversation)
    );
    if let Some(root) = root.filter(|root| *root != message) {
        url.push_str(&format!("?parentMessageId={root}"));
    }
    url
}

pub fn search(response: &Value, from: usize, size: usize) -> Result<Value, String> {
    let mut results = Vec::new();
    let mut total = None;
    let mut reported_more = None;
    for entity in array(response, "EntitySets")? {
        for set in array(entity, "ResultSets")? {
            let set_total = ["Total", "TotalCount", "TotalEstimate"]
                .iter()
                .find_map(|key| set[key].as_u64());
            total = set_total.or(total);
            if let Some(more) = set["MoreResultsAvailable"].as_bool() {
                reported_more = Some(reported_more.unwrap_or(false) || more);
            }
            let items = match set.get("Results") {
                None if set_total == Some(0) => &[][..],
                _ => array(set, "Results")?.as_slice(),
            };
            for item in items {
                results.push(search_item(item));
            }
        }
    }
    results.truncate(size);
    let has_more = !results.is_empty()
        && reported_more.unwrap_or_else(|| {
            total.map_or(results.len() >= size, |total| from + size < total as usize)
        });
    Ok(
        json!({"results":results,"pagination":{"from":from,"size":size,"returned":results.len(),"total":total,"hasMore":has_more,"nextFrom":has_more.then_some(from+size)}}),
    )
}

fn search_item(item: &Value) -> Value {
    let source = &item["Source"];
    let conversation = first_text(source, &["ClientThreadId", "ClientConversationId"]);
    let conversation = if conversation.is_empty() {
        text(
            &source["Extensions"],
            "SkypeSpaces_ConversationPost_Extension_SkypeGroupId",
        )
    } else {
        conversation.split(';').next().unwrap_or(conversation)
    };
    let timestamp = first_text(
        source,
        &[
            "DateTimeReceived",
            "DateTimeSent",
            "DateTimeCreated",
            "ReceivedTime",
            "CreatedDateTime",
        ],
    );
    let message_id = search_message_id(source, timestamp);
    let root = text(source, "ClientConversationId")
        .split_once(";messageid=")
        .map(|(_, root)| root.split(';').next().unwrap_or(root))
        .filter(|root| *root != message_id);
    let sender = match source.get("From").or_else(|| source.get("Sender")) {
        Some(Value::Object(from)) => {
            json!({"name":from["EmailAddress"]["Name"],"email":from["EmailAddress"]["Address"]})
        }
        Some(Value::String(name)) => json!({"name":name}),
        _ => Value::Null,
    };
    let link = (!conversation.is_empty() && !message_id.is_empty())
        .then(|| web_url(conversation, &message_id, root));
    json!({"id":first_text(item,&["Id","ReferenceId"]),"messageId":message_id,"threadRootId":root,"conversationId":conversation,"content":plain_text(first_text(item,&["HitHighlightedSummary","Summary"])),"sender":sender,"timestamp":timestamp,"channelName":first_text(source,&["ChannelName","Topic"]),"teamName":first_text(source,&["TeamName","GroupName"]),"webUrl":link})
}

fn search_message_id(source: &Value, timestamp: &str) -> String {
    if let Ok(timestamp) = chrono::DateTime::parse_from_rfc3339(timestamp) {
        return timestamp.timestamp_millis().to_string();
    }
    first_text(
        source,
        &[
            "MessageId",
            "OriginalMessageId",
            "ReferenceObjectId",
            "ClientMessageId",
        ],
    )
    .to_owned()
}

pub fn people(response: &Value, limit: usize) -> Result<Value, String> {
    let mut results = Vec::new();
    for group in array(response, "Groups")? {
        for person in array(group, "Suggestions")? {
            require_id(person, "Id")?;
            let id = person_id(text(person, "Id"));
            let mri = person_id(text(person, "MRI"))
                .or_else(|| id.clone())
                .map(|id| format!("8:orgid:{id}"));
            results.push(json!({"id":id.unwrap_or_else(||text(person,"Id").to_owned()),"mri":mri,"displayName":person["DisplayName"],"email":person["EmailAddresses"].get(0),"givenName":person["GivenName"],"surname":person["Surname"],"jobTitle":person["JobTitle"],"department":person["Department"],"companyName":person["CompanyName"]}));
        }
    }
    results.truncate(limit);
    Ok(json!({"results":results,"returned":results.len()}))
}

fn person_id(id: &str) -> Option<String> {
    let id = id
        .strip_prefix("8:orgid:")
        .or_else(|| id.strip_prefix("orgid:"))
        .unwrap_or(id)
        .split('@')
        .next()?;
    if id.len() == 36
        && id.chars().enumerate().all(|(i, c)| {
            if [8, 13, 18, 23].contains(&i) {
                c == '-'
            } else {
                c.is_ascii_hexdigit()
            }
        })
    {
        return Some(id.to_ascii_lowercase());
    }
    let bytes = STANDARD.decode(id).ok()?;
    if bytes.len() != 16 {
        return None;
    }
    let order = [3, 2, 1, 0, 5, 4, 7, 6, 8, 9, 10, 11, 12, 13, 14, 15];
    let mut guid = String::new();
    for (position, index) in order.into_iter().enumerate() {
        if [4, 6, 8, 10].contains(&position) {
            guid.push('-');
        }
        guid.push_str(&format!("{:02x}", bytes[index]));
    }
    Some(guid)
}

pub fn channels(response: &Value, query: &str, limit: usize) -> Result<Value, String> {
    let mut results = Vec::new();
    let query = query.to_lowercase();
    for team in array(response, "teams")? {
        for channel in array(team, "channels")? {
            require_id(channel, "id")?;
            if !text(channel, "displayName").to_lowercase().contains(&query) {
                continue;
            }
            results.push(json!({"channelId":channel["id"],"channelName":channel["displayName"],"teamName":team["displayName"],"teamId":channel.get("groupId").or_else(||team.get("id")),"channelType":channel["channelType"],"description":channel["description"],"isMember":true}));
        }
    }
    results.truncate(limit);
    Ok(json!({"results":results,"returned":results.len(),"scope":"member-teams-only"}))
}

pub fn teams(response: &Value, limit: usize) -> Result<Value, String> {
    let all = array(response, "teams")?;
    let mut results = Vec::new();
    for team in all.iter().take(limit) {
        require_id(team, "id")?;
        let mut channels = Vec::new();
        for channel in array(team, "channels")? {
            require_id(channel, "id")?;
            channels.push(json!({"channelId":channel["id"],"name":channel["displayName"]}));
        }
        results.push(json!({"teamId":team["id"],"name":team["displayName"],"channels":channels}));
    }
    Ok(json!({"teams":results,"returned":results.len(),"total":all.len()}))
}

pub fn profiles(response: &Value, requested: usize) -> Result<Value, String> {
    for person in array(response, "value")? {
        require_id(person, "mri")?;
    }
    let results: Vec<Value> = array(response,"value")?.iter().map(|person| json!({"mri":person["mri"],"displayName":person["displayName"],"email":person.get("email").or_else(||person.get("userPrincipalName")),"jobTitle":person["jobTitle"],"department":person["department"],"companyName":person.get("companyName").or_else(||person.get("tenantName"))})).collect();
    Ok(
        json!({"profiles":results,"returned":results.len(),"unresolved":requested.saturating_sub(results.len())}),
    )
}

pub fn chats(response: &Value, limit: usize) -> Result<Value, String> {
    for conversation in array(response, "conversations")?.iter().take(limit) {
        require_id(conversation, "id")?;
    }
    let conversations: Vec<Value> = array(response,"conversations")?.iter().take(limit).map(|conversation| {
        let last = &conversation["lastMessage"];
        json!({"conversationId":conversation["id"],"topic":conversation["threadProperties"]["topic"],"chatType":chat_type(text(conversation,"id")),"lastMessageId":last["id"],"lastMessageTime":first_text(last,&["originalarrivaltime","composetime"]),"lastMessageFrom":last["imdisplayname"],"lastMessagePreview":message_content(last).chars().take(120).collect::<String>()})
    }).collect();
    Ok(
        json!({"conversations":conversations,"returned":conversations.len(),"coverage":{"scope":"recent-conversations","maxConversations":200,"complete":false}}),
    )
}

/// One message. `standalone` adds the conversation ID and link, which a thread states once.
pub fn message(
    response: &Value,
    conversation: &str,
    options: &Options,
    standalone: bool,
) -> Result<Value, String> {
    let id = text(response, "id");
    if id.is_empty() {
        return Err("Teams message response is missing its ID".into());
    }
    let root = text(response, "rootMessageId");
    let root = (!root.is_empty() && root != id).then_some(root);
    let mut content = message_content(response);
    let truncated = options
        .max_chars
        .is_some_and(|max| truncate(&mut content, max));
    let kind = text(response, "messagetype");
    let mut value = json!({"id":id,"content":content,"sender":{"mri":mri(text(response,"from")),"displayName":response.get("imdisplayname").or_else(||response.get("displayName"))},"timestamp":first_text(response,&["originalarrivaltime","composetime"]),"threadRootId":root});
    let fields = value
        .as_object_mut()
        .ok_or("Message serialization failed")?;
    if standalone {
        fields.insert("conversationId".into(), json!(conversation));
        fields.insert("webUrl".into(), json!(web_url(conversation, id, root)));
    }
    if !matches!(kind, "" | "Text" | "RichText/Html") {
        fields.insert("contentType".into(), json!(kind));
    }
    if truncated {
        fields.insert("truncated".into(), json!(true));
    }
    enrich(fields, &response["properties"]);
    Ok(value)
}

/// Adds compact reactions, mentions, files, subject and edit/reply markers when present.
fn enrich(fields: &mut Map<String, Value>, properties: &Value) {
    let reactions: Map<String, Value> = embedded(&properties["emotions"])
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|reaction| {
            let count = reaction["users"].as_array().map_or(0, Vec::len);
            (count > 0).then(|| (text(reaction, "key").to_owned(), json!(count)))
        })
        .filter(|(key, _)| !key.is_empty())
        .collect();
    if !reactions.is_empty() {
        fields.insert("reactions".into(), Value::Object(reactions));
    }
    let mut mentions: Vec<&str> = Vec::new();
    let embedded_mentions = embedded(&properties["mentions"]);
    for mention in embedded_mentions.as_array().into_iter().flatten() {
        let name = text(mention, "displayName");
        if !name.is_empty() && !mentions.contains(&name) {
            mentions.push(name);
        }
    }
    if !mentions.is_empty() {
        fields.insert("mentions".into(), json!(mentions));
    }
    let files: Vec<Value> = embedded(&properties["files"])
        .as_array()
        .into_iter()
        .flatten()
        .map(file)
        .collect();
    if !files.is_empty() {
        fields.insert("files".into(), json!(files));
    }
    for (key, name) in [("subject", "subject"), ("importance", "importance")] {
        let value = text(properties, key);
        if !matches!(value, "" | "normal") {
            fields.insert(name.into(), json!(value));
        }
    }
    if !text(properties, "edittime").is_empty() {
        fields.insert("edited".into(), json!(true));
    }
    if let Some(quoted) = embedded(&properties["qtdMsgs"])
        .as_array()
        .and_then(|quoted| quoted.first())
        .and_then(|quoted| id_text(&quoted["messageId"]))
    {
        fields.insert("replyTo".into(), json!(quoted));
    }
    let cards: Vec<&str> = properties["cards"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|card| text(card, "appName"))
        .collect();
    if !cards.is_empty() {
        fields.insert("cards".into(), json!(cards));
    }
}

fn file(file: &Value) -> Value {
    let info = &file["fileInfo"];
    let url = [
        text(info, "shareUrl"),
        text(file, "objectUrl"),
        text(info, "fileUrl"),
    ]
    .into_iter()
    .find(|url| !url.is_empty());
    json!({"name":first_text(file,&["fileName","title"]),"type":file["fileType"],"url":url})
}

/// Truncates to `max` characters; returns whether anything was removed.
fn truncate(content: &mut String, max: usize) -> bool {
    match content.char_indices().nth(max) {
        Some((index, _)) => {
            content.truncate(index);
            true
        }
        None => false,
    }
}

fn message_content(response: &Value) -> String {
    if text(response, "messagetype") == "Text" {
        text(response, "content").to_owned()
    } else {
        plain_text(text(response, "content"))
    }
}

fn readable(raw: &Value) -> bool {
    let kind = text(raw, "messagetype");
    !kind.starts_with("Control/")
        && !kind.starts_with("ThreadActivity/")
        && raw["properties"]["deletetime"].is_null()
}

pub fn thread(
    response: &Value,
    conversation: &str,
    limit: usize,
    options: &Options,
) -> Result<Value, String> {
    let mut messages = Vec::new();
    for raw in array(response, "messages")? {
        let time = arrival(raw);
        let outside = time.is_some_and(|time| {
            options.since.is_some_and(|since| time < since)
                || options.until.is_some_and(|until| time > until)
        });
        if readable(raw) && !outside {
            messages.push(message(raw, conversation, options, false)?);
        }
    }
    messages.truncate(limit);
    let cursor = backward_cursor(text(&response["_metadata"], "backwardLink"));
    Ok(
        json!({"conversationId":conversation,"messages":messages,"returned":messages.len(),"olderCursor":cursor}),
    )
}

fn backward_cursor(link: &str) -> Option<String> {
    let url = url::Url::parse(link).ok()?;
    let query: Vec<_> = url
        .query_pairs()
        .filter(|(key, _)| !matches!(key.as_ref(), "view" | "pageSize"))
        .collect();
    if query.is_empty() {
        return None;
    }
    Some(
        url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(query)
            .finish(),
    )
}

/// Activity-feed entries from Teams' notifications, mentions or followed-thread streams.
pub fn activity(response: &Value, limit: usize) -> Result<Value, String> {
    let mut items = Vec::new();
    for raw in array(response, "messages")? {
        let activity = embedded(&raw["properties"]["activity"]);
        if !activity.is_object() {
            continue;
        }
        let message = id_text(&activity["sourceMessageId"]);
        let root = id_text(&activity["sourceReplyChainId"])
            .filter(|root| root != "0" && Some(root) != message.as_ref());
        let preview: String = plain_text(text(&activity, "messagePreview"))
            .chars()
            .take(200)
            .collect();
        items.push(json!({"type":activity["activityType"],"subtype":activity["activitySubtype"],
            "time":activity["activityTimestamp"],
            "from":activity["sourceUserImDisplayName"],"fromMri":activity["sourceUserId"],
            "conversationId":activity["sourceThreadId"],"topic":activity["sourceThreadTopic"],
            "messageId":message,"threadRootId":root,"preview":preview,"read":truthy(&raw["properties"]["isread"])}));
    }
    items.truncate(limit);
    let cursor = backward_cursor(text(&response["_metadata"], "backwardLink"));
    Ok(json!({"activities":items,"returned":items.len(),"olderCursor":cursor}))
}

pub fn favorite_ids(folders: &Value) -> Result<impl Iterator<Item = &str>, String> {
    Ok(array(folders, "conversationFolders")?
        .iter()
        .filter(|folder| folder["folderType"] == "Favorites")
        .flat_map(|folder| {
            folder["conversationFolderItems"]
                .as_array()
                .into_iter()
                .flatten()
        })
        .filter_map(|item| item["conversationId"].as_str()))
}

pub fn favorites(folders: &Value, conversations: Option<&Value>) -> Result<Value, String> {
    let known: HashMap<&str, &Value> = batch_data(conversations)
        .and_then(|data| data["conversations"].as_array())
        .into_iter()
        .flatten()
        .map(|conversation| (text(conversation, "id"), conversation))
        .collect();
    let results: Vec<Value> = favorite_ids(folders)?
        .map(|id| {
            let conversation = known.get(id).copied().unwrap_or(&Value::Null);
            let last = &conversation["lastMessage"];
            json!({"conversationId":id,"topic":conversation["threadProperties"]["topic"],"chatType":chat_type(id),
                "lastMessageTime":first_text(last,&["originalarrivaltime","composetime"]),"lastMessageFrom":last["imdisplayname"]})
        })
        .collect();
    Ok(json!({"favorites":results,"returned":results.len()}))
}

/// The two user IDs of a 1:1 chat ID (`19:<a>_<b>@unq.gbl.spaces`).
fn one_on_one_members(id: &str) -> Option<(&str, &str)> {
    id.strip_prefix("19:")?
        .strip_suffix("@unq.gbl.spaces")?
        .split_once('_')
}

pub fn one_on_one(conversations: &Value, profile: &Value, person: &str) -> Result<Value, String> {
    let me = text(profile, "id").to_ascii_lowercase();
    let other = crate::operations::person_id(person).ok_or("Invalid person")?;
    if other == me {
        return Ok(json!({"found":true,"conversationId":"48:notes","chatType":"self"}));
    }
    for conversation in array(conversations, "conversations")? {
        let id = text(conversation, "id");
        if let Some((a, b)) = one_on_one_members(id) {
            let (a, b) = (a.to_ascii_lowercase(), b.to_ascii_lowercase());
            if (a == me && b == other) || (a == other && b == me) {
                let last = &conversation["lastMessage"];
                return Ok(
                    json!({"found":true,"conversationId":id,"lastMessageTime":first_text(last,&["originalarrivaltime","composetime"])}),
                );
            }
        }
    }
    let (first, second) = if me <= other {
        (&me, &other)
    } else {
        (&other, &me)
    };
    Ok(
        json!({"found":false,"candidates":[format!("19:{first}_{second}@unq.gbl.spaces"),format!("19:{second}_{first}@unq.gbl.spaces")],
        "note":"No 1:1 chat with this person among the 200 most recent conversations. teams_get_thread accepts a candidate ID if one exists."}),
    )
}

struct Contact {
    score: u32,
    last: i64,
    name: String,
}

fn rank_contacts(conversations: &Value, profile: &Value) -> Result<Vec<(String, Contact)>, String> {
    let me = text(profile, "id").to_ascii_lowercase();
    let mut tally: HashMap<String, Contact> = HashMap::new();
    let mut add = |id: &str, weight: u32, time: i64, name: &str| {
        let id = id.to_ascii_lowercase();
        if id.is_empty() || id == me || crate::operations::person_id(&id).is_none() {
            return;
        }
        let entry = tally.entry(id).or_insert(Contact {
            score: 0,
            last: 0,
            name: String::new(),
        });
        entry.score += weight;
        entry.last = entry.last.max(time);
        if entry.name.is_empty() {
            entry.name = name.to_owned();
        }
    };
    for conversation in array(conversations, "conversations")? {
        let id = text(conversation, "id");
        let last = &conversation["lastMessage"];
        let time = arrival(last).unwrap_or(0);
        if let Some((a, b)) = one_on_one_members(id) {
            add(a, 3, time, "");
            add(b, 3, time, "");
        }
        if let Some(sender) = mri(text(last, "from")).strip_prefix("8:orgid:") {
            add(sender, 1, time, text(last, "imdisplayname"));
        }
    }
    let mut ranked: Vec<_> = tally.into_iter().collect();
    ranked.sort_by(|(_, a), (_, b)| b.score.cmp(&a.score).then(b.last.cmp(&a.last)));
    Ok(ranked)
}

pub fn contact_candidates(
    conversations: &Value,
    profile: &Value,
    limit: usize,
) -> Result<Vec<String>, String> {
    Ok(rank_contacts(conversations, profile)?
        .into_iter()
        .take(limit)
        .map(|(id, _)| format!("8:orgid:{id}"))
        .collect())
}

pub fn contacts(
    conversations: &Value,
    profile: &Value,
    limit: usize,
    profiles: Option<&Value>,
) -> Result<Value, String> {
    let known: HashMap<String, &Value> = batch_data(profiles)
        .and_then(|data| data["value"].as_array())
        .into_iter()
        .flatten()
        .map(|person| (text(person, "mri").to_ascii_lowercase(), person))
        .collect();
    let results: Vec<Value> = rank_contacts(conversations, profile)?
        .into_iter()
        .take(limit)
        .map(|(id, contact)| {
            let mri = format!("8:orgid:{id}");
            let person = known.get(&mri).copied().unwrap_or(&Value::Null);
            let name = first_text(person, &["displayName"]);
            let time = chrono::DateTime::from_timestamp_millis(contact.last)
                .filter(|_| contact.last > 0)
                .map(|time| time.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
            json!({"mri":mri,"displayName":if name.is_empty() { contact.name.as_str() } else { name },
                "email":person.get("email").or_else(||person.get("userPrincipalName")),"lastInteraction":time})
        })
        .collect();
    Ok(json!({"contacts":results,"returned":results.len(),"basis":"recent-conversations"}))
}

pub fn shared_files(response: &Value, conversation: &str) -> Result<Value, String> {
    let messages = array(response, "messages")?;
    let mut files = Vec::new();
    let mut links = Vec::new();
    for raw in messages.iter().filter(|raw| readable(raw)) {
        let origin = |mut value: Value| {
            value["sharedBy"] = raw["imdisplayname"].clone();
            value["time"] = json!(first_text(raw, &["originalarrivaltime", "composetime"]));
            value["messageId"] = raw["id"].clone();
            value
        };
        for item in embedded(&raw["properties"]["files"])
            .as_array()
            .into_iter()
            .flatten()
        {
            files.push(origin(file(item)));
        }
        for item in embedded(&raw["properties"]["links"])
            .as_array()
            .into_iter()
            .flatten()
        {
            if !text(item, "url").is_empty() {
                links.push(origin(json!({"url":item["url"]})));
            }
        }
    }
    let cursor = backward_cursor(text(&response["_metadata"], "backwardLink"));
    Ok(
        json!({"conversationId":conversation,"files":files,"links":links,"scannedMessages":messages.len(),"olderCursor":cursor}),
    )
}

/// Read position of a conversation with messages newer than it, from someone else.
enum ReadState<'a> {
    Read,
    Empty,
    Unknown(&'static str),
    Pending { id: &'a str, read: u64 },
}

fn classify<'a>(conversation: &'a Value, profile: &Value) -> ReadState<'a> {
    let id = text(conversation, "id");
    let last = &conversation["lastMessage"];
    if last.as_object().is_none_or(Map::is_empty)
        || truthy(&conversation["properties"]["isemptyconversation"]) == Some(true)
    {
        return ReadState::Empty;
    }
    let read = text(&conversation["properties"], "consumptionhorizon")
        .split(';')
        .next()
        .and_then(|value| value.parse::<u64>().ok());
    let timestamp = text(last, "id").parse::<u64>().ok();
    let Some((read, timestamp)) = read.zip(timestamp) else {
        return ReadState::Unknown("missing-or-invalid-read-position");
    };
    if timestamp <= read {
        return ReadState::Read;
    }
    let me = text(profile, "mri");
    if me.is_empty() || text(last, "from").rsplit('/').next() == Some(me) {
        return ReadState::Unknown("self-reply-gap-or-unknown-current-user");
    }
    ReadState::Pending { id, read }
}

fn selected<'a>(
    response: &'a Value,
    conversation_id: Option<&str>,
) -> Result<Vec<&'a Value>, String> {
    let conversations = array(response, "conversations")?;
    for conversation in conversations.iter().take(200) {
        require_id(conversation, "id")?;
    }
    Ok(conversations
        .iter()
        .take(200)
        .filter(|conversation| conversation_id.is_none_or(|id| text(conversation, "id") == id))
        .collect())
}

/// Unread conversations and their read positions, in response order.
pub fn unread_targets<'a>(
    response: &'a Value,
    profile: &Value,
    conversation_id: Option<&str>,
) -> Result<Vec<(&'a str, u64)>, String> {
    Ok(selected(response, conversation_id)?
        .into_iter()
        .filter_map(|conversation| match classify(conversation, profile) {
            ReadState::Pending { id, read } => Some((id, read)),
            _ => None,
        })
        .collect())
}

pub fn unread(
    response: &Value,
    profile: &Value,
    conversation_id: Option<&str>,
    messages: &[Value],
    message_limit: usize,
) -> Result<Value, String> {
    let selected = selected(response, conversation_id)?;
    let fetched: HashMap<&str, &Value> = unread_targets(response, profile, conversation_id)?
        .into_iter()
        .map(|(id, _)| id)
        .zip(messages)
        .collect();
    let mut chats = Vec::new();
    let mut channels = Vec::new();
    let mut unknown = Vec::new();
    let mut empty = 0;
    for conversation in &selected {
        match classify(conversation, profile) {
            ReadState::Read => {}
            ReadState::Empty => empty += 1,
            ReadState::Unknown(reason) => {
                unknown.push(json!({"conversationId":text(conversation,"id"),"reason":reason}))
            }
            ReadState::Pending { id, read } => {
                let last = &conversation["lastMessage"];
                let mut entry = json!({"conversationId":id,"displayName":conversation["threadProperties"]["topic"],"lastMessageFrom":last["imdisplayname"],"lastMessageId":last["id"],"readUpTo":read});
                if let Some(item) = fetched.get(id) {
                    attach_unread(&mut entry, item, id, profile, message_limit);
                }
                if chat_type(id) == "channel" {
                    channels.push(entry);
                } else {
                    chats.push(entry);
                }
            }
        }
    }
    if conversation_id.is_some() && selected.is_empty() {
        unknown.push(
            json!({"conversationId":conversation_id,"reason":"conversation-not-in-recent-200"}),
        );
    }
    Ok(
        json!({"unreadChats":chats,"unreadChannels":channels,"unknown":unknown,"emptyConversations":empty,"totalChecked":selected.len(),"coverage":{"scope":"recent-conversations","maxConversations":200,"complete":false,"exactUnreadCountAvailable":!messages.is_empty()}}),
    )
}

fn attach_unread(
    entry: &mut Value,
    item: &Value,
    conversation: &str,
    profile: &Value,
    limit: usize,
) {
    let Some(data) = batch_data(Some(item)) else {
        entry["messagesError"] = item["error"]["code"].clone();
        return;
    };
    let me = text(profile, "mri");
    let raw = data["messages"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let options = Options::default();
    let unread: Vec<Value> = raw
        .iter()
        .filter(|message| readable(message) && mri(text(message, "from")) != me)
        .filter_map(|message| self::message(message, conversation, &options, false).ok())
        .collect();
    entry["unreadCount"] = json!(unread.len());
    if raw.len() >= limit {
        entry["countIsLowerBound"] = json!(true);
    }
    entry["messages"] = json!(unread);
}

fn chat_type(id: &str) -> &'static str {
    if id.contains("meeting_") {
        "meeting"
    } else if id.contains("@thread.tacv2") {
        "channel"
    } else if id.contains("@unq.gbl.spaces") {
        "oneOnOne"
    } else if id.contains("@thread.v2") {
        "group"
    } else {
        "chat"
    }
}

fn plain_text(html: &str) -> String {
    let mut text = String::new();
    let mut remaining = html;
    while let Some(start) = remaining.find('<') {
        append_html_text(&mut text, &remaining[..start]);
        remaining = &remaining[start..];
        if let Some((length, name)) = html_tag(remaining) {
            append_tag_boundary(&mut text, &name);
            remaining = &remaining[length..];
        } else {
            text.push('<');
            remaining = &remaining[1..];
        }
    }
    append_html_text(&mut text, remaining);
    text.replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
        .trim()
        .to_owned()
}

fn html_tag(source: &str) -> Option<(usize, String)> {
    if source.starts_with("<!--") {
        return source.find("-->").map(|end| (end + 3, String::new()));
    }
    let body = source.strip_prefix('<')?;
    let name_start = body.strip_prefix('/').unwrap_or(body);
    if !name_start.starts_with(|character: char| character.is_ascii_alphabetic()) {
        return None;
    }
    let name = name_start
        .split(|character: char| !character.is_ascii_alphanumeric())
        .next()?
        .to_ascii_lowercase();
    let mut quote = None;
    for (index, character) in body.char_indices() {
        match character {
            '\'' | '"' if quote == Some(character) => quote = None,
            '\'' | '"' if quote.is_none() => quote = Some(character),
            '>' if quote.is_none() => return Some((index + 2, name)),
            _ => {}
        }
    }
    None
}

fn append_html_text(output: &mut String, text: &str) {
    for character in text.chars() {
        if matches!(character, '\r' | '\n') && output.ends_with('\n') {
            continue;
        }
        output.push(character);
    }
}

fn append_tag_boundary(output: &mut String, tag: &str) {
    if tag == "br" {
        output.push('\n');
        return;
    }
    if matches!(
        tag,
        "p" | "div"
            | "li"
            | "ul"
            | "ol"
            | "blockquote"
            | "pre"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "tr"
    ) {
        while output.ends_with([' ', '\t', '\r']) {
            output.pop();
        }
        if !output.is_empty() && !output.ends_with('\n') {
            output.push('\n');
        }
    } else if matches!(tag, "td" | "th")
        && !output.ends_with(char::is_whitespace)
        && !output.is_empty()
    {
        output.push(' ');
    }
}
