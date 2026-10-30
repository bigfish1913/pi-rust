use super::*;

fn reply(n: usize, action: &str, content: Value) -> String {
    format!("{}\n", json!({"type":"auth_response","requestId":format!("auth-{n}"),"action":action,"content":content}))
}
fn interaction(display: Value) -> AuthResult<OAuthActionResponse> {
    OAuthActionResponse::parse(json!({"kind":"interaction","payload":{"display":display,"opaque":"NEVER_PRINT_OPAQUE"}})).map_err(|_| "Invalid response")
}
fn credential() -> AuthResult<OAuthActionResponse> {
    OAuthActionResponse::parse(json!({"kind":"credential","payload":{"access":"NEVER_PRINT_ACCESS","refresh":"NEVER_PRINT_REFRESH","expires_at":9999999999_i64}})).map_err(|_| "Invalid response")
}
fn lines(out: &[u8]) -> Vec<Value> {
    std::str::from_utf8(out).unwrap().lines().map(|s| {
        assert!(s.len() < LIMIT);
        serde_json::from_str(s).unwrap()
    }).collect()
}
fn no_secrets(out: &[u8]) {
    assert!(!std::str::from_utf8(out).unwrap().contains("NEVER_PRINT"));
}

#[tokio::test]
async fn url_accept_only_exchanges_and_never_authenticates_without_credential() {
    for returns_credential in [false, true] {
        let input = reply(1, "accept", json!({}));
        let mut out = Vec::new();
        let mut calls = Vec::new();
        let mut stored = Vec::new();
        let code = drive("fake", true, input.as_bytes(), &mut out, |action, content| {
            calls.push((action.to_owned(), content));
            if action == "begin" {
                interaction(json!({"url":"https://example.test/authorize","instructions":["Approve in browser"]}))
            } else if returns_credential { credential() }
            else { OAuthActionResponse::parse(json!({"kind":"success"})).map_err(|_| "Invalid response") }
        }, |c| { stored.push(c); Ok(()) }, Instant::now() + DEADLINE).await;
        assert_eq!(calls, vec![("begin".into(), json!({})), ("exchange".into(), json!({}))]);
        assert_eq!(stored.len(), usize::from(returns_credential));
        assert_eq!(code == 0, returns_credential);
        let events = lines(&out);
        assert_eq!(events[0], json!({"type":"auth_ready","provider":"fake","kind":"oauth"}));
        assert_eq!(events[1]["mode"], "url");
        assert_eq!(events.last().unwrap()["authenticated"], returns_credential);
        no_secrets(&out);
    }
}

#[tokio::test]
async fn structured_form_and_fallback_transfer_only_user_content() {
    for explicit_schema in [false, true] {
        let mut out = Vec::new();
        let content = if explicit_schema { json!({"confirmed":true}) } else { json!({"value":"NEVER_PRINT_CODE"}) };
        let input = reply(1, "accept", content.clone());
        let mut exchanged = None;
        let code = drive("fake", true, input.as_bytes(), &mut out, |action, value| {
            if action == "begin" {
                interaction(if explicit_schema { json!({"schema":{"type":"object","properties":{"confirmed":{"type":"boolean"}},"required":["confirmed"]}}) } else { json!({}) })
            } else { exchanged = Some(value); credential() }
        }, |_| Ok(()), Instant::now() + DEADLINE).await;
        assert_eq!(code, 0);
        assert_eq!(exchanged, Some(content));
        assert_eq!(lines(&out)[1]["mode"], "form");
        no_secrets(&out);
    }
}

#[tokio::test]
async fn cancel_decline_eof_malformed_and_wrong_request_never_exchange_or_store() {
    for input in [
        reply(1,"cancel",json!({})), reply(1,"decline",json!({})), String::new(),
        "{bad NEVER_PRINT_ERROR}\n".into(), reply(2,"accept",json!({})),
        reply(1,"accept",json!({})).trim_end().to_owned(),
        format!("{}\n", "x".repeat(LIMIT + 1)),
    ] {
        let mut out = Vec::new();
        let mut calls = 0;
        let code = drive("fake", true, input.as_bytes(), &mut out, |_, _| {
            calls += 1;
            interaction(json!({"url":"https://example.test"}))
        }, |_| panic!("must not persist"), Instant::now() + DEADLINE).await;
        assert_ne!(code, 0);
        assert_eq!(calls, 1);
        assert_eq!(lines(&out).last().unwrap()["authenticated"], false);
        no_secrets(&out);
    }
}

#[tokio::test]
async fn typed_api_key_only_and_native_provider_isolation() {
    let _config = super::super::tests::TempConfig::new();
    let mut out = Vec::new();
    let input = reply(1,"accept",json!({"apiKey":"NEVER_PRINT_KEY"}));
    let code = drive("isolated", false, input.as_bytes(), &mut out, |_,_| panic!("API key must not call OAuth"),
        |c| config::upsert_credential("isolated", c).map_err(|_| "Storage failed"), Instant::now() + DEADLINE).await;
    assert_eq!(code, 0);
    assert!(config::read_auth().unwrap().contains_key("isolated"));
    assert!(!config::read_auth().unwrap().contains_key("anthropic"));
    let events = lines(&out);
    assert_eq!(events[1]["requestedSchema"]["properties"]["apiKey"]["password"], true);
    no_secrets(&out);
    for content in [json!({"apiKey":42}), json!({"apiKey":""}), json!({"apiKey":"key","extra":"NEVER_PRINT_EXTRA"}), json!({})] {
        let input = reply(1,"accept",content);
        let mut out = Vec::new();
        assert_ne!(drive("fake", false, input.as_bytes(), &mut out, |_,_| panic!(), |_| panic!("invalid key persisted"), Instant::now() + DEADLINE).await, 0);
        no_secrets(&out);
    }
}

#[tokio::test]
async fn interaction_schema_instruction_and_deadline_bounds() {
    let input: String = (1..=MAX_INTERACTIONS+1).map(|n| reply(n,"accept",json!({}))).collect();
    let mut out = Vec::new();
    let code = drive("fake", true, input.as_bytes(), &mut out, |_,_| interaction(json!({"url":"https://example.test"})), |_| panic!(), Instant::now()+DEADLINE).await;
    assert_ne!(code,0);
    assert_eq!(lines(&out).iter().filter(|v| v["type"] == "auth_interaction").count(), MAX_INTERACTIONS);
    for display in [json!({"instructions":["x".repeat(LIMIT)]}), json!({"schema":{"type":"object","properties":{},"default":"NEVER_PRINT_DEFAULT"}}), json!({"url":"javascript:NEVER_PRINT_URL"})] {
        let mut out = Vec::new();
        assert_ne!(drive("fake", true, &b""[..], &mut out, |_,_| interaction(display.clone()), |_| panic!(), Instant::now()+DEADLINE).await, 0);
        no_secrets(&out);
        assert_eq!(lines(&out).len(),2);
    }
    let mut out = Vec::new();
    assert_ne!(drive("fake", true, &b""[..], &mut out, |_,_| panic!("expired callback"), |_| panic!(), Instant::now()).await,0);
    let mut out = Vec::new();
    let (_writer, reader) = tokio::io::duplex(64);
    assert_ne!(drive("fake", false, BufReader::new(reader), &mut out, |_,_| panic!(), |_| panic!(), Instant::now()+Duration::from_millis(10)).await,0);
    // Once the common deadline expires even the terminal frame must not block.
    assert_eq!(lines(&out).last().unwrap()["type"], "auth_interaction");
}

#[tokio::test]
async fn provider_errors_and_storage_errors_are_redacted() {
    for storage_error in [true, false] {
        let mut out = Vec::new();
        let code = drive("fake", true, &b""[..], &mut out, |_,_| {
            if storage_error { credential() }
            else { OAuthActionResponse::parse(json!({"kind":"error","payload":{"message":"NEVER_PRINT_PROVIDER_ERROR"}})).map_err(|_| "Invalid response") }
        }, |_| Err("Credential storage failed"), Instant::now()+DEADLINE).await;
        assert_ne!(code,0);
        no_secrets(&out);
    }
}

#[tokio::test]
async fn url_then_code_then_native_credential() {
    let schema = json!({"type":"object","title":"Authorization code","properties":{"code":{"type":"string","title":"Code","minLength":3,"maxLength":40,"password":true}},"required":["code"]});
    let input = reply(1,"accept",json!({})) + &reply(2,"accept",json!({"code":"NEVER_PRINT_CODE"}));
    let mut out = Vec::new();
    let mut calls = 0;
    let mut stored = 0;
    let code = drive("fake", true, input.as_bytes(), &mut out, |action, content| {
        calls += 1;
        match calls {
            1 => { assert_eq!(action,"begin"); interaction(json!({"url":"https://example.test/authorize"})) },
            2 => { assert_eq!(action,"exchange"); assert_eq!(content,json!({})); interaction(json!({"requestedSchema":schema})) },
            3 => { assert_eq!(content,json!({"code":"NEVER_PRINT_CODE","value":"NEVER_PRINT_CODE"})); credential() },
            _ => panic!("unexpected callback"),
        }
    }, |c| { assert!(matches!(c, Credential::Oauth { .. })); stored += 1; Ok(()) }, Instant::now()+DEADLINE).await;
    assert_eq!(code,0);
    assert_eq!((calls,stored),(3,1));
    assert_eq!(lines(&out)[2]["requestedSchema"],schema);
    no_secrets(&out);
}

#[tokio::test]
async fn arbitrary_typed_forms_preserve_and_enforce_constraints() {
    let schema = json!({"type":"object","title":"Provider form","properties":{
        "region":{"type":"string","enum":["east","west"],"title":"Region"},
        "attempt":{"type":"integer","minimum":1,"maximum":3},
        "ratio":{"type":"number","minimum":0.25,"maximum":0.75},
        "label":{"type":"string","minLength":2,"maxLength":3},
        "enabled":{"type":"boolean"}},"required":["region","attempt","ratio","label","enabled"],"additionalProperties":false});
    let good = json!({"region":"east","attempt":2,"ratio":0.5,"label":"é界","enabled":true});
    assert!(valid_schema(&schema,0));
    for (field,bad) in [("region",json!("north")),("attempt",json!(0)),("attempt",json!(4)),("attempt",json!(1.5)),("ratio",json!(0.8)),("label",json!("a")),("label",json!("abcd")),("enabled",json!("true"))] {
        let mut content = good.clone(); content[field] = bad;
        let input = reply(1,"accept",content);
        let mut out = Vec::new();
        assert_ne!(drive("fake",true,input.as_bytes(),&mut out,|action,_| {
            assert_eq!(action,"begin"); interaction(json!({"requestedSchema":schema}))
        }, |_| panic!("invalid form persisted"),Instant::now()+DEADLINE).await,0);
    }
    let input = reply(1,"accept",good.clone());
    let mut out = Vec::new();
    assert_eq!(drive("fake",true,input.as_bytes(),&mut out,|action,content| {
        if action == "begin" { interaction(json!({"requestedSchema":schema})) }
        else { assert_eq!(content,good); credential() }
    }, |_| Ok(()),Instant::now()+DEADLINE).await,0);
    assert_eq!(lines(&out)[1]["requestedSchema"],schema);
    for schema in [json!({"type":"string","minimum":1}),json!({"type":"number","minimum":"1"}),json!({"type":"integer","enum":["wrong"]}),json!({"type":"string","minLength":4,"maxLength":2}),json!({"type":"string","title":42})] {
        assert!(!valid_schema(&schema,0));
    }
}

#[test]
fn parsed_urls_reject_missing_host_and_all_userinfo() {
    for url in ["http://", "https://?q=secret", "https://user@example.test", "https://:secret@example.test", "https://@example.test", "javascript:secret", "https://[broken"] {
        assert!(!valid_url(url),"{url}");
    }
    for url in ["https://example.test/a", "http://localhost:8080/", "https://[::1]/"] { assert!(valid_url(url)); }
    assert_eq!(exchange_input(json!({"apiKey":"secret"})),json!({"apiKey":"secret","value":"secret"}));
    assert_eq!(exchange_input(json!({"value":"secret"})),json!({"value":"secret"}));
}

#[tokio::test]
async fn output_and_input_are_bounded_by_size_and_common_deadline() {
    let mut out = Vec::new();
    assert!(emit(&mut out,&json!("x".repeat(LIMIT)),Instant::now()+DEADLINE).await.is_err());
    assert!(out.is_empty());
    let (mut blocked,_reader) = tokio::io::duplex(1);
    let deadline = Instant::now()+Duration::from_millis(10);
    let result = tokio::time::timeout(Duration::from_secs(1), drive("fake",false,&b""[..],&mut blocked,|_,_| panic!(),|_| panic!(),deadline)).await;
    assert_ne!(result.expect("stdout must not block beyond deadline"),0);
    let input = format!("{}\n","x".repeat(LIMIT));
    assert!(response(&mut input.as_bytes(),"auth-1",Instant::now()+DEADLINE).await.is_err());
    let input = reply(1,"accept",json!({}));
    assert!(response(&mut input.as_bytes(),"auth-1",Instant::now()+DEADLINE).await.is_ok());
}
