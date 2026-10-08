//! The real cmux adapter against stub `cmux` executables: `notify`,
//! one-line sends and a named workspace, and the marks and unpin before a
//! close.

use dagq::{
    application::{WorkspaceBackend, WorkspaceTags},
    infrastructure::adapters::Cmux,
};
use std::fs;

/// The real adapter's `notify` is `cmux notify --title … --body …`, with
/// `--workspace` only when a target is given; a failing cmux is an error.
#[test]
fn the_cmux_adapter_notifies_with_title_body_and_an_optional_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let dump = dir.path().join("args.txt");
    let stub = dir.path().join("cmux-stub");
    crate::common::template::script_env(
        &stub,
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$STUB_PATH\"\n[ \"$1\" = notify ] || exit 2\n[ \"$3\" != fail ]\n",
        &[("STUB_PATH", dump.to_str().unwrap())],
    );

    let cmux = Cmux { executable: stub };
    let args = || fs::read_to_string(&dump).unwrap();
    cmux.notify("dagq repo: task 1 failed", "a task\nnext: x", Some("WS"))
        .unwrap();
    assert_eq!(
        args(),
        "notify\n--title\ndagq repo: task 1 failed\n--body\na task\nnext: x\n--workspace\nWS\n"
    );
    cmux.notify("title", "body", None).unwrap();
    assert_eq!(args(), "notify\n--title\ntitle\n--body\nbody\n");
    assert!(cmux.notify("fail", "body", None).is_err());
}

/// The real adapter opens a named workspace (the inbox's) in its directory
/// with its name, description, env and group. A run's session opens no
/// workspace (ADR-t1433-3).
#[test]
fn the_cmux_adapter_opens_a_named_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let dump = dir.path().join("args.txt");
    let stub = dir.path().join("cmux-stub");
    crate::common::template::script_env(
        &stub,
        "#!/bin/sh\nprintf '%s\\n' \"$@\" >> \"$STUB_PATH\"\ncase \"$1\" in workspace) echo 'OK workspace:7' ;; --json) echo '{\"caller\":{\"workspace_id\":\"01234567-89ab-4def-8123-000000000007\"}}' ;; esac\n",
        &[("STUB_PATH", dump.to_str().unwrap())],
    );

    let cmux = Cmux { executable: stub };
    assert_eq!(
        cmux.create_named(
            "[my-repo]inbox",
            dir.path(),
            "agent",
            &WorkspaceTags {
                env: vec![("DAGQ_ROLE".into(), "inbox".into())],
                description: Some("dagq role=inbox queue=abc".into()),
                group: Some("group:1".into()),
            },
        )
        .unwrap(),
        "01234567-89ab-4def-8123-000000000007"
    );
    let args = fs::read_to_string(&dump).unwrap();
    assert!(
        args.starts_with(&format!(
            "workspace\ncreate\n--name\n[my-repo]inbox\n--description\ndagq role=inbox queue=abc\n--env\nDAGQ_ROLE=inbox\n--group\ngroup:1\n--command\nagent\n--focus\nfalse\n--cwd\n{}\n",
            dir.path().display()
        )),
        "{args}"
    );
}

/// The real adapter's look calls are `workspace-action --action set-color
/// --color <c>`, `set-status <key> <value> --icon <i>` and
/// `workspace-action --action pin`, each with `--workspace <uuid>`; `close`
/// unpins before it closes, since cmux refuses to close a pinned
/// workspace, and an unpin that fails does not stop the close.
#[test]
fn the_cmux_adapter_marks_a_workspace_and_unpins_it_before_closing() {
    let dir = tempfile::tempdir().unwrap();
    let dump = dir.path().join("args.txt");
    let stub = dir.path().join("cmux-stub");
    // The unpin of `GONE` and the close of `STUCK` fail.
    crate::common::template::script_env(
        &stub,
        "#!/bin/sh\nprintf '%s ' \"$@\" >> \"$STUB_PATH\"\necho >> \"$STUB_PATH\"\ncase \"$*\" in *'unpin --workspace GONE') exit 1 ;; 'workspace close STUCK') exit 1 ;; 'workspace close'*) echo \"OK workspace:3\" ;; *) echo OK ;; esac\n",
        &[("STUB_PATH", dump.to_str().unwrap())],
    );

    let cmux = Cmux { executable: stub };
    let calls = || {
        let calls = fs::read_to_string(&dump).unwrap_or_default();
        let _ = fs::remove_file(&dump);
        calls
    };
    cmux.set_color("WS", "Amber").unwrap();
    cmux.set_status("WS", "dagq_role", "inbox", "tray").unwrap();
    cmux.pin("WS").unwrap();
    assert_eq!(
        calls(),
        "workspace-action --action set-color --color Amber --workspace WS \n\
set-status dagq_role inbox --icon tray --workspace WS \n\
workspace-action --action pin --workspace WS \n"
    );
    cmux.close("WS").unwrap();
    assert_eq!(
        calls(),
        "workspace-action --action unpin --workspace WS \nworkspace close WS \n"
    );
    cmux.close("GONE").unwrap();
    assert_eq!(
        calls(),
        "workspace-action --action unpin --workspace GONE \nworkspace close GONE \n"
    );
    assert!(cmux.close("STUCK").is_err());
    assert!(cmux.set_color("GONE", "Blue").is_ok());
}
