use std::os::unix::fs::{PermissionsExt, symlink};

use dagq_broker_protocol::{Operation, TokenClaims, decode};

use super::*;
use crate::config::Limits;

/// A mounted root with the workspaces of run-1 and run-2 and a directory
/// outside them.
struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    backend: FsBackend,
    claims: TokenClaims,
    limits: Limits,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let root = base.join("runs");
        for run in ["run-1", "run-2"] {
            std::fs::create_dir_all(root.join(run).join("worktree/src")).unwrap();
        }
        std::fs::create_dir_all(base.join("outside")).unwrap();
        std::fs::write(base.join("outside/secret"), "secret\n").unwrap();
        std::fs::write(root.join("run-2/worktree/theirs"), "theirs\n").unwrap();
        let workspace = root.join("run-1/worktree");
        std::fs::write(workspace.join(".git"), "gitdir: /somewhere\n").unwrap();
        let claims: TokenClaims = serde_json::from_value(serde_json::json!({
            "v": 1, "jti": "j", "actor_id": "worker:run-1", "role": "worker",
            "task_id": 831, "run_id": "run-1", "workspace": workspace,
            "branch": "dagq/run-1", "committer": {"name": "n", "email": "e"},
            "capabilities": ["fs.read", "fs.write"], "iat": 0, "exp": 1
        }))
        .unwrap();
        Self {
            backend: FsBackend::new(vec![root.clone()]),
            _dir: dir,
            root,
            claims,
            limits: Limits::default(),
        }
    }

    fn workspace(&self) -> PathBuf {
        PathBuf::from(&self.claims.workspace)
    }

    fn base(&self) -> PathBuf {
        self.root.parent().unwrap().to_path_buf()
    }

    /// Run `body` as `operation`, confining its path as the server does.
    fn call(&self, operation: Operation, body: serde_json::Value) -> Result<Vec<u8>, Failure> {
        let request = BackendRequest::decode(operation, body.to_string().as_bytes()).unwrap();
        let confined = request
            .paths()
            .into_iter()
            .map(|path| self.claims.confine(Path::new(path)))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| Failure::new(error.code(), error.to_string()))?;
        let call = Call {
            operation,
            claims: &self.claims,
            confined,
            limits: &self.limits,
        };
        self.backend.call(&call, request).map(|done| {
            assert_eq!(done.exit_code, None);
            done.body
        })
    }

    fn read(&self, body: serde_json::Value) -> Result<fs::ReadResponse, Failure> {
        self.call(Operation::FsRead, body)
            .map(|body| decode(&body).unwrap())
    }

    fn write(&self, path: &str, content: &str) -> Result<fs::WriteResponse, Failure> {
        self.call(
            Operation::FsWrite,
            serde_json::json!({ "path": path, "content": content }),
        )
        .map(|body| decode(&body).unwrap())
    }

    fn edit(&self, body: serde_json::Value) -> Result<fs::EditResponse, Failure> {
        self.call(Operation::FsEdit, body)
            .map(|body| decode(&body).unwrap())
    }

    fn list(&self, path: &str) -> Result<fs::ListResponse, Failure> {
        self.call(Operation::FsList, serde_json::json!({ "path": path }))
            .map(|body| decode(&body).unwrap())
    }

    /// The names in the workspace's directory `dir`, to see no temporary
    /// file is left.
    fn names(&self, dir: &str) -> Vec<String> {
        let mut names: Vec<_> = std::fs::read_dir(self.workspace().join(dir))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }
}

fn code<T: std::fmt::Debug>(result: Result<T, Failure>) -> ErrorCode {
    result.unwrap_err().code
}

#[test]
fn writes_reads_edits_and_lists_inside_the_workspace() {
    let fx = Fixture::new();
    assert_eq!(
        fx.write("src/a.txt", "one\ntwo\nthree\n").unwrap().bytes,
        14
    );
    let absolute = fx.workspace().join("src/a.txt");
    let read = fx.read(serde_json::json!({ "path": absolute })).unwrap();
    assert_eq!(read.content, "one\ntwo\nthree\n");
    assert_eq!((read.lines, read.truncated), (3, false));

    let read = fx
        .read(serde_json::json!({ "path": "./src/a.txt", "offset": 1, "limit": 1 }))
        .unwrap();
    assert_eq!(read.content, "two\n");
    assert_eq!((read.lines, read.truncated), (1, true));
    let read = fx
        .read(serde_json::json!({ "path": "src/a.txt", "offset": 9 }))
        .unwrap();
    assert_eq!(
        (read.content.as_str(), read.lines, read.truncated),
        ("", 0, false)
    );

    let edit = fx
        .edit(serde_json::json!({ "path": "src/a.txt", "old_string": "two", "new_string": "2" }))
        .unwrap();
    assert_eq!(edit.replacements, 1);
    assert_eq!(
        std::fs::read_to_string(&absolute).unwrap(),
        "one\n2\nthree\n"
    );

    std::fs::write(fx.workspace().join("big"), vec![b'x'; 10]).unwrap();
    let list = fx.list(".").unwrap();
    let table: Vec<_> = list
        .entries
        .iter()
        .map(|entry| (entry.name.as_str(), entry.kind, entry.size))
        .collect();
    assert_eq!(
        table,
        [
            (".git", fs::EntryKind::File, 19),
            ("big", fs::EntryKind::File, 10),
            ("src", fs::EntryKind::Dir, 0),
        ]
    );
    assert_eq!(fx.names("src"), ["a.txt"]);
}

#[test]
fn write_replaces_atomically_keeps_the_mode_and_creates_dirs_when_asked() {
    let fx = Fixture::new();
    let script = fx.workspace().join("run.sh");
    std::fs::write(&script, "old").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o750)).unwrap();
    fx.write("run.sh", "new").unwrap();
    assert_eq!(std::fs::read_to_string(&script).unwrap(), "new");
    let mode = std::fs::metadata(&script).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o750);
    assert_eq!(fx.names("."), [".git", "run.sh", "src"]);

    assert_eq!(code(fx.write("a/b/c.txt", "x")), ErrorCode::BackendError);
    let written = fx.call(
        Operation::FsWrite,
        serde_json::json!({ "path": "a/b/c.txt", "content": "x", "create_dirs": true }),
    );
    assert!(written.is_ok(), "{written:?}");
    assert_eq!(
        std::fs::read_to_string(fx.workspace().join("a/b/c.txt")).unwrap(),
        "x"
    );

    // Onto a directory, or the workspace itself.
    assert_eq!(code(fx.write("src", "x")), ErrorCode::BackendError);
    assert_eq!(code(fx.write(".", "x")), ErrorCode::BackendError);
    assert_eq!(fx.names("src"), Vec::<String>::new());
}

#[test]
fn edit_needs_one_match_unless_replace_all() {
    let fx = Fixture::new();
    fx.write("e.txt", "a b a b a\n").unwrap();
    let failure = fx
        .edit(serde_json::json!({ "path": "e.txt", "old_string": "a", "new_string": "c" }))
        .unwrap_err();
    assert_eq!(failure.code, ErrorCode::InvalidRequest);
    assert!(
        failure.message.contains("matches 3 times"),
        "{}",
        failure.message
    );
    assert!(!failure.message.contains("a b a"), "no content in messages");
    assert_eq!(
        std::fs::read_to_string(fx.workspace().join("e.txt")).unwrap(),
        "a b a b a\n"
    );

    for (old, new, message) in [
        ("zzz", "y", "was not found"),
        ("", "y", "is empty"),
        ("b", "b", "are the same"),
    ] {
        let failure = fx
            .edit(serde_json::json!({ "path": "e.txt", "old_string": old, "new_string": new }))
            .unwrap_err();
        assert_eq!(failure.code, ErrorCode::InvalidRequest, "{old}");
        assert!(failure.message.contains(message), "{}", failure.message);
    }

    let edit = fx
        .edit(serde_json::json!({
            "path": "e.txt", "old_string": "a", "new_string": "c", "replace_all": true
        }))
        .unwrap();
    assert_eq!(edit.replacements, 3);
    assert_eq!(
        std::fs::read_to_string(fx.workspace().join("e.txt")).unwrap(),
        "c b c b c\n"
    );
    assert_eq!(
        code(fx.edit(serde_json::json!({ "path": "nope", "old_string": "a", "new_string": "b" }))),
        ErrorCode::BackendError
    );
}

#[test]
fn escapes_by_letters_are_workspace_violations() {
    let fx = Fixture::new();
    let theirs = fx.root.join("run-2/worktree/theirs");
    let secret = fx.base().join("outside/secret");
    for path in [
        "../run-2/worktree/theirs".to_owned(),
        "src/../../run-2/worktree/theirs".to_owned(),
        theirs.to_string_lossy().into_owned(),
        secret.to_string_lossy().into_owned(),
        "/etc/passwd".to_owned(),
    ] {
        assert_eq!(
            code(fx.read(serde_json::json!({ "path": path }))),
            ErrorCode::WorkspaceViolation,
            "{path}"
        );
        assert_eq!(
            code(fx.write(&path, "x")),
            ErrorCode::WorkspaceViolation,
            "{path}"
        );
    }
    // `~` is not expanded: it is a name in the workspace.
    fx.write("~", "tilde").unwrap();
    assert!(fx.workspace().join("~").is_file());
    assert_eq!(std::fs::read_to_string(&theirs).unwrap(), "theirs\n");
}

#[test]
fn symlinks_are_not_followed_in_or_out() {
    let fx = Fixture::new();
    let ws = fx.workspace();
    symlink(fx.base().join("outside/secret"), ws.join("leak")).unwrap();
    symlink(fx.base().join("outside"), ws.join("out")).unwrap();
    symlink(fx.root.join("run-2/worktree"), ws.join("src/other-run")).unwrap();
    symlink("src", ws.join("inner")).unwrap();
    for path in [
        "leak",
        "out/secret",
        "out/new",
        "src/other-run/theirs",
        "inner/x",
    ] {
        assert_eq!(
            code(fx.read(serde_json::json!({ "path": path }))),
            ErrorCode::WorkspaceViolation,
            "read {path}"
        );
        assert_eq!(
            code(fx.write(path, "x")),
            ErrorCode::WorkspaceViolation,
            "write {path}"
        );
        assert_eq!(
            code(
                fx.edit(serde_json::json!({ "path": path, "old_string": "s", "new_string": "t" }))
            ),
            ErrorCode::WorkspaceViolation,
            "edit {path}"
        );
    }
    for path in ["out", "src/other-run", "inner"] {
        assert_eq!(
            code(fx.list(path)),
            ErrorCode::WorkspaceViolation,
            "list {path}"
        );
    }
    let create = fx.call(
        Operation::FsWrite,
        serde_json::json!({ "path": "out/deeper/x", "content": "x", "create_dirs": true }),
    );
    assert_eq!(code(create), ErrorCode::WorkspaceViolation);
    assert_eq!(
        std::fs::read_to_string(fx.base().join("outside/secret")).unwrap(),
        "secret\n"
    );
    assert!(!fx.base().join("outside/new").exists());
    assert!(!fx.base().join("outside/deeper").exists());
    // Listed, but as symlinks.
    let kinds: Vec<_> = fx
        .list(".")
        .unwrap()
        .entries
        .into_iter()
        .filter(|entry| entry.kind == fs::EntryKind::Symlink)
        .map(|entry| entry.name)
        .collect();
    assert_eq!(kinds, ["inner", "leak", "out"]);
}

#[test]
fn a_workspace_reached_through_a_symlink_is_refused() {
    let fx = Fixture::new();
    // run-3's worktree is a symlink to run-2's.
    std::fs::create_dir_all(fx.root.join("run-3")).unwrap();
    symlink(
        fx.root.join("run-2/worktree"),
        fx.root.join("run-3/worktree"),
    )
    .unwrap();
    let mut fx = fx;
    fx.claims.workspace = fx
        .root
        .join("run-3/worktree")
        .to_string_lossy()
        .into_owned();
    assert_eq!(
        code(fx.read(serde_json::json!({ "path": "theirs" }))),
        ErrorCode::WorkspaceViolation
    );
    fx.claims.workspace = fx.base().join("outside").to_string_lossy().into_owned();
    assert_eq!(
        code(fx.read(serde_json::json!({ "path": "secret" }))),
        ErrorCode::WorkspaceViolation
    );
}

#[test]
fn the_worktrees_git_is_off_limits_in_any_case() {
    let fx = Fixture::new();
    for path in [".git", ".GIT", ".git/config", "./.Git/HEAD"] {
        assert_eq!(
            code(fx.read(serde_json::json!({ "path": path }))),
            ErrorCode::WorkspaceViolation,
            "{path}"
        );
        assert_eq!(
            code(fx.write(path, "x")),
            ErrorCode::WorkspaceViolation,
            "{path}"
        );
    }
    assert_eq!(code(fx.list(".git")), ErrorCode::WorkspaceViolation);
    assert_eq!(
        std::fs::read_to_string(fx.workspace().join(".git")).unwrap(),
        "gitdir: /somewhere\n"
    );
    // A `.git` deeper down is a name like any other.
    fx.write("src/.gitignore", "target\n").unwrap();
}

#[test]
fn reads_and_writes_over_the_limit_are_refused() {
    let mut fx = Fixture::new();
    fx.limits.fs_limit_bytes = 8;
    assert_eq!(code(fx.write("w", "123456789")), ErrorCode::OutputLimit);
    assert!(!fx.workspace().join("w").exists());
    fx.write("w", "12345678").unwrap();

    std::fs::write(fx.workspace().join("long"), "1234\n5678\n9\n").unwrap();
    assert_eq!(
        code(fx.read(serde_json::json!({ "path": "long" }))),
        ErrorCode::OutputLimit
    );
    let read = fx
        .read(serde_json::json!({ "path": "long", "limit": 1 }))
        .unwrap();
    assert_eq!((read.content.as_str(), read.truncated), ("1234\n", true));
    let read = fx
        .read(serde_json::json!({ "path": "long", "offset": 1 }))
        .unwrap();
    assert_eq!(read.content, "5678\n9\n");

    std::fs::write(fx.workspace().join("oneline"), vec![b'x'; 100]).unwrap();
    assert_eq!(
        code(fx.read(serde_json::json!({ "path": "oneline" }))),
        ErrorCode::OutputLimit
    );
    assert_eq!(
        code(
            fx.edit(serde_json::json!({ "path": "oneline", "old_string": "x", "new_string": "y" }))
        ),
        ErrorCode::OutputLimit
    );
    assert_eq!(
        code(fx.edit(serde_json::json!({ "path": "w", "old_string": "1", "new_string": "ab" }))),
        ErrorCode::OutputLimit
    );
    assert_eq!(
        std::fs::read_to_string(fx.workspace().join("w")).unwrap(),
        "12345678"
    );

    // The listing's answer is bounded too: fine under a limit it fits in,
    // refused once one more entry pushes it over.
    fx.limits.fs_limit_bytes = u64::MAX;
    let fits = fx
        .call(Operation::FsList, serde_json::json!({"path": "."}))
        .unwrap()
        .len() as u64;
    fx.limits.fs_limit_bytes = fits;
    assert!(fx.list(".").is_ok());
    std::fs::write(fx.workspace().join("one-more"), "").unwrap();
    assert_eq!(code(fx.list(".")), ErrorCode::OutputLimit);
}

#[test]
fn what_is_not_a_readable_text_file_is_a_backend_error() {
    let fx = Fixture::new();
    std::fs::write(fx.workspace().join("bin"), [0xff, 0xfe, b'\n']).unwrap();
    let cases = [
        ("missing", "no such file"),
        ("src", "is a directory"),
        ("bin", "not UTF-8"),
        ("bin/x", "not a directory"),
        (".", "the workspace"),
    ];
    for (path, message) in cases {
        let failure = fx.read(serde_json::json!({ "path": path })).unwrap_err();
        assert_eq!(failure.code, ErrorCode::BackendError, "{path}");
        assert!(
            failure.message.contains(message),
            "{path}: {}",
            failure.message
        );
    }
    assert_eq!(code(fx.list("bin")), ErrorCode::BackendError);
    assert_eq!(code(fx.list("missing")), ErrorCode::BackendError);
    assert_eq!(
        code(fx.edit(serde_json::json!({ "path": "bin", "old_string": "a", "new_string": "b" }))),
        ErrorCode::BackendError
    );

    // A FIFO is neither read (without blocking) nor replaced.
    let fifo = fx.workspace().join("fifo");
    let path = CString::new(fifo.as_os_str().as_bytes()).unwrap();
    // SAFETY: a NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    let failure = fx.read(serde_json::json!({ "path": "fifo" })).unwrap_err();
    assert!(
        failure.message.contains("not a regular file"),
        "{}",
        failure.message
    );
    assert_eq!(code(fx.write("fifo", "x")), ErrorCode::BackendError);
    let kind = fx
        .list(".")
        .unwrap()
        .entries
        .into_iter()
        .find(|entry| entry.name == "fifo")
        .unwrap()
        .kind;
    assert_eq!(kind, fs::EntryKind::Other);
}

#[test]
fn refuses_what_is_not_one_fs_path() {
    let fx = Fixture::new();
    let call = Call {
        operation: Operation::GitStatus,
        claims: &fx.claims,
        confined: Vec::new(),
        limits: &fx.limits,
    };
    let request = BackendRequest::GitStatus(dagq_broker_protocol::git::StatusRequest {});
    assert_eq!(
        fx.backend.call(&call, request.clone()).unwrap_err().code,
        ErrorCode::InvalidRequest
    );
    let call = Call {
        confined: vec![fx.workspace()],
        ..call
    };
    assert_eq!(
        fx.backend.call(&call, request).unwrap_err().code,
        ErrorCode::InvalidRequest
    );
    let unmounted = FsBackend::new(vec![fx.base().join("elsewhere")]);
    let call = Call {
        operation: Operation::FsList,
        confined: vec![fx.workspace()],
        ..call
    };
    let request = BackendRequest::FsList(fs::ListRequest {
        path: ".".to_owned(),
    });
    assert_eq!(
        unmounted.call(&call, request.clone()).unwrap_err().code,
        ErrorCode::WorkspaceViolation
    );
    let missing_root = FsBackend::new(vec![fx.root.clone()]);
    std::fs::rename(&fx.root, fx.base().join("moved")).unwrap();
    assert_eq!(
        missing_root.call(&call, request).unwrap_err().code,
        ErrorCode::BackendError
    );
}
