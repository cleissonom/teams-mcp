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
    assert_eq!(tools.len(), 8);
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
