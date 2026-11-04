use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};

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
        .map(|(_, root)| root.split(';').next().unwrap_or(root));
    json!({"id":first_text(item,&["Id","ReferenceId"]),"messageId":message_id,"threadRootId":root.filter(|root| *root != message_id),"conversationId":conversation,"content":plain_text(first_text(item,&["HitHighlightedSummary","Summary"])),"sender":source.get("From").or_else(||source.get("Sender")),"timestamp":timestamp,"channelName":first_text(source,&["ChannelName","Topic"]),"teamName":first_text(source,&["TeamName","GroupName"])})
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

pub fn message(response: &Value, conversation: &str) -> Result<Value, String> {
    if text(response, "id").is_empty() {
        return Err("Teams message response is missing its ID".into());
    }
    let id = text(response, "id");
    let root = text(response, "rootMessageId");
    Ok(
        json!({"id":id,"conversationId":conversation,"content":message_content(response),"contentType":response["messagetype"],"sender":{"mri":response["from"],"displayName":response.get("imdisplayname").or_else(||response.get("displayName"))},"timestamp":first_text(response,&["originalarrivaltime","composetime"]),"threadRootId":(!root.is_empty() && root!=id).then_some(root),"isThreadReply":!root.is_empty() && root!=id,"reactions":response["properties"]["emotions"]}),
    )
}

fn message_content(response: &Value) -> String {
    if text(response, "messagetype") == "Text" {
        text(response, "content").to_owned()
    } else {
        plain_text(text(response, "content"))
    }
}

pub fn thread(response: &Value, conversation: &str, limit: usize) -> Result<Value, String> {
    let mut messages = Vec::new();
    for raw in array(response, "messages")? {
        let kind = text(raw, "messagetype");
        if kind.starts_with("Control/")
            || kind.starts_with("ThreadActivity/")
            || !raw["properties"]["deletetime"].is_null()
        {
            continue;
        }
        messages.push(message(raw, conversation)?);
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

pub fn unread(
    response: &Value,
    profile: &Value,
    conversation_id: Option<&str>,
) -> Result<Value, String> {
    let conversations = array(response, "conversations")?;
    for conversation in conversations.iter().take(200) {
        require_id(conversation, "id")?;
    }
    let mut chats = Vec::new();
    let mut channels = Vec::new();
    let mut unknown = Vec::new();
    let selected: Vec<_> = conversations
        .iter()
        .take(200)
        .filter(|conversation| conversation_id.is_none_or(|id| text(conversation, "id") == id))
        .collect();
    for conversation in &selected {
        classify_unread(
            conversation,
            profile,
            &mut chats,
            &mut channels,
            &mut unknown,
        );
    }
    if conversation_id.is_some() && selected.is_empty() {
        unknown.push(
            json!({"conversationId":conversation_id,"reason":"conversation-not-in-recent-200"}),
        );
    }
    Ok(
        json!({"unreadChats":chats,"unreadChannels":channels,"unknown":unknown,"totalChecked":selected.len(),"coverage":{"scope":"recent-conversations","maxConversations":200,"complete":false,"exactUnreadCountAvailable":false}}),
    )
}

fn classify_unread(
    conversation: &Value,
    profile: &Value,
    chats: &mut Vec<Value>,
    channels: &mut Vec<Value>,
    unknown: &mut Vec<Value>,
) {
    let id = text(conversation, "id");
    let last = &conversation["lastMessage"];
    let read = text(&conversation["properties"], "consumptionhorizon")
        .split(';')
        .next()
        .and_then(|value| value.parse::<u64>().ok());
    let timestamp = text(last, "id").parse::<u64>().ok();
    let Some((read, timestamp)) = read.zip(timestamp) else {
        unknown.push(json!({"conversationId":id,"reason":"missing-or-invalid-read-position"}));
        return;
    };
    if timestamp <= read {
        return;
    }
    let mri = text(profile, "mri");
    if mri.is_empty() || text(last, "from").rsplit('/').next() == Some(mri) {
        unknown
            .push(json!({"conversationId":id,"reason":"self-reply-gap-or-unknown-current-user"}));
        return;
    }
    let entry = json!({"conversationId":id,"displayName":conversation["threadProperties"]["topic"],"lastMessageFrom":last["imdisplayname"],"lastMessageId":last["id"],"readUpTo":read});
    if chat_type(id) == "channel" {
        channels.push(entry);
    } else {
        chats.push(entry);
    }
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
