//! Real-process regressions for early-closing CLI pipeline consumers.

#[cfg(unix)]
mod unix {
    use std::fs;
    use std::os::unix::process::ExitStatusExt;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use tempfile::TempDir;
    use worksgood::graph::{Node, Task, WorkGraph};
    use worksgood::parser::save_graph;

    fn wg_binary() -> PathBuf {
        let mut path = std::env::current_exe().expect("could not get current exe path");
        path.pop();
        if path.ends_with("deps") {
            path.pop();
        }
        path.push("wg");
        assert!(
            path.exists(),
            "wg binary not found at {:?}. Run `cargo build` first.",
            path
        );
        path
    }

    fn wg_command(wg_dir: &Path) -> Command {
        let global_dir = wg_dir.parent().unwrap_or(wg_dir).join("isolated-global-wg");
        fs::create_dir_all(&global_dir).expect("could not create isolated global WG directory");

        let mut command = Command::new(wg_binary());
        command
            .arg("--dir")
            .arg(wg_dir)
            // Do not let a developer's global config or active profile change
            // real-process integration behavior.
            .env("WG_GLOBAL_DIR", global_dir);
        command
    }

    fn run(wg_dir: &Path, args: &[&str]) -> std::process::Output {
        wg_command(wg_dir)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .unwrap_or_else(|error| panic!("failed to run wg {args:?}: {error}"))
    }

    #[test]
    fn show_to_head_exits_quietly_and_preserves_normal_output() {
        let tmp = TempDir::new().unwrap();
        let wg_dir = tmp.path().join(".wg");
        fs::create_dir_all(&wg_dir).unwrap();

        let description = (0..3_000)
            .map(|index| format!("pipeline regression line {index:05}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(Task {
            id: "pipe-task".to_string(),
            title: "Pipeline regression task".to_string(),
            description: Some(description),
            ..Task::default()
        }));
        save_graph(&graph, &wg_dir.join("graph.jsonl")).unwrap();

        let normal = run(&wg_dir, &["show", "pipe-task"]);
        assert!(
            normal.status.success(),
            "normal wg show failed: {}",
            String::from_utf8_lossy(&normal.stderr)
        );
        assert!(normal.stderr.is_empty(), "normal wg show wrote to stderr");
        let normal_stdout = String::from_utf8(normal.stdout).unwrap();
        assert!(normal_stdout.starts_with("Task: pipe-task\n"));
        assert!(normal_stdout.contains("pipeline regression line 02999"));

        // Exercise the actual human terminal flow: the real wg process writes
        // directly into the real `head -n 1` process, which closes the pipe as
        // soon as it has consumed the first line.
        let mut wg = wg_command(&wg_dir)
            .args(["show", "pipe-task"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn wg show");
        let wg_stdout = wg.stdout.take().expect("wg stdout was not piped");
        let head = Command::new("head")
            .args(["-n", "1"])
            .stdin(Stdio::from(wg_stdout))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn head -n 1")
            .wait_with_output()
            .expect("failed to wait for head");
        let wg_output = wg.wait_with_output().expect("failed to wait for wg show");

        assert!(head.status.success(), "head failed: {head:?}");
        assert_eq!(head.stdout, b"Task: pipe-task\n");
        assert!(head.stderr.is_empty(), "head wrote to stderr: {head:?}");
        assert_eq!(
            wg_output.status.signal(),
            Some(libc::SIGPIPE),
            "wg show should terminate by the conventional SIGPIPE without a panic; status={:?}, stderr={}",
            wg_output.status,
            String::from_utf8_lossy(&wg_output.stderr)
        );
        assert!(
            wg_output.stderr.is_empty(),
            "wg show emitted a panic/backtrace: {}",
            String::from_utf8_lossy(&wg_output.stderr)
        );
    }

    #[test]
    fn hostile_ambient_global_config_cannot_reach_wg_children() {
        let tmp = TempDir::new().unwrap();
        let hostile_global_dir = tmp.path().join("hostile-ambient-global-marker");
        fs::create_dir_all(&hostile_global_dir).unwrap();
        fs::write(
            hostile_global_dir.join("config.toml"),
            "this is deliberately invalid TOML = [\n",
        )
        .unwrap();

        // Run the real-process regression in a nested test process whose
        // inherited WG_GLOBAL_DIR is hostile. The local command builder must
        // replace it before either built `wg` child starts.
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "unix::show_to_head_exits_quietly_and_preserves_normal_output",
                "--nocapture",
            ])
            .env("WG_GLOBAL_DIR", &hostile_global_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("failed to run nested hostile-environment regression");

        assert!(
            output.status.success(),
            "built wg child read hostile ambient config\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("1 passed"),
            "nested regression did not execute exactly one test:\n{}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
}
