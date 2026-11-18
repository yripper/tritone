use super::*;

// Env vars are process-global, so every test that touches TRITONE_HOME
// goes through this one test.
#[test]
fn paths_respect_tritone_home_and_config_defaults_port_7781() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("state");
    unsafe { std::env::set_var("TRITONE_HOME", &home) };
    let p = Paths::resolve().unwrap();
    assert_eq!(p.db, home.join("tritone.db"));
    assert_eq!(p.style, home.join("style.md"));
    assert_eq!(p.config, home.join("config.json"));
    let mode = std::fs::metadata(&home).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700);

    assert_eq!(Config::load(&p).unwrap().port, 7781);
    std::fs::write(&p.config, r#"{"port": 8000}"#).unwrap();
    assert_eq!(Config::load(&p).unwrap().port, 8000);
    unsafe { std::env::remove_var("TRITONE_HOME") };
}

fn paths_in(dir: &std::path::Path) -> Paths {
    Paths {
        home: dir.to_path_buf(),
        db: dir.join("tritone.db"),
        config: dir.join("config.json"),
        style: dir.join("style.md"),
    }
}

#[test]
fn config_defaults_when_missing() {
    let dir = tempfile::tempdir().unwrap();
    let c = Config::load(&paths_in(dir.path())).unwrap();
    assert_eq!(c.port, 7781);
    assert!(c.topics.is_empty());
    assert_eq!(c.unanswered_after_mins, 30);
    assert_eq!(c.unanswered_max_age_hours, 48);
    assert_eq!(c.recent_per_channel, 200);
    assert_eq!(c.recent_max_age_hours, 24);
}

#[test]
fn config_reads_topics() {
    let dir = tempfile::tempdir().unwrap();
    let p = paths_in(dir.path());
    std::fs::write(&p.config, r#"{"topics":[{"kind":"my-project","keywords":["my-project","myproj"]}]}"#).unwrap();
    let c = Config::load(&p).unwrap();
    assert_eq!(
        c.topics,
        vec![Topic { kind: "my-project".into(), keywords: vec!["my-project".into(), "myproj".into()] }]
    );
    // Fields not in the file keep their defaults.
    assert_eq!(c.port, 7781);
    assert_eq!(c.recent_per_channel, 200);
}
