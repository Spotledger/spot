//! Real NetAPI user/group conformance (WP-ADM-05.T3, `users` in `matrix.toml`).
//!
//! Creates a throw-away local account through the module (which generates a password from
//! the OS CSPRNG and delivers it on the sealed channel), then disables it, resets its
//! password and sets its group membership. The account carries this run's prefix and is
//! deleted on every path; the suite never touches a built-in account.

use std::sync::Arc;

use spot_agent_windows::catalogue::{
    GROUP_LIST, USER_CREATE, USER_DISABLE, USER_LIST, USER_RESET_PASSWORD, USER_SET_GROUPS,
};
use spot_agent_windows::{OsPasswordSource, Users, WindowsError};
use spot_agent_windows_real::{
    Cleanup, RecordingChannel, args, assert_ours, config, list, name, net, prefix, text,
};

#[test]
fn throwaway_user_lifecycle() {
    let account = name("user");
    // The local-account name limit is 20 characters; the prefix keeps this inside it.
    assert!(account.len() <= 20, "account name too long: {account:?}");

    let api = spot_agent_windows_real::api();
    match api.local_user(&account) {
        Err(WindowsError::UnknownUser { .. }) => {}
        other => panic!("the throw-away account {account:?} already exists: {other:?}"),
    }

    let channel = Arc::new(RecordingChannel::default());
    let users = Users::new(
        api.clone(),
        config(),
        Arc::new(OsPasswordSource),
        channel.clone(),
    );

    let mut cleanup = Cleanup::new();
    {
        let doomed = account.clone();
        cleanup.push(move || {
            assert_ours(&doomed);
            let _ = net(&["user", &doomed, "/delete"]);
        });
    }

    // Create through the module: generate a password, install it, deliver it sealed.
    users
        .execute(&args(
            USER_CREATE,
            vec![("user_name", text(&account)), ("groups", list(&["Users"]))],
        ))
        .expect("creating the throw-away account must succeed");

    let created = api
        .local_user(&account)
        .expect("the new account must be readable");
    assert!(created.enabled);
    assert!(
        created
            .groups
            .iter()
            .any(|group| group.eq_ignore_ascii_case("Users")),
        "the account must be in Users: {:?}",
        created.groups
    );
    // The generated password was delivered, and only its length was recorded.
    assert_eq!(channel.accounts(), vec![account.clone()]);

    // Disable: the account is read, then disabled, then read back.
    users
        .execute(&args(USER_DISABLE, vec![("user_name", text(&account))]))
        .expect("disabling the account must succeed");
    assert!(!api.local_user(&account).expect("read back").enabled);

    // Reset the password: a fresh secret is generated and delivered; never returned.
    users
        .execute(&args(USER_RESET_PASSWORD, vec![("user_name", text(&account))]))
        .expect("resetting the password must succeed");
    assert_eq!(channel.accounts(), vec![account.clone(), account.clone()]);

    // Set group membership to the empty set, then back to Users.
    users
        .execute(&args(
            USER_SET_GROUPS,
            vec![("user_name", text(&account)), ("groups", list(&[]))],
        ))
        .expect("clearing group membership must succeed");
    assert!(
        api.local_user(&account)
            .expect("read back")
            .groups
            .is_empty()
    );
    users
        .execute(&args(
            USER_SET_GROUPS,
            vec![("user_name", text(&account)), ("groups", list(&["Users"]))],
        ))
        .expect("restoring group membership must succeed");

    // Only our own prefixed account may exist.
    let ours: Vec<String> = api
        .list_local_users()
        .expect("enumerating users must succeed")
        .into_iter()
        .filter(|user| user.name.starts_with(&prefix()))
        .map(|user| user.name)
        .collect();
    assert_eq!(ours, vec![account.clone()]);

    // A built-in account is refused without the allow-list flag, touching nothing.
    let refused = users
        .plan(&args(USER_DISABLE, vec![("user_name", text("Administrator"))]))
        .expect_err("disabling Administrator without the flag must be refused");
    assert_eq!(refused.code(), spot_agent_windows::error::USER_PROTECTED);

    // Delete now; the cleanup guard repeats it harmlessly on a panic path.
    let deleted = net(&["user", &account, "/delete"]);
    assert!(
        deleted.status.success(),
        "net user /delete failed: {}",
        String::from_utf8_lossy(&deleted.stderr)
    );
}

#[test]
fn local_groups_and_users_are_readable() {
    let api = spot_agent_windows_real::api();
    let users = Users::new(
        api.clone(),
        config(),
        Arc::new(OsPasswordSource),
        Arc::new(RecordingChannel::default()),
    );

    let accounts = users.list_users().expect("listing users must succeed");
    assert!(!accounts.is_empty(), "a Windows machine has local accounts");
    assert!(accounts.windows(2).all(|pair| pair[0].name <= pair[1].name));

    let groups = users.list_groups().expect("listing groups must succeed");
    assert!(
        groups.iter().any(|group| group.name.eq_ignore_ascii_case("Users")),
        "the built-in Users group must be present"
    );

    // The read-only catalogue operations succeed against the real machine.
    users
        .verify(&args(USER_LIST, vec![]))
        .expect("verifying a user list must succeed");
    users
        .verify(&args(GROUP_LIST, vec![]))
        .expect("verifying a group list must succeed");
}
