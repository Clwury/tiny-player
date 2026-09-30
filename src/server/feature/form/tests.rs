use super::*;

#[test]
fn parses_full_url_for_auto_formatting() {
    let endpoint = parsed_full_url_endpoint(" http://example.com:8096/custom ").unwrap();

    assert_eq!(endpoint.protocol, Protocol::Http);
    assert_eq!(endpoint.address_input_value(), "example.com");
    assert_eq!(endpoint.port, 8096);
    assert_eq!(endpoint.path, "/custom");
}

#[test]
fn parses_full_url_without_port_using_scheme_default() {
    let endpoint = parsed_full_url_endpoint("https://example.com").unwrap();

    assert_eq!(endpoint.protocol, Protocol::Https);
    assert_eq!(endpoint.port, 443);
    assert_eq!(endpoint.path, "");
}

#[test]
fn ignores_non_full_url_for_auto_formatting() {
    assert!(parsed_full_url_endpoint("example.com:8096").is_none());
    assert!(parsed_full_url_endpoint("ftp://example.com").is_none());
}

#[test]
fn validates_address_before_credentials() {
    let error = validate_server_submission(Protocol::Https, "", "443", "", "", "")
        .expect_err("blank address should fail validation first");

    assert_eq!(error.as_str(), "请输入服务器地址");
}

#[test]
fn validates_username_before_password() {
    let error = validate_server_submission(Protocol::Https, "example.com", "443", "", "", "")
        .expect_err("blank username should fail validation before password");

    assert_eq!(error.as_str(), "请输入用户名");

    let error = validate_server_submission(Protocol::Https, "example.com", "443", "", "user", "")
        .expect_err("blank password should fail after username is valid");
    assert_eq!(error.as_str(), "请输入密码");
}

#[test]
fn valid_submission_trims_credentials() {
    let submission = validate_server_submission(
        Protocol::Https,
        "example.com",
        "443",
        "",
        " user ",
        " password ",
    )
    .expect("valid fields should produce a submission");

    assert_eq!(submission.endpoint.address, "example.com");
    assert_eq!(submission.username, "user");
    assert_eq!(submission.password, "password");
}

#[test]
fn changing_protocol_replaces_only_empty_or_previous_default_ports() {
    for port in ["", "443", "9443", " 443 "] {
        let mut controller = ServerFormController::new(&ServerFormProps::default());
        let update = controller.dispatch(ServerFormIntent::SelectProtocol {
            protocol: Protocol::Http,
            port,
        });
        if port.is_empty() || port == "443" {
            assert!(matches!(update, ServerFormUpdate::PortChanged("8096")));
        } else {
            assert!(matches!(update, ServerFormUpdate::Changed));
        }
        assert_eq!(controller.view().protocol, Protocol::Http);
        assert!(matches!(
            controller.dispatch(ServerFormIntent::SelectProtocol {
                protocol: Protocol::Http,
                port: ""
            }),
            ServerFormUpdate::Ignored
        ));
        assert!(matches!(
            controller.dispatch(ServerFormIntent::SelectProtocol {
                protocol: Protocol::Https,
                port: "8096"
            }),
            ServerFormUpdate::PortChanged("443")
        ));
    }
}

#[test]
fn address_intent_updates_protocol_only_for_a_valid_complete_url() {
    let mut controller = ServerFormController::new(&ServerFormProps::default());
    let ServerFormUpdate::EndpointChanged(endpoint) = controller.dispatch(
        ServerFormIntent::AddressChanged(" HTTP://[::1]:8096/custom "),
    ) else {
        panic!("valid full URL")
    };
    assert_eq!(controller.view().protocol, Protocol::Http);
    assert_eq!(endpoint.address_input_value(), "[::1]");
    assert_eq!(endpoint.port, 8096);
    assert_eq!(endpoint.path, "/custom");
    for address in [
        "[::1]",
        "https://",
        "https://host:invalid",
        "ftp://example.com",
    ] {
        assert!(matches!(
            controller.dispatch(ServerFormIntent::AddressChanged(address)),
            ServerFormUpdate::Ignored
        ));
        assert_eq!(controller.view().protocol, Protocol::Http);
    }
}

#[test]
fn busy_submission_is_ignored_and_retry_uses_current_inputs() {
    let props = ServerFormProps {
        edit_server_id: Some("local".into()),
        ..Default::default()
    };
    let mut controller = ServerFormController::new(&props);
    assert_eq!(controller.view().edit_server_id, Some("local"));
    assert_eq!(
        (controller.view().title, controller.view().submit_label),
        ("编辑服务器", "保存")
    );
    let input = || ServerFormInput {
        address: "example.com",
        port: "443",
        path: "",
        username: " user ",
        password: " password ",
    };
    controller.dispatch(ServerFormIntent::SetSubmitting(true));
    assert!(controller.view().is_submitting);
    assert!(matches!(
        controller.dispatch(ServerFormIntent::Submit(input())),
        ServerFormUpdate::Ignored
    ));
    controller.dispatch(ServerFormIntent::SetSubmitting(false));
    let ServerFormUpdate::Submission(Ok(submission)) =
        controller.dispatch(ServerFormIntent::Submit(input()))
    else {
        panic!("valid retry")
    };
    assert_eq!(submission.username, "user");
    assert_eq!(submission.password, "password");
    // The runner sets busy after accepting the submission, as before.
    assert!(!controller.view().is_submitting);
}
