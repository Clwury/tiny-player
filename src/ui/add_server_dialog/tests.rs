use super::*;

#[gpui::test]
fn full_url_paste_and_protocol_intents_update_the_actual_editors(cx: &mut gpui::TestAppContext) {
    let dialog = cx.new(AddServerDialogState::new);
    dialog.update(cx, |dialog, cx| {
        dialog.address.update(cx, |input, cx| {
            input.set_value("http://[::1]:8096/custom", cx)
        });
    });
    cx.run_until_parked();
    dialog.read_with(cx, |dialog, cx| {
        assert_eq!(dialog.address.read(cx).value(), "[::1]");
        assert_eq!(dialog.port.read(cx).value(), "8096");
        assert_eq!(dialog.path.read(cx).value(), "/custom");
        assert_eq!(dialog.controller.view().protocol, Protocol::Http);
    });
    dialog.update(cx, |dialog, cx| dialog.select_protocol(Protocol::Https, cx));
    cx.run_until_parked();
    dialog.update(cx, |dialog, cx| {
        assert_eq!(dialog.port.read(cx).value(), "443");
        dialog
            .port
            .update(cx, |input, cx| input.set_value("9443", cx));
        dialog.select_protocol(Protocol::Http, cx);
        assert_eq!(dialog.port.read(cx).value(), "9443");
        dialog
            .address
            .update(cx, |input, cx| input.set_value("https://", cx));
    });
    cx.run_until_parked();
    dialog.read_with(cx, |dialog, cx| {
        assert_eq!(dialog.controller.view().protocol, Protocol::Http);
        assert_eq!(dialog.port.read(cx).value(), "9443");
        assert_eq!(dialog.path.read(cx).value(), "/custom");
    });
}

#[gpui::test]
fn edit_props_and_busy_retry_preserve_values_and_notification_lifecycle(
    cx: &mut gpui::TestAppContext,
) {
    let props = ServerFormProps {
        edit_server_id: Some("local".into()),
        protocol: Protocol::Http,
        address: "example.com".into(),
        port: "8096".into(),
        path: "/custom".into(),
        username: " user ".into(),
        password: " password ".into(),
    };
    let dialog = cx.new(|cx| AddServerDialogState::from_props(props, cx));
    dialog.update(cx, |dialog, cx| {
        assert_eq!(dialog.edit_server_id().as_deref(), Some("local"));
        assert_eq!(dialog.username.read(cx).value(), " user ");
        assert_eq!(dialog.password.read(cx).value(), " password ");
        dialog.push_error_notification("previous error", cx);
        let id = dialog.notifications.iter().next().unwrap().notification.id;
        dialog.set_submitting(true, cx);
        assert!(dialog.submit(cx).is_none());
        assert_eq!(
            dialog.notifications.iter().next().unwrap().notification.id,
            id
        );
        dialog.set_submitting(false, cx);
        let submission = dialog.submit(cx).unwrap();
        assert_eq!(submission.endpoint.protocol, Protocol::Http);
        assert_eq!(submission.endpoint.path, "/custom");
        assert_eq!(submission.username, "user");
        assert_eq!(submission.password, "password");
        assert!(dialog.notifications.is_empty());
        dialog
            .username
            .update(cx, |input, cx| input.set_value("", cx));
        assert!(dialog.submit(cx).is_none());
        assert_eq!(
            dialog
                .notifications
                .iter()
                .next()
                .unwrap()
                .notification
                .message,
            "请输入用户名"
        );
    });
}
