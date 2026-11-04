#[path = "../src/normalize.rs"]
mod normalize;
#[path = "../src/operations.rs"]
mod operations;

use operations::{Operation, tools};
use serde_json::{Value, json};

#[test]
fn short_search_messages_and_source_identifiers_are_preserved() {
    let operation =
        Operation::parse("teams_search", json!({"query":"ok","from":20,"size":5})).unwrap();
    let request = &operation.requests()[0];
    assert_eq!(request.service, "substrate");
    assert_eq!(
        request.body.as_ref().unwrap()["entityRequests"][0]["from"],
        20
    );
    let response = json!({"EntitySets":[{"ResultSets":[{"Total":31,"Results":[{
        "Id":"search-id","HitHighlightedSummary":"Hi","Source":{
            "ClientThreadId":"19:chat@thread.v2","MessageId":"1760000000000","From":"Example Person"}}]}]}]});
    let result = operation.finish(vec![response], &Value::Null).unwrap();
    assert_eq!(result["results"][0]["content"], "Hi");
    assert_eq!(result["results"][0]["conversationId"], "19:chat@thread.v2");
    assert_eq!(result["results"][0]["messageId"], "1760000000000");
    assert_eq!(result["pagination"]["hasMore"], true);
    assert_eq!(result["pagination"]["nextFrom"], 25);
}

#[test]
fn identifiers_cannot_escape_authenticated_conversation_paths() {
    let operation = Operation::parse(
        "teams_get_message",
        json!({
        "conversationId":"19:a/../../evil?host=bad#x", "messageId":"123/../456"}),
    )
    .unwrap();
    let requests = operation.requests();
    assert!(
        requests[0]
            .path
            .contains("19%3Aa%2F..%2F..%2Fevil%3Fhost%3Dbad%23x")
    );
    assert!(requests[0].path.ends_with("123%2F..%2F456"));
    assert!(
        Operation::parse(
            "teams_get_thread",
            json!({"conversationId":"19:chat", "cursor":"https://attacker.invalid/?secret=yes"})
        )
        .is_err()
    );
}

#[test]
fn invalid_and_unbounded_arguments_are_rejected() {
    for args in [
        json!({"query":""}),
        json!({"query":"q","size":201}),
        json!({"query":"q","from":-1}),
        json!({"query":"q","size":1.5}),
        json!({"query":"q","maxResults":0}),
        json!({"query":"q","unknown":true}),
    ] {
        assert!(Operation::parse("teams_search", args).is_err());
    }
    assert!(Operation::parse("teams_get_person", json!({"mris":[]})).is_err());
    assert!(Operation::parse("teams_list_chats", json!({"limit":201})).is_err());
    assert!(
        Operation::parse(
            "teams_get_thread",
            json!({"conversationId":"c", "markRead":true})
        )
        .is_err()
    );
    assert!(Operation::parse("teams_delete_message", json!({})).is_err());
}

#[test]
fn thread_pagination_uses_query_data_and_preserves_reply_roots() {
    let operation = Operation::parse("teams_get_thread", json!({"conversationId":"19:channel", "threadRootId":"100", "limit":10, "cursor":"endTime=999&pageSize=5000&view=bad"})).unwrap();
    let request = &operation.requests()[0];
    assert!(request.path.contains("19%3Achannel%3Bmessageid%3D100"));
    assert!(request.query.contains(&("pageSize".into(), "10".into())));
    assert!(
        !request
            .query
            .iter()
            .any(|(k, v)| k == "pageSize" && v == "5000")
    );
    let response = json!({"messages":[{"id":"101","rootMessageId":"100","messagetype":"RichText/Html","content":"<p>Reply &amp; test</p>"}],"_metadata":{"backwardLink":"https://teams.microsoft.com/api/chatsvc/amer/v1/users/ME/conversations/channel/messages?view=x&pageSize=10&endTime=88"}});
    let result = operation.finish(vec![response], &Value::Null).unwrap();
    assert_eq!(result["messages"][0]["threadRootId"], "100");
    assert_eq!(result["messages"][0]["content"], "Reply & test");
    assert_eq!(result["olderCursor"], "endTime=88");
}

#[test]
fn changed_api_shapes_return_an_error_instead_of_empty_success() {
    for name in [
        "teams_search",
        "teams_search_people",
        "teams_find_channel",
        "teams_list_chats",
        "teams_get_thread",
        "teams_get_unread",
        "teams_get_person",
    ] {
        let args = match name {
            "teams_search" | "teams_search_people" | "teams_find_channel" => {
                json!({"query":"example"})
            }
            "teams_get_thread" => json!({"conversationId":"19:chat"}),
            "teams_get_person" => json!({"mris":["8:orgid:example"]}),
            _ => json!({}),
        };
        let operation = Operation::parse(name, args).unwrap();
        assert!(
            operation
                .finish(vec![json!({"newShape":[]})], &Value::Null)
                .is_err(),
            "{name}"
        );
    }
}

#[test]
fn unread_reports_missing_horizons_as_unknown_and_bounds_coverage() {
    let operation = Operation::parse("teams_get_unread", json!({})).unwrap();
    let result = operation.finish(vec![json!({"conversations":[
        {"id":"unread","properties":{"consumptionhorizon":"100;100;100"},"lastMessage":{"id":"200","from":"8:orgid:other"}},
        {"id":"unknown","lastMessage":{"id":"200","from":"8:orgid:other"}},
        {"id":"self","properties":{"consumptionhorizon":"100;100;100"},"lastMessage":{"id":"200","from":"8:orgid:self"}}
    ]})],&json!({"mri":"8:orgid:self"})).unwrap();
    assert_eq!(result["unreadChats"].as_array().unwrap().len(), 1);
    assert_eq!(result["unknown"].as_array().unwrap().len(), 2);
    assert_eq!(result["coverage"]["complete"], false);
    assert_eq!(result["coverage"]["maxConversations"], 200);
}

#[test]
fn exposed_tools_describe_read_only_strict_object_schemas() {
    let tools = tools();
    assert_eq!(tools.len(), 16);
    for tool in tools {
        let json = serde_json::to_value(tool).unwrap();
        assert_eq!(json["annotations"]["readOnlyHint"], true);
        assert_eq!(json["inputSchema"]["type"], "object");
        assert_eq!(json["inputSchema"]["additionalProperties"], false);
    }
}

#[test]
fn search_uses_the_reply_timestamp_instead_of_the_parent_message_id() {
    let operation = Operation::parse("teams_search", json!({"query":"reply"})).unwrap();
    let result = operation.finish(vec![json!({"EntitySets":[{"ResultSets":[{"Results":[{
        "Id":"hit", "HitHighlightedSummary":"reply", "Source":{
            "DateTimeReceived":"2026-01-20T14:30:00.000Z", "ClientConversationId":"19:channel@thread.tacv2;messageid=1768910000000"}}
    ]}]}]})],&Value::Null).unwrap();
    assert_eq!(result["results"][0]["messageId"], "1768919400000");
    assert_eq!(result["results"][0]["threadRootId"], "1768910000000");
}

#[test]
fn people_search_normalizes_microsoft_guid_formats_for_follow_up_profile_calls() {
    // Distinct synthetic bytes exercise Microsoft's little-endian GUID encoding.
    let operation = Operation::parse("teams_search_people", json!({"query":"example"})).unwrap();
    let result = operation.finish(vec![json!({"Groups":[{"Suggestions":[
        {"Id":"MyIRAFVEd2aImaq7zN3u/w==", "DisplayName":"Example"},
        {"Id":"00112233-4455-6677-8899-aabbccddeeff@tenant.example", "DisplayName":"Example"}
    ]}]})],&Value::Null).unwrap();
    for person in result["results"].as_array().unwrap() {
        assert_eq!(
            person["mri"],
            "8:orgid:00112233-4455-6677-8899-aabbccddeeff"
        );
    }
}

#[test]
fn literal_comparisons_in_plain_messages_are_not_treated_as_html_tags() {
    let operation = Operation::parse(
        "teams_get_message",
        json!({"conversationId":"c","messageId":"1"}),
    )
    .unwrap();
    let result = operation
        .finish(
            vec![json!({"id":"1","content":"if a < b && b > c","messagetype":"Text"})],
            &Value::Null,
        )
        .unwrap();
    assert_eq!(result["content"], "if a < b && b > c");
    assert!(
        Operation::parse(
            "teams_get_message",
            json!({"conversationId":"c","messageId":".."})
        )
        .is_err()
    );
}

#[test]
fn chat_previews_preserve_plain_text_comparisons() {
    let operation = Operation::parse("teams_list_chats", json!({})).unwrap();
    let result = operation.finish(vec![json!({"conversations":[{"id":"19:c","lastMessage":{"id":"1","content":"if a < b && b > c","messagetype":"Text"}}]})],&Value::Null).unwrap();
    assert_eq!(
        result["conversations"][0]["lastMessagePreview"],
        "if a < b && b > c"
    );
}

#[test]
fn malformed_item_identifiers_fail_instead_of_returning_unusable_results() {
    let cases = [
        (
            "teams_search_people",
            json!({"query":"name"}),
            json!({"Groups":[{"Suggestions":[{"DisplayName":"Name"}]}]}),
        ),
        (
            "teams_find_channel",
            json!({"query":"channel"}),
            json!({"teams":[{"id":"team","displayName":"Team","channels":[{"displayName":"Channel"}]}]}),
        ),
        (
            "teams_get_person",
            json!({"mris":["8:orgid:example"]}),
            json!({"value":[{"displayName":"Name"}]}),
        ),
        (
            "teams_list_chats",
            json!({}),
            json!({"conversations":[{"threadProperties":{"topic":"Chat"}}]}),
        ),
    ];
    for (name, args, response) in cases {
        assert!(
            Operation::parse(name, args)
                .unwrap()
                .finish(vec![response], &Value::Null)
                .is_err(),
            "{name}"
        );
    }
}

#[test]
fn unread_does_not_exceed_its_declared_recent_conversation_boundary() {
    let conversations:Vec<_> = (0..201).map(|i|json!({"id":i.to_string(),"properties":{"consumptionhorizon":"100;100;100"},"lastMessage":{"id":"200","from":"8:orgid:other"}})).collect();
    let operation = Operation::parse("teams_get_unread", json!({})).unwrap();
    let result = operation
        .finish(
            vec![json!({"conversations":conversations})],
            &json!({"mri":"8:orgid:self"}),
        )
        .unwrap();
    assert_eq!(result["totalChecked"], 200);
    assert_eq!(result["unreadChats"].as_array().unwrap().len(), 200);
}

#[test]
fn zero_result_search_may_omit_results_but_nonempty_or_unknown_shapes_fail() {
    let operation = Operation::parse("teams_search", json!({"query":"absent"})).unwrap();
    let result = operation
        .finish(
            vec![json!({"EntitySets":[{"ResultSets":[{"Total":0,"MoreResultsAvailable":false}]}]})],
            &Value::Null,
        )
        .unwrap();
    assert_eq!(result["results"], json!([]));
    assert_eq!(result["pagination"]["total"], 0);
    assert_eq!(result["pagination"]["hasMore"], false);
    for set in [
        json!({"Total":1}),
        json!({}),
        json!({"Total":0,"Results":null}),
        json!({"Total":0,"Results":{}}),
    ] {
        assert!(
            operation
                .finish(
                    vec![json!({"EntitySets":[{"ResultSets":[set]}]})],
                    &Value::Null
                )
                .is_err()
        );
    }
}

#[test]
fn explicit_api_more_results_flag_controls_nonempty_search_pagination() {
    let operation = Operation::parse("teams_search", json!({"query":"term","size":1})).unwrap();
    for available in [false, true] {
        let result = operation.finish(vec![json!({"EntitySets":[{"ResultSets":[{"Results":[{"Id":"result","Summary":"Hi"}],"MoreResultsAvailable":available}]}]})],&Value::Null).unwrap();
        assert_eq!(result["pagination"]["hasMore"], available);
    }
}

#[test]
fn rich_text_block_and_break_boundaries_preserve_message_structure() {
    let operation = Operation::parse(
        "teams_get_message",
        json!({"conversationId":"c","messageId":"1"}),
    )
    .unwrap();
    for (html, expected) in [
        ("<p>first</p><p>second</p>", "first\nsecond"),
        ("one<br>two", "one\ntwo"),
        ("<div>one<br/>two</div><div>three</div>", "one\ntwo\nthree"),
        ("<p>Hello <strong>team</strong>!</p>", "Hello team!"),
        ("<p>one</p>\n<p>two</p>", "one\ntwo"),
    ] {
        let result = operation
            .finish(
                vec![json!({"id":"1","content":html,"messagetype":"RichText/Html"})],
                &Value::Null,
            )
            .unwrap();
        assert_eq!(result["content"], expected, "{html}");
    }
}

fn query(request: &operations::ApiRequest, key: &str) -> Option<String> {
    request
        .query
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.clone())
}

#[test]
fn page_sizes_follow_requested_limits_and_lean_defaults() {
    let page = |name: &str, args: Value| {
        query(
            &Operation::parse(name, args).unwrap().requests()[0],
            "pageSize",
        )
    };
    assert_eq!(
        page("teams_list_chats", json!({"limit":5})).as_deref(),
        Some("5")
    );
    assert_eq!(page("teams_list_chats", json!({})).as_deref(), Some("20"));
    assert_eq!(
        page("teams_get_thread", json!({"conversationId":"19:c"})).as_deref(),
        Some("20")
    );
    // Unread keeps its declared 200-conversation coverage.
    assert_eq!(page("teams_get_unread", json!({})).as_deref(), Some("200"));
    let search = Operation::parse("teams_search", json!({"query":"q"})).unwrap();
    assert_eq!(
        search.requests()[0].body.as_ref().unwrap()["entityRequests"][0]["size"],
        10
    );
}

#[test]
fn thread_messages_are_compact_and_keep_useful_metadata() {
    let operation = Operation::parse(
        "teams_get_thread",
        json!({"conversationId":"19:c@thread.v2","maxContentChars":50}),
    )
    .unwrap();
    let long = "x".repeat(80);
    let response = json!({"messages":[{"id":"2","messagetype":"RichText/Html","content":format!("<p>{long}</p>"),
        "from":"https://apac.ng.msg.teams.microsoft.com/v1/users/ME/contacts/8:orgid:aaaaaaaa-0000-4000-8000-000000000001",
        "imdisplayname":"Example Sender","originalarrivaltime":"2026-11-04T10:00:00.000Z",
        "properties":{"emotions":[{"key":"like","users":[{"mri":"8:orgid:x"},{"mri":"8:orgid:y"}]}],
            "mentions":"[{\"displayName\":\"Example Person\",\"mri\":\"8:orgid:z\"}]",
            "files":[{"fileName":"plan.docx","fileType":"docx","fileInfo":{"shareUrl":"https://example.sharepoint.test/plan"}}],
            "edittime":"1762250000000","qtdMsgs":[{"messageId":1}],"importance":"normal"}},
        {"id":"1","messagetype":"Control/Typing","content":""}]});
    let result = operation.finish(vec![response], &Value::Null).unwrap();
    let message = &result["messages"][0];
    assert_eq!(result["returned"], 1);
    assert_eq!(message["content"].as_str().unwrap().chars().count(), 50);
    assert_eq!(message["truncated"], true);
    assert_eq!(
        message["sender"]["mri"],
        "8:orgid:aaaaaaaa-0000-4000-8000-000000000001"
    );
    assert_eq!(message["reactions"], json!({"like":2}));
    assert_eq!(message["mentions"], json!(["Example Person"]));
    assert_eq!(
        message["files"][0]["url"],
        "https://example.sharepoint.test/plan"
    );
    assert_eq!(message["edited"], true);
    assert_eq!(message["replyTo"], "1");
    for absent in [
        "conversationId",
        "contentType",
        "isThreadReply",
        "threadRootId",
        "importance",
    ] {
        assert!(message.get(absent).is_none(), "{absent}");
    }
    assert!(
        result.get("olderCursor").is_none(),
        "null fields are omitted"
    );
}

#[test]
fn thread_time_window_uses_start_time_and_filters_the_end() {
    let operation = Operation::parse(
        "teams_get_thread",
        json!({"conversationId":"19:c","since":"2026-11-01","until":"2026-11-02T00:00:00Z"}),
    )
    .unwrap();
    assert_eq!(
        query(&operation.requests()[0], "startTime").as_deref(),
        Some("1793491200000")
    );
    let response = json!({"messages":[
        {"id":"3","messagetype":"Text","content":"late","originalarrivaltime":"2026-11-03T00:00:00Z"},
        {"id":"2","messagetype":"Text","content":"inside","originalarrivaltime":"2026-11-01T12:00:00Z"}]});
    let result = operation.finish(vec![response], &Value::Null).unwrap();
    assert_eq!(result["messages"].as_array().unwrap().len(), 1);
    assert_eq!(result["messages"][0]["content"], "inside");
    for args in [
        json!({"conversationId":"c","since":"yesterday"}),
        json!({"conversationId":"c","until":"2026-13-01"}),
    ] {
        assert!(Operation::parse("teams_get_thread", args).is_err());
    }
}

#[test]
fn search_filters_compile_to_validated_kql_and_results_are_linkable() {
    let operation = Operation::parse(
        "teams_search",
        json!({"query":"budget","sender":"Example Person","after":"2026-10-01","before":"2026-11-01"}),
    )
    .unwrap();
    let body = operation.requests()[0].body.clone().unwrap();
    assert_eq!(
        body["entityRequests"][0]["query"]["queryString"],
        "budget AND from:\"Example Person\" AND sent>=2026-10-01 AND sent<=2026-11-01 AND NOT (isClientSoftDeleted:TRUE)"
    );
    assert_eq!(
        body["entityRequests"][0]["query"]["displayQueryString"],
        "budget"
    );
    for args in [
        json!({"query":"q","sender":"x\" OR y"}),
        json!({"query":"q","after":"2026/10/01"}),
    ] {
        assert!(Operation::parse("teams_search", args).is_err());
    }
    let result = operation.finish(vec![json!({"EntitySets":[{"ResultSets":[{"Results":[{"Id":"r","Summary":"s","Source":{
        "ClientConversationId":"19:c@thread.tacv2;messageid=100","DateTimeReceived":"2026-10-02T00:00:00Z",
        "From":{"EmailAddress":{"Name":"Example Person","Address":"person@example.test"}}}}]}]}]})],&Value::Null).unwrap();
    let item = &result["results"][0];
    assert_eq!(
        item["sender"],
        json!({"name":"Example Person","email":"person@example.test"})
    );
    assert_eq!(
        item["webUrl"],
        "https://teams.microsoft.com/l/message/19%3Ac%40thread.tacv2/1790899200000?parentMessageId=100"
    );
}

#[test]
fn activity_and_followed_threads_read_teams_feed_conversations() {
    let path = |args: Value| {
        Operation::parse("teams_get_activity", args)
            .unwrap()
            .requests()[0]
            .path
            .clone()
    };
    assert!(path(json!({})).ends_with("/48%3Anotifications/messages"));
    assert!(path(json!({"mentionsOnly":true})).ends_with("/48%3Amentions/messages"));
    let followed = Operation::parse("teams_get_followed_threads", json!({})).unwrap();
    assert!(
        followed.requests()[0]
            .path
            .ends_with("/48%3Athreads/messages")
    );
    let response = json!({"messages":[{"id":"1","properties":{"isread":"false","activity":"{\"activityType\":\"mention\",\"activityTimestamp\":\"2026-11-04T09:00:00Z\",\"sourceThreadId\":\"19:c@thread.v2\",\"sourceMessageId\":42,\"sourceReplyChainId\":0,\"sourceUserImDisplayName\":\"Example\",\"messagePreview\":\"<p>Hi there</p>\"}"}},{"id":"2","properties":{}}]});
    let result = Operation::parse("teams_get_activity", json!({}))
        .unwrap()
        .finish(vec![response], &Value::Null)
        .unwrap();
    assert_eq!(result["returned"], 1);
    let item = &result["activities"][0];
    assert_eq!(
        (
            item["type"].as_str(),
            item["messageId"].as_str(),
            item["preview"].as_str()
        ),
        (Some("mention"), Some("42"), Some("Hi there"))
    );
    assert_eq!(item["read"], false);
    assert!(item.get("threadRootId").is_none());
    assert!(
        Operation::parse("teams_get_activity", json!({}))
            .unwrap()
            .finish(vec![json!({})], &Value::Null)
            .is_err()
    );
}

#[test]
fn favorites_fetch_topics_only_when_pinned_conversations_exist() {
    let operation = Operation::parse("teams_get_favorites", json!({})).unwrap();
    assert_eq!(
        operation.requests()[0].path,
        "/api/v1/teams/users/me/conversationFolders"
    );
    let folders = json!({"conversationFolders":[{"folderType":"Favorites","conversationFolderItems":[{"conversationId":"19:a@thread.v2"},{"conversationId":"19:b@thread.v2"}]},{"folderType":"QuickViews","conversationFolderItems":[{"conversationId":"19:x"}]}]});
    assert_eq!(
        operation
            .follow_up(std::slice::from_ref(&folders), &Value::Null)
            .unwrap()
            .len(),
        1
    );
    assert!(
        operation
            .follow_up(&[json!({"conversationFolders":[]})], &Value::Null)
            .unwrap()
            .is_empty()
    );
    let topics = json!({"ok":true,"data":{"conversations":[{"id":"19:a@thread.v2","threadProperties":{"topic":"Planning"}}]}});
    let result = operation
        .finish(vec![folders.clone(), topics], &Value::Null)
        .unwrap();
    assert_eq!(result["returned"], 2);
    assert_eq!(result["favorites"][0]["topic"], "Planning");
    let degraded = operation
        .finish(
            vec![folders, json!({"ok":false,"error":{"code":"API_ERROR"}})],
            &Value::Null,
        )
        .unwrap();
    assert_eq!(degraded["favorites"][1]["conversationId"], "19:b@thread.v2");
}

#[test]
fn one_on_one_chat_is_found_in_either_member_order_or_reported_with_candidates() {
    let me = "aaaaaaaa-0000-4000-8000-000000000001";
    let other = "bbbbbbbb-0000-4000-8000-000000000002";
    let profile = json!({"id":me});
    let conversations = json!({"conversations":[{"id":format!("19:{other}_{me}@unq.gbl.spaces"),"lastMessage":{}}]});
    let found = Operation::parse(
        "teams_get_chat",
        json!({"person":format!("8:orgid:{other}")}),
    )
    .unwrap()
    .finish(vec![conversations], &profile)
    .unwrap();
    assert_eq!(
        found["conversationId"],
        format!("19:{other}_{me}@unq.gbl.spaces")
    );
    let missing = Operation::parse("teams_get_chat", json!({"person":other}))
        .unwrap()
        .finish(vec![json!({"conversations":[]})], &profile)
        .unwrap();
    assert_eq!(missing["found"], false);
    assert_eq!(
        missing["candidates"][0],
        format!("19:{me}_{other}@unq.gbl.spaces")
    );
    let me_chat = Operation::parse("teams_get_chat", json!({"person":me}))
        .unwrap()
        .finish(vec![json!({"conversations":[]})], &profile)
        .unwrap();
    assert_eq!(me_chat["conversationId"], "48:notes");
    assert!(Operation::parse("teams_get_chat", json!({"person":"someone@example.test"})).is_err());
}

#[test]
fn teams_are_listed_with_compact_channels() {
    let response = json!({"teams":[{"id":"t1","displayName":"Team","channels":[{"id":"19:c@thread.tacv2","displayName":"General","description":"long"}]},{"id":"t2","displayName":"Other","channels":[]}]});
    let result = Operation::parse("teams_list_teams", json!({"limit":1}))
        .unwrap()
        .finish(vec![response], &Value::Null)
        .unwrap();
    assert_eq!(
        result,
        json!({"teams":[{"teamId":"t1","name":"Team","channels":[{"channelId":"19:c@thread.tacv2","name":"General"}]}],"returned":1,"total":2})
    );
}

#[test]
fn frequent_contacts_rank_recent_conversations_then_resolve_names_in_one_request() {
    let me = "aaaaaaaa-0000-4000-8000-000000000001";
    let close = "bbbbbbbb-0000-4000-8000-000000000002";
    let other = "cccccccc-0000-4000-8000-000000000003";
    let profile = json!({"id":me,"mri":format!("8:orgid:{me}")});
    let conversations = json!({"conversations":[
        {"id":format!("19:{me}_{close}@unq.gbl.spaces"),"lastMessage":{"from":format!("https://x.test/v1/users/ME/contacts/8:orgid:{close}"),"imdisplayname":"Close","originalarrivaltime":"2026-11-04T00:00:00Z"}},
        {"id":"19:group@thread.v2","lastMessage":{"from":format!("https://x.test/v1/users/ME/contacts/8:orgid:{other}"),"imdisplayname":"Other","originalarrivaltime":"2026-11-03T00:00:00Z"}},
        {"id":"48:notifications","lastMessage":{}}]});
    let operation = Operation::parse("teams_get_frequent_contacts", json!({"limit":2})).unwrap();
    let follow = operation
        .follow_up(std::slice::from_ref(&conversations), &profile)
        .unwrap();
    assert_eq!(follow[0].path, "/beta/users/fetchShortProfile");
    assert_eq!(
        follow[0].body,
        Some(json!([
            format!("8:orgid:{close}"),
            format!("8:orgid:{other}")
        ]))
    );
    let profiles = json!({"ok":true,"data":{"value":[{"mri":format!("8:orgid:{close}"),"displayName":"Close Colleague","email":"close@example.test"}]}});
    let result = operation
        .finish(vec![conversations, profiles], &profile)
        .unwrap();
    assert_eq!(result["contacts"][0]["displayName"], "Close Colleague");
    assert_eq!(result["contacts"][0]["email"], "close@example.test");
    assert_eq!(result["contacts"][1]["displayName"], "Other");
}

#[test]
fn shared_files_and_links_are_extracted_from_message_properties() {
    let response = json!({"messages":[{"id":"9","imdisplayname":"Sharer","originalarrivaltime":"2026-11-04T00:00:00Z","messagetype":"RichText/Html","properties":{
        "files":"[{\"fileName\":\"plan.docx\",\"fileType\":\"docx\",\"objectUrl\":\"https://example.sharepoint.test/plan\"}]",
        "links":[{"url":"https://example.test/doc"},{"url":""}]}}]});
    let result = Operation::parse("teams_get_shared_files", json!({"conversationId":"19:c"}))
        .unwrap()
        .finish(vec![response], &Value::Null)
        .unwrap();
    assert_eq!(
        result["files"][0],
        json!({"name":"plan.docx","type":"docx","url":"https://example.sharepoint.test/plan","sharedBy":"Sharer","time":"2026-11-04T00:00:00Z","messageId":"9"})
    );
    assert_eq!(result["links"].as_array().unwrap().len(), 1);
    assert_eq!(result["scannedMessages"], 1);
}

#[test]
fn message_links_are_parsed_only_for_exact_teams_hosts() {
    let link = "https://teams.microsoft.com/l/message/19:abc@thread.tacv2/1700000000002?tenantId=t&parentMessageId=1700000000001";
    let operation = Operation::parse("teams_open_link", json!({"url":link})).unwrap();
    assert!(
        operation.requests()[0]
            .path
            .ends_with("/19%3Aabc%40thread.tacv2/messages/1700000000002")
    );
    let result = operation.finish(vec![json!({"id":"1700000000002","rootMessageId":"1700000000001","messagetype":"Text","content":"hi"})], &Value::Null).unwrap();
    assert_eq!(result["conversationId"], "19:abc@thread.tacv2");
    assert_eq!(
        result["webUrl"],
        "https://teams.microsoft.com/l/message/19%3Aabc%40thread.tacv2/1700000000002?parentMessageId=1700000000001"
    );
    for url in [
        "https://teams.microsoft.com.attacker.test/l/message/19:a/1",
        "http://teams.microsoft.com/l/message/19:a/1",
        "https://user@teams.microsoft.com/l/message/19:a/1",
        "https://teams.microsoft.com/l/chat/19:a/1",
        "https://teams.microsoft.com/l/message/../1",
        "https://teams.microsoft.com/l/message/19:a/not-a-number",
    ] {
        assert!(
            Operation::parse("teams_open_link", json!({"url":url})).is_err(),
            "{url}"
        );
    }
}

#[test]
fn unread_skips_empty_conversations_and_optionally_counts_unread_messages() {
    let profile = json!({"mri":"8:orgid:self"});
    let response = json!({"conversations":[
        {"id":"19:a@thread.v2","properties":{"consumptionhorizon":"100;100;100"},"lastMessage":{"id":"300","from":"8:orgid:other"}},
        {"id":"19:b@thread.tacv2","properties":{"consumptionhorizon":"100;100;100"},"lastMessage":{"id":"200","from":"8:orgid:other"}},
        {"id":"19:empty@thread.tacv2","properties":{"isemptyconversation":"True"},"lastMessage":{}}]});
    let plain = Operation::parse("teams_get_unread", json!({})).unwrap();
    assert!(
        plain
            .follow_up(std::slice::from_ref(&response), &profile)
            .unwrap()
            .is_empty()
    );
    let result = plain.finish(vec![response.clone()], &profile).unwrap();
    assert_eq!(result["unknown"], json!([]));
    assert_eq!(result["emptyConversations"], 1);
    let operation = Operation::parse(
        "teams_get_unread",
        json!({"includeMessages":true,"messageLimit":2,"maxConversations":1}),
    )
    .unwrap();
    let follow = operation
        .follow_up(std::slice::from_ref(&response), &profile)
        .unwrap();
    assert_eq!(follow.len(), 1);
    assert_eq!(query(&follow[0], "startTime").as_deref(), Some("101"));
    assert_eq!(query(&follow[0], "pageSize").as_deref(), Some("2"));
    let messages = json!({"ok":true,"data":{"messages":[
        {"id":"300","messagetype":"Text","content":"new","from":"https://x.test/contacts/8:orgid:other"},
        {"id":"250","messagetype":"Text","content":"mine","from":"https://x.test/contacts/8:orgid:self"}]}});
    let result = operation
        .finish(vec![response, messages], &profile)
        .unwrap();
    let chat = &result["unreadChats"][0];
    assert_eq!(chat["unreadCount"], 1);
    assert_eq!(chat["countIsLowerBound"], true);
    assert_eq!(chat["messages"][0]["content"], "new");
    assert!(result["unreadChannels"][0].get("messages").is_none());
    assert_eq!(result["coverage"]["exactUnreadCountAvailable"], true);
}

#[test]
fn new_feed_tools_fail_on_changed_shapes() {
    for (name, args) in [
        ("teams_get_activity", json!({})),
        ("teams_get_followed_threads", json!({})),
        ("teams_get_favorites", json!({})),
        ("teams_list_teams", json!({})),
        ("teams_get_frequent_contacts", json!({})),
        ("teams_get_shared_files", json!({"conversationId":"19:c"})),
        (
            "teams_get_chat",
            json!({"person":"aaaaaaaa-0000-4000-8000-000000000001"}),
        ),
    ] {
        assert!(
            Operation::parse(name, args)
                .unwrap()
                .finish(vec![json!({"newShape":[]})], &json!({"id":"x"}))
                .is_err(),
            "{name}"
        );
    }
}
