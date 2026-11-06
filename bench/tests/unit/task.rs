use std::sync::atomic::{AtomicUsize, Ordering};
use std::{fs, process};

use super::*;

/// A scratch dir under /tmp, removed on drop.
struct Tmp(PathBuf);

impl Tmp {
    fn new() -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!("itse-bench-unit-{}-{}", process::id(), N.fetch_add(1, Ordering::Relaxed)));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

const TOML: &str = "version = \"1.0\"\n[metadata]\ntags = [\"rust\"]\n[agent]\ntimeout_sec = 600\n[verifier]\ntimeout_sec = 120\n[itse_bench]\nuse = [\"rust\"]\nmax_turns = 20\n";

fn write_task(dir: &Path, files: &[(&str, &str)]) {
    for (path, text) in files {
        fs::create_dir_all(dir.join(path).parent().unwrap()).unwrap();
        fs::write(dir.join(path), text).unwrap();
    }
}

fn full_task(dir: &Path) {
    write_task(dir, &[("instruction.md", "Fix it.\n"), ("task.toml", TOML), ("environment/setup.sh", "true\n"), ("tests/test.sh", "true\n"), ("solution/solve.sh", "true\n")]);
}

#[test]
fn a_complete_task_loads_with_its_timeouts_and_bench_keys() {
    let tmp = Tmp::new();
    let dir = tmp.0.join("off-by-one");
    full_task(&dir);
    let task = Task::load(&dir).unwrap();
    assert_eq!(
        (task.id.as_str(), task.tags.clone(), task.agent_timeout, task.verifier_timeout),
        ("off-by-one", vec!["rust".to_string()], Duration::from_secs(600), Duration::from_secs(120))
    );
    assert_eq!((task.bench.uses.clone(), task.bench.max_turns, task.instruction.as_str()), (vec!["rust".to_string()], Some(20), "Fix it.\n"));
    assert_eq!(task.sha.len(), 64);
}

#[test]
fn harbors_float_timeouts_load() {
    let tmp = Tmp::new();
    let dir = tmp.0.join("harbor");
    full_task(&dir);
    fs::write(dir.join("task.toml"), TOML.replace("600", "600.0").replace("120", "90.5")).unwrap();
    let task = Task::load(&dir).unwrap();
    assert_eq!((task.agent_timeout, task.verifier_timeout), (Duration::from_secs(600), Duration::from_millis(90_500)));
}

#[test]
fn the_task_sha_depends_on_paths_and_bytes_not_on_write_order() {
    let tmp = Tmp::new();
    let (a, b) = (tmp.0.join("a"), tmp.0.join("b"));
    write_task(&a, &[("x.txt", "1"), ("sub/y.txt", "2"), ("z", "")]);
    write_task(&b, &[("z", ""), ("sub/y.txt", "2"), ("x.txt", "1")]);
    assert_eq!(sha(&a).unwrap(), sha(&b).unwrap());
    fs::write(b.join("x.txt"), "1 ").unwrap();
    assert_ne!(sha(&a).unwrap(), sha(&b).unwrap(), "a changed byte");
    fs::write(b.join("x.txt"), "1").unwrap();
    fs::rename(b.join("z"), b.join("w")).unwrap();
    assert_ne!(sha(&a).unwrap(), sha(&b).unwrap(), "a renamed file");
    // Moving bytes across a file boundary changes it too, since lengths are hashed.
    let (c, d) = (tmp.0.join("c"), tmp.0.join("d"));
    write_task(&c, &[("p", "ab"), ("q", "")]);
    write_task(&d, &[("p", "a"), ("q", "b")]);
    assert_ne!(sha(&c).unwrap(), sha(&d).unwrap());
}

#[test]
fn the_sha_of_a_known_tree_is_stable() {
    let tmp = Tmp::new();
    write_task(&tmp.0, &[("b", "2"), ("a", "1")]);
    let mut h = Sha256::new();
    h.update(b"a\0\0\0\0\0\0\0\0\x011b\0\0\0\0\0\0\0\0\x012");
    let want: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(sha(&tmp.0).unwrap(), want);
}

#[test]
fn check_task_names_what_is_wrong() {
    let tmp = Tmp::new();
    let err = |dir: &Path| format!("{:#}", Task::load(dir).unwrap_err());
    let dir = tmp.0.join("t1");
    write_task(&dir, &[("instruction.md", "x"), ("task.toml", TOML)]);
    assert_eq!(err(&dir), "t1: missing environment/setup.sh, tests/test.sh, solution/solve.sh");
    full_task(&dir);
    fs::write(dir.join("task.toml"), "version = \"1.0\"\n[agent]\ntimeout_sec = 6x\n").unwrap();
    assert_eq!(err(&dir), "t1/task.toml: 3:15: '6x' is not a string, number, boolean or array");
    fs::write(dir.join("task.toml"), "version = \"1.0\"\n[agent]\ntimeout_sec = 6\n[verifier]\ntimeout_sec = 6\n").unwrap();
    assert_eq!(err(&dir), "t1/task.toml: no metadata");
    fs::write(dir.join("task.toml"), TOML.replace("max_turns", "max_turn")).unwrap();
    assert!(err(&dir).starts_with("t1/task.toml: [itse_bench]: unknown field `max_turn`"), "{}", err(&dir));
    for bad in ["0", "0.0", "-5.0", "1e12"] {
        fs::write(dir.join("task.toml"), TOML.replace("600", bad)).unwrap();
        assert_eq!(err(&dir), "t1/task.toml: [agent] timeout_sec must be a number above 0", "{bad}");
        fs::write(dir.join("task.toml"), TOML.replace("120", bad)).unwrap();
        assert_eq!(err(&dir), "t1/task.toml: [verifier] timeout_sec must be a number above 0", "{bad}");
    }
    fs::write(dir.join("task.toml"), TOML.replace("600", "\"600\"")).unwrap();
    assert_eq!(err(&dir), "t1/task.toml: [agent] timeout_sec must be a number above 0", "a string is no number");
    fs::write(dir.join("task.toml"), TOML.replace("[\"rust\"]\n[agent]", "\"rust\"\n[agent]")).unwrap();
    assert!(err(&dir).starts_with("t1/task.toml: [metadata] tags: invalid type: string \"rust\", expected a sequence"), "{}", err(&dir));
    fs::write(dir.join("task.toml"), TOML).unwrap();
    fs::write(dir.join("instruction.md"), " \n").unwrap();
    assert_eq!(err(&dir), "t1/instruction.md is empty");
    fs::write(dir.join("instruction.md"), "x").unwrap();
    // A Harbor verifier that reports through reward.txt and exits 0 would always pass here.
    fs::write(dir.join("tests/test.sh"), "pytest; echo $? >/logs/verifier/reward.txt\n").unwrap();
    assert_eq!(err(&dir), "t1/tests/test.sh: the verdict is its exit status, not /logs/verifier/reward.txt");
    fs::write(dir.join("tests/test.sh"), "true\n").unwrap();
    std::os::unix::fs::symlink("/nonexistent", dir.join("tests/extra")).unwrap();
    assert_eq!(err(&dir), "t1: tests/extra is not a file or a dir", "the task's id and a path inside it");
    let bad = tmp.0.join("Bad_Id");
    full_task(&bad);
    assert_eq!(err(&bad), "task id 'Bad_Id' is not lowercase letters, digits and '-'");
}

#[test]
fn a_tasks_dir_lists_its_tasks_by_name_and_a_task_dir_itself() {
    let tmp = Tmp::new();
    for id in ["b", "a"] {
        full_task(&tmp.0.join(id));
    }
    fs::create_dir_all(tmp.0.join("not-a-task")).unwrap();
    assert_eq!(list(&tmp.0).unwrap(), vec![tmp.0.join("a"), tmp.0.join("b")]);
    // A task whose task.toml is misnamed is listed, so it fails to load rather than being skipped.
    full_task(&tmp.0.join("c"));
    fs::rename(tmp.0.join("c/task.toml"), tmp.0.join("c/task.tml")).unwrap();
    assert_eq!(list(&tmp.0).unwrap(), vec![tmp.0.join("a"), tmp.0.join("b"), tmp.0.join("c")]);
    assert_eq!(format!("{:#}", Task::load(&tmp.0.join("c")).unwrap_err()), "c: missing task.toml");
    assert_eq!(list(&tmp.0.join("a")).unwrap(), vec![tmp.0.join("a")]);
    assert!(list(&tmp.0.join("not-a-task")).is_err());
}
