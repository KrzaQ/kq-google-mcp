//! The connect flow: the consent URL, the code exchange and userinfo.

use super::*;
use crate::google::oauth;

async fn oauth_client(server: &MockServer) -> oauth::OAuth {
    oauth::OAuth::new(
        crate::google::http_client().unwrap(),
        &google_config(server),
        &"http://localhost:8000".parse().unwrap(),
    )
    .unwrap()
}

#[tokio::test]
async fn the_consent_url_asks_for_offline_access_and_the_right_scopes() {
    let server = MockServer::start().await;
    let oauth = oauth_client(&server).await;
    let authorization = oauth.authorize("work", &[Service::Gmail, Service::Sheets]);

    let query: std::collections::HashMap<_, _> =
        authorization.url.query_pairs().into_owned().collect();
    assert_eq!(authorization.url.path(), "/o/oauth2/v2/auth");
    assert_eq!(query["access_type"], "offline");
    assert_eq!(query["include_granted_scopes"], "true");
    assert_eq!(query["prompt"], "consent select_account");
    assert_eq!(query["code_challenge_method"], "S256");
    assert_eq!(
        query["redirect_uri"],
        "http://localhost:8000/api/google/callback"
    );
    assert!(!query.contains_key("login_hint"));
    assert_eq!(query["state"], authorization.state.csrf);

    // The bundle is the scope registry's, with Drive pulled in by Sheets.
    let scopes: Vec<&str> = query["scope"].split(' ').collect();
    assert!(scopes.contains(&"openid"));
    assert!(scopes.contains(&"https://www.googleapis.com/auth/gmail.modify"));
    assert!(scopes.contains(&"https://www.googleapis.com/auth/spreadsheets"));
    assert!(scopes.contains(&"https://www.googleapis.com/auth/drive.readonly"));

    assert_eq!(authorization.state.label, "work");
    assert_eq!(
        authorization.state.services,
        [Service::Gmail, Service::Sheets]
    );
    assert_eq!(authorization.state.reconnect, None);
    assert!(!authorization.state.pkce_verifier.is_empty());
}

#[tokio::test]
async fn a_reconnect_names_the_account_and_does_not_offer_the_picker() {
    let server = MockServer::start().await;
    let oauth = oauth_client(&server).await;
    let authorization = oauth.reconnect(12, "work", "anna@example.test", &[Service::Gmail]);
    let query: std::collections::HashMap<_, _> =
        authorization.url.query_pairs().into_owned().collect();
    assert_eq!(query["prompt"], "consent");
    assert_eq!(query["login_hint"], "anna@example.test");
    assert_eq!(authorization.state.reconnect, Some(12));
    assert_eq!(authorization.state.label, "work");
}

#[test]
fn the_flow_state_only_accepts_the_state_it_generated() {
    let state = oauth::FlowState {
        csrf: "the-random-token".into(),
        pkce_verifier: "v".into(),
        services: vec![Service::Gmail],
        reconnect: None,
        label: "work".into(),
    };
    assert!(state.check("the-random-token").is_ok());
    assert_eq!(state.check(""), Err(oauth::FlowError::Missing));
    assert_eq!(
        state.check("something else"),
        Err(oauth::FlowError::Mismatch)
    );
    // It survives the cookie it will travel in.
    let json = serde_json::to_string(&state).unwrap();
    assert_eq!(
        serde_json::from_str::<oauth::FlowState>(&json).unwrap(),
        state
    );
}

#[tokio::test]
async fn the_code_exchange_yields_the_refresh_token_and_the_granted_scopes() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("oauth_token.json")))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/oauth2/v3/userinfo"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("userinfo.json")))
        .mount(&server)
        .await;
    let oauth = oauth_client(&server).await;
    let state = oauth
        .authorize("work", &[Service::Gmail, Service::Drive])
        .state;

    let grant = oauth.exchange("4/0Aexample-code", &state).await.unwrap();
    assert_eq!(
        grant.refresh_token.as_deref(),
        Some("1//09exampleRefreshTokenForTests")
    );
    // What Google granted, which is what gets stored and shown as partial
    // when it is narrower than what was asked for.
    assert!(grant.granted_scopes.contains(&"openid".to_string()));
    assert!(
        grant
            .granted_scopes
            .contains(&"https://www.googleapis.com/auth/gmail.modify".to_string())
    );
    assert!(
        !grant
            .granted_scopes
            .contains(&"https://www.googleapis.com/auth/drive.file".to_string())
    );
    assert_eq!(grant.expires_in, Some(std::time::Duration::from_secs(3599)));

    let form = String::from_utf8_lossy(
        &server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .find(|r| r.url.path() == "/token")
            .unwrap()
            .body,
    )
    .into_owned();
    assert!(form.contains("grant_type=authorization_code"), "{form}");
    assert!(form.contains("code_verifier="), "{form}");

    let who = oauth.userinfo(&grant.access_token).await.unwrap();
    assert_eq!(who.email.as_deref(), Some("anna@example.test"));
    assert!(who.email_verified);
    assert_eq!(who.sub, "104729384756102938475");
}

#[tokio::test]
async fn a_refused_code_carries_googles_own_words() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": "invalid_grant",
            "error_description": "Bad Request"
        })))
        .mount(&server)
        .await;
    let oauth = oauth_client(&server).await;
    let state = oauth.authorize("work", &[Service::Gmail]).state;
    let error = oauth.exchange("nope", &state).await.unwrap_err();
    assert!(error.to_string().contains("invalid_grant"), "{error}");
    assert!(error.to_string().contains("Bad Request"), "{error}");
}
