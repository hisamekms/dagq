//! Shared executable publication and per-fixture isolation across processes.
use crate::common::{Bounded, template};
use std::{fs, os::unix::fs::MetadataExt, path::Path, process::Command};

#[test]
fn concurrent_processes_share_a_warmed_inode_but_keep_paths_and_values_private() {
    const CHILD: &str = "DAGQ_TEST_TEMPLATE_CHILD";
    if let Some(dir) = std::env::var_os(CHILD) {
        let dir = Path::new(&dir);
        // A new content key per parent run exercises the cold publication race.
        let body = fs::read_to_string(dir.parent().unwrap().join("body")).unwrap();
        template::script_env(&dir.join("stub"), body, &[("VALUE", dir.to_str().unwrap())]);
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let body = format!(
        "#!/bin/sh\n# {}\nprintf '%s\\n' \"$0\" \"$VALUE\" \"$@\"\nprintf touched > \"$0.called\"\n",
        root.path().display()
    );
    fs::write(root.path().join("body"), &body).unwrap();
    let dirs: Vec<_> = (0..6)
        .map(|i| root.path().join(format!("{i} ' $()")))
        .collect();
    std::thread::scope(|scope| {
        for dir in &dirs {
            fs::create_dir(dir).unwrap();
            scope.spawn(move || {
                let output = Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", "stub_templates::concurrent_processes_share_a_warmed_inode_but_keep_paths_and_values_private"])
                    .env(CHILD, dir)
                    .bounded_output().unwrap();
                assert!(output.status.success(), "{output:?}");
            });
        }
    });
    let first = fs::metadata(dirs[0].join("stub")).unwrap();
    for dir in &dirs {
        let stub = dir.join("stub");
        let metadata = fs::metadata(&stub).unwrap();
        assert_eq!((metadata.dev(), metadata.ino()), (first.dev(), first.ino()));
        assert_eq!(
            stub.canonicalize().unwrap(),
            dir.canonicalize().unwrap().join("stub")
        );
        assert!(
            !dir.join("stub.called").exists(),
            "warmup must not run the body"
        );
        let output = Command::new(&stub)
            .arg("an argument")
            .bounded_output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            format!("{}\n{}\nan argument\n", stub.display(), dir.display())
        );
        assert_eq!(
            fs::read_to_string(dir.join("stub.called")).unwrap(),
            "touched"
        );
    }
    template::script(&dirs[0].join("stub"), "#!/bin/sh\n");
    assert!(
        Command::new(dirs[0].join("stub"))
            .bounded_status()
            .unwrap()
            .success()
    );
    assert_ne!(
        fs::metadata(dirs[0].join("stub")).unwrap().ino(),
        first.ino()
    );
    assert_eq!(
        fs::metadata(dirs[1].join("stub")).unwrap().ino(),
        first.ino()
    );
}
