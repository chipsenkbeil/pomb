use super::Client;

#[test]
fn from_vars_should_name_the_variable_that_is_missing() {
    let missing_url = Client::from_vars(None, Some("pomb_secret".to_string())).expect_err("url");
    assert_eq!(missing_url.to_string(), "POMB_URL is not set");

    let blank_url = Client::from_vars(Some(String::new()), Some("pomb_secret".to_string()))
        .expect_err("blank url");
    assert_eq!(blank_url.to_string(), "POMB_URL is not set");

    let missing_token =
        Client::from_vars(Some("http://127.0.0.1:1".to_string()), None).expect_err("token");
    assert_eq!(missing_token.to_string(), "POMB_API_TOKEN is not set");
}

#[test]
fn client_debug_should_omit_the_token() {
    let client = Client::new("http://127.0.0.1:1", "pomb_secret".to_string());
    assert_eq!(
        format!("{client:?}"),
        r#"Client { base: "http://127.0.0.1:1", .. }"#
    );
}

#[test]
fn client_should_trim_trailing_slashes_from_the_url() {
    let client = Client::new("http://127.0.0.1:1///", "pomb_secret".to_string());
    assert_eq!(
        format!("{client:?}"),
        r#"Client { base: "http://127.0.0.1:1", .. }"#
    );
}
