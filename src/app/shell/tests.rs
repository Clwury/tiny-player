use super::*;
use std::{cell::RefCell, rc::Rc};

fn identity(user: &str) -> WorkspaceIdentity {
    WorkspaceIdentity {
        local_server_id: "local".into(),
        remote_server_id: Some("remote".into()),
        user_id: Some(user.into()),
    }
}

struct Subscription {
    name: &'static str,
    dropped: Rc<RefCell<Vec<&'static str>>>,
}
impl Drop for Subscription {
    fn drop(&mut self) {
        self.dropped.borrow_mut().push(self.name);
    }
}
fn subscription(name: &'static str, dropped: &Rc<RefCell<Vec<&'static str>>>) -> Subscription {
    Subscription {
        name,
        dropped: dropped.clone(),
    }
}
type Shell = ShellController<Rc<str>, Rc<str>, Subscription>;

#[test]
fn playback_replacement_and_back_retain_the_exact_home_mount() {
    let mut shell = Shell::default();
    assert_eq!(shell.route(), AppRoute::Servers);
    let account = identity("user");
    let home: Rc<str> = Rc::from("Home");
    let home_token = shell.mount_home(home.clone(), account.clone());
    assert_eq!(shell.route(), AppRoute::Home(&account));
    let first = shell.mount_playback(Rc::from("episode one")).unwrap();
    assert_eq!(shell.route(), AppRoute::Playback(&account));
    assert!(shell.accepts_home(&home_token));
    let next = shell.mount_playback(Rc::from("episode two")).unwrap();
    assert!(!shell.accepts_playback(&first));
    assert!(shell.accepts_playback(&next));
    assert!(Rc::ptr_eq(shell.home().unwrap(), &home));
    assert!(shell.return_home());
    assert_eq!(shell.route(), AppRoute::Home(&account));
    assert!(shell.accepts_home(&home_token));
    assert!(!shell.accepts_playback(&next));
    assert!(Rc::ptr_eq(shell.home().unwrap(), &home));
    assert!(!shell.return_home());
}

#[test]
fn same_workspace_remount_and_account_switch_reject_prior_events() {
    let mut shell = Shell::default();
    let old = shell.mount_home(Rc::from("old"), identity("user"));
    let old_playback = shell.mount_playback(Rc::from("video")).unwrap();
    let remount = shell.mount_home(Rc::from("new"), identity("user"));
    assert!(!shell.accepts_home(&old) && !shell.accepts_playback(&old_playback));
    assert!(shell.accepts_home(&remount));
    let switched = shell.mount_home(Rc::from("other"), identity("other"));
    assert!(!shell.accepts_home(&remount));
    assert!(shell.accepts_home(&switched));
    assert!(!shell.accepts_home(&old_playback));
    let mut other_shell = Shell::default();
    let other = other_shell.mount_home(Rc::from("other instance"), identity("other"));
    assert!(!shell.accepts_home(&other));
}

#[test]
fn return_to_servers_invalidates_both_scopes_and_prevents_orphan_playback() {
    let mut shell = Shell::default();
    assert!(shell.mount_playback(Rc::from("orphan")).is_none());
    let home = shell.mount_home(Rc::from("home"), identity("user"));
    let playback = shell.mount_playback(Rc::from("video")).unwrap();
    shell.show_servers();
    shell.show_servers();
    assert_eq!(shell.route(), AppRoute::Servers);
    assert!(shell.home().is_none());
    assert!(!shell.accepts_home(&home) && !shell.accepts_playback(&playback));
    assert!(shell.mount_playback(Rc::from("orphan")).is_none());
}

#[test]
fn replacing_and_unmounting_cancel_only_the_retired_subscriptions() {
    let dropped = Rc::new(RefCell::new(vec![]));
    let mut shell = Shell::default();
    let home = shell.mount_home(Rc::from("home"), identity("user"));
    shell.subscribe_home(&home, subscription("home", &dropped));
    let first = shell.mount_playback(Rc::from("one")).unwrap();
    shell.subscribe_playback(&first, subscription("one", &dropped));
    let next = shell.mount_playback(Rc::from("two")).unwrap();
    assert_eq!(*dropped.borrow(), ["one"]);
    shell.subscribe_playback(&next, subscription("two", &dropped));
    shell.subscribe_playback(&first, subscription("stale", &dropped));
    assert_eq!(*dropped.borrow(), ["one", "stale"]);
    shell.return_home();
    assert_eq!(*dropped.borrow(), ["one", "stale", "two"]);
    shell.show_servers();
    assert_eq!(*dropped.borrow(), ["one", "stale", "two", "home"]);
    shell.subscribe_home(&home, subscription("late", &dropped));
    assert_eq!(dropped.borrow().last(), Some(&"late"));
}

#[test]
fn shell_release_cancels_both_subscriptions_and_releases_mounted_entities() {
    let dropped = Rc::new(RefCell::new(vec![]));
    let mut shell = Shell::default();
    let home: Rc<str> = Rc::from("home");
    let playback: Rc<str> = Rc::from("video");
    let home_token = shell.mount_home(home.clone(), identity("user"));
    let playback_token = shell.mount_playback(playback.clone()).unwrap();
    shell.subscribe_home(&home_token, subscription("home", &dropped));
    shell.subscribe_playback(&playback_token, subscription("playback", &dropped));
    assert_eq!(Rc::strong_count(&home), 2);
    assert_eq!(Rc::strong_count(&playback), 2);
    drop(shell);
    assert_eq!(Rc::strong_count(&home), 1);
    assert_eq!(Rc::strong_count(&playback), 1);
    assert_eq!(*dropped.borrow(), ["home", "playback"]);
}
