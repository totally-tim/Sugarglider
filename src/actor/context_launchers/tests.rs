use std::os::unix::fs::symlink;
use std::process::Command;

use super::*;
use crate::model::contexts::{Contexts, Scope};

fn snapshot(contexts: &Contexts) -> ContextsSnapshot {
    ContextsSnapshot::new(contexts, Scope::Global, vec![], 1)
}

fn desired(contexts: &Contexts) -> Commands {
    commands(
        &snapshot(contexts),
        Path::new("/Applications/Sugarglider.app/Contents/MacOS/sugarglider"),
    )
    .unwrap()
}

#[test]
fn create_rename_delete_and_disable_update_only_owned_commands() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("commands");
    let mut contexts = Contexts::default();
    let id = contexts.create("Client work").unwrap();
    sync(&dir, &desired(&contexts)).unwrap();
    let file = dir.join(format!("context-{}.sh", id.get()));
    let before = fs::read_to_string(&file).unwrap();
    assert!(before.contains("# @raycast.title Client work\n"));
    assert!(before.contains(&format!("context switch --id {}", id.get())));
    assert_eq!(0o700, fs::metadata(&file).unwrap().permissions().mode() & 0o777);
    let modified = fs::metadata(&file).unwrap().modified().unwrap();
    sync(&dir, &desired(&contexts)).unwrap();
    assert_eq!(modified, fs::metadata(&file).unwrap().modified().unwrap());
    fs::write(dir.join("personal.sh"), "keep me").unwrap();
    contexts.rename(id, "Renamed project").unwrap();
    sync(&dir, &desired(&contexts)).unwrap();
    assert!(
        fs::read_to_string(&file)
            .unwrap()
            .contains("# @raycast.title Renamed project\n")
    );
    contexts.delete(id).unwrap();
    sync(&dir, &desired(&contexts)).unwrap();
    assert!(!file.exists());
    assert!(dir.join("everything.sh").exists());
    sync(
        &dir,
        &commands(&ContextsSnapshot::off(), Path::new("unused")).unwrap(),
    )
    .unwrap();
    assert!(!dir.join("everything.sh").exists());
    assert_eq!("keep me", fs::read_to_string(dir.join("personal.sh")).unwrap());
}

#[test]
fn disabled_contexts_do_not_create_a_directory() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("absent");
    sync(&dir, &Commands::new()).unwrap();
    assert!(!dir.exists());
}

#[test]
fn script_passes_only_the_stable_id_and_preserves_cli_failure() {
    let root = tempfile::tempdir().unwrap();
    let binary = root.path().join("Tim's $(false) CLI");
    fs::write(
        &binary,
        "#!/bin/bash\nprintf '<%s>\\n' \"$@\"\necho refused >&2\nexit 7\n",
    )
    .unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
    let mut contexts = Contexts::default();
    let id = contexts.create("2024 $(touch SHOULD_NOT_EXIST)").unwrap();
    let scripts = commands(&snapshot(&contexts), &binary).unwrap();
    let file = root.path().join("run.sh");
    fs::write(&file, &scripts[&format!("context-{}.sh", id.get())]).unwrap();
    assert!(Command::new("/bin/bash").arg("-n").arg(&file).status().unwrap().success());
    let result = Command::new("/bin/bash").arg(&file).current_dir(root.path()).output().unwrap();
    assert_eq!(Some(7), result.status.code());
    assert_eq!(
        format!("<context>\n<switch>\n<--id>\n<{}>\nrefused\n", id.get()),
        String::from_utf8(result.stdout).unwrap()
    );
    assert!(!root.path().join("SHOULD_NOT_EXIST").exists());
    fs::remove_file(&binary).unwrap();
    let result = Command::new("/bin/bash").arg(file).output().unwrap();
    assert_eq!(Some(1), result.status.code());
    assert!(String::from_utf8(result.stderr).unwrap().contains("CLI is missing"));
}

#[test]
fn metadata_cannot_inject_a_second_directive_or_shell_line() {
    let script = script(
        "Project\n# @raycast.mode silent\r\nexit 0",
        "'/bin/false'",
        "switch --id 1",
    );
    assert_eq!(
        1,
        script.lines().filter(|line| line.starts_with("# @raycast.mode ")).count()
    );
    assert_eq!(
        1,
        script.lines().filter(|line| line.starts_with("# @raycast.title ")).count()
    );
    assert!(!script.lines().any(|line| line == "exit 0"));
}

#[test]
fn unicode_separators_and_control_only_names_have_indexable_titles() {
    let result = script(
        "Client\u{2028}work\u{2029}today",
        "'/bin/false'",
        "switch --id 1",
    );
    assert!(result.contains("# @raycast.title Client work today\n"));
    assert!(!result.contains(['\u{2028}', '\u{2029}']));
    let result = script("\u{7}\u{1b}", "'/bin/false'", "switch --id 2");
    assert!(result.contains("# @raycast.title Unnamed context\n"));
}

#[test]
fn a_symlinked_server_uses_the_cli_beside_the_real_executable() {
    let root = tempfile::tempdir().unwrap();
    let real = root.path().join("app/bin");
    let links = root.path().join("links");
    fs::create_dir_all(&real).unwrap();
    fs::create_dir(&links).unwrap();
    fs::write(real.join("sugarglider_server"), "server").unwrap();
    let server = links.join("sugarglider_server");
    symlink(real.join("sugarglider_server"), &server).unwrap();
    assert_eq!(
        real.canonicalize().unwrap().join("sugarglider"),
        sibling_cli(&server).unwrap()
    );
}

#[test]
fn edited_and_unowned_files_abort_the_whole_batch() {
    for owned in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let mut contexts = Contexts::default();
        let id = contexts.create("Original").unwrap();
        if owned {
            sync(root.path(), &desired(&contexts)).unwrap();
        }
        let file = root.path().join(format!("context-{}.sh", id.get()));
        fs::write(&file, "user's content").unwrap();
        contexts.rename(id, "New name").unwrap();
        contexts.create("Second").unwrap();
        assert!(sync(root.path(), &desired(&contexts)).is_err());
        assert_eq!("user's content", fs::read_to_string(&file).unwrap());
        assert!(!root.path().join("context-2.sh").exists());
    }
}

#[test]
fn symlinked_commands_manifest_directory_and_lock_are_refused() {
    for name in ["context-1.sh", MANIFEST, ".lock"] {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("keep");
        fs::write(&target, "unchanged").unwrap();
        let dir = root.path().join("commands");
        fs::create_dir(&dir).unwrap();
        symlink(&target, dir.join(name)).unwrap();
        let mut contexts = Contexts::default();
        contexts.create("Work").unwrap();
        assert!(sync(&dir, &desired(&contexts)).is_err());
        assert_eq!("unchanged", fs::read_to_string(target).unwrap());
    }
    let root = tempfile::tempdir().unwrap();
    let target = root.path().join("real");
    fs::create_dir(&target).unwrap();
    let link = root.path().join("link");
    symlink(&target, &link).unwrap();
    assert!(sync(&link, &desired(&Contexts::default())).is_err());
    assert_eq!(0, fs::read_dir(target).unwrap().count());
}

#[test]
fn a_manifest_cannot_claim_a_path_outside_the_command_directory() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("commands");
    fs::create_dir(&dir).unwrap();
    fs::write(root.path().join("keep"), "unchanged").unwrap();
    fs::write(
        dir.join(MANIFEST),
        r#"{"version":1,"files":{"../keep":{"content":"unchanged"}}}"#,
    )
    .unwrap();
    assert!(sync(&dir, &Commands::new()).is_err());
    assert_eq!(
        "unchanged",
        fs::read_to_string(root.path().join("keep")).unwrap()
    );
}

#[test]
fn interrupted_updates_can_finish_from_either_script_version() {
    for replaced in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let mut contexts = Contexts::default();
        contexts.create("Original").unwrap();
        let initial = desired(&contexts);
        sync(root.path(), &initial).unwrap();
        let old = initial["context-1.sh"].clone();
        let id = contexts.contexts()[0].id;
        contexts.rename(id, "Second").unwrap();
        let second = desired(&contexts);
        let mut manifest: Manifest =
            serde_json::from_str(&fs::read_to_string(root.path().join(MANIFEST)).unwrap()).unwrap();
        manifest.files.insert(
            "context-1.sh".into(),
            OwnedFile {
                content: second["context-1.sh"].clone(),
                previous: Some(old),
            },
        );
        fs::write(
            root.path().join(MANIFEST),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        if replaced {
            fs::write(root.path().join("context-1.sh"), &second["context-1.sh"]).unwrap();
        }
        if !replaced {
            contexts.rename(id, "Third").unwrap();
        }
        let third = desired(&contexts);
        sync(root.path(), &third).unwrap();
        assert_eq!(
            third["context-1.sh"],
            fs::read_to_string(root.path().join("context-1.sh")).unwrap()
        );
        let manifest: Manifest =
            serde_json::from_str(&fs::read_to_string(root.path().join(MANIFEST)).unwrap()).unwrap();
        assert!(manifest.files.values().all(|file| file.previous.is_none()));
        sync(root.path(), &Commands::new()).unwrap();
        assert!(!root.path().join("context-1.sh").exists());
    }
}

#[test]
fn a_second_writer_cannot_update_commands_while_the_lock_is_held() {
    let root = tempfile::tempdir().unwrap();
    let lock = File::create(root.path().join(".lock")).unwrap();
    lock.lock().unwrap();
    assert!(sync(root.path(), &desired(&Contexts::default())).is_err());
    assert!(!root.path().join("everything.sh").exists());
    // A concurrent shell test can fork while this descriptor is open.
    lock.unlock().unwrap();
    drop(lock);
    sync(root.path(), &desired(&Contexts::default())).unwrap();
    assert!(root.path().join("everything.sh").exists());
}

#[test]
fn writer_unlocks_even_when_another_descriptor_remains_open() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join(".lock");
    let lock = File::create(&path).unwrap();
    lock.lock().unwrap();
    let inherited = lock.try_clone().unwrap();
    let contender = File::open(&path).unwrap();
    assert!(contender.try_lock().is_err());
    drop(WriterLock(lock));
    contender.try_lock().unwrap();
    contender.unlock().unwrap();
    drop(inherited);
}

#[test]
fn unsorted_is_indexed_only_while_the_snapshot_lists_it() {
    let mut snapshot = snapshot(&Contexts::default());
    snapshot.unsorted.listed = false;
    let binary = Path::new("/usr/local/bin/sugarglider");
    assert!(!commands(&snapshot, binary).unwrap().contains_key("unsorted.sh"));
    snapshot.unsorted.listed = true;
    assert!(commands(&snapshot, binary).unwrap()["unsorted.sh"].contains("switch --name Unsorted"));
}
