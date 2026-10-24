//! 运行选项测试：路径、实例目录、监听约束与直接使用原生配置。
//! 测试中的 unwrap 使意外错误直接导致用例失败，便于定位与报告。

use std::path::{Path, PathBuf};

use a2alinker::options::{AgentKind, AuthMode, Options};

fn options(root: &Path) -> Options {
    let mut cfg = Options::default();
    cfg.agent.kind = AgentKind::Codex;
    cfg.agent.env.insert(
        "CODEX_HOME".into(),
        root.join("user-codex").display().to_string(),
    );
    cfg.agent.workspace = root.into();
    cfg.storage.directory = root.join("data");
    cfg
}

#[test]
fn defaults_match_native_pi_and_paths_resolve_against_the_current_process() {
    let mut cfg = Options::default();
    assert_eq!(cfg.agent.kind, AgentKind::Pi);
    assert_eq!(cfg.identity.name, "Alan");
    assert_eq!(cfg.server.listen, "127.0.0.1:10100".parse().unwrap());
    assert_eq!(cfg.storage.directory, PathBuf::from("~/.rein/storage"));
    assert!(cfg.skills.directories.is_empty());
    assert!(cfg.native_config_directory().unwrap().is_none());
    assert!(cfg.agent.allow_extensions);
    assert!(cfg.agent.sandbox.is_none());
    assert!(cfg.agent.approval_policy.is_none());
    cfg.agent.workspace = ".".into();
    cfg.storage.directory = "state".into();
    cfg.resolve().unwrap();
    assert_eq!(
        cfg.agent.workspace.canonicalize().unwrap(),
        std::env::current_dir().unwrap()
    );
    assert_eq!(
        cfg.storage.directory,
        std::env::current_dir().unwrap().join("state")
    );
    for value in ["", " ", "  \t"] {
        cfg.agent.executable = Some(value.into());
        assert_eq!(cfg.executable(), PathBuf::from("pi"));
    }
    cfg.agent.kind = AgentKind::Codex;
    assert_eq!(cfg.executable(), PathBuf::from("codex"));
}

#[test]
fn configuration_rejects_unsecured_listeners_and_unsupported_parallelism() {
    let temp = tempfile::tempdir().unwrap();
    let mut cfg = options(temp.path());
    cfg.server.listen = "0.0.0.0:10100".parse().unwrap();
    assert!(cfg.validate().is_err());
    cfg.auth.mode = AuthMode::Bearer;
    cfg.validate().unwrap();
    cfg.runtime.max_concurrent_tasks = 2;
    assert!(cfg.validate().is_err());
}

#[test]
fn instance_storage_uses_the_name_without_mutating_the_shared_root() {
    let temp = tempfile::tempdir().unwrap();
    let mut cfg = options(temp.path());
    let root = cfg.storage.directory.clone();
    cfg.resolve().unwrap();
    cfg.resolve().unwrap();
    assert_eq!(cfg.storage.directory, root);
    assert_eq!(cfg.instance_directory().unwrap(), root.join("Alan"));
    cfg.identity.name = "Bob".into();
    assert_eq!(cfg.instance_directory().unwrap(), root.join("Bob"));
    assert_eq!(cfg.storage.directory, root);
    for name in [
        "", " ", ".", "..", "../other", "a/b", "a\\b", "/tmp", "C:other", "a\0b",
    ] {
        cfg.identity.name = name.into();
        assert!(cfg.validate().is_err(), "unsafe instance name: {name:?}");
    }
}

#[test]
fn explicit_native_configuration_uses_its_directory_without_copying() {
    let temp = tempfile::tempdir().unwrap();
    for (kind, filename) in [
        (AgentKind::Pi, "settings.json"),
        (AgentKind::Codex, "config.toml"),
    ] {
        let directory = temp.path().join(kind.as_str());
        std::fs::create_dir(&directory).unwrap();
        let file = directory.join(filename);
        std::fs::write(&file, "native contents are parsed only by the agent").unwrap();
        let mut options = options(temp.path());
        options.agent.kind = kind;
        for path in [&directory, &file] {
            options.agent.native_config = Some(path.clone());
            options.resolve().unwrap();
            assert_eq!(
                options.native_config_directory().unwrap(),
                Some(directory.clone())
            );
        }
        assert!(!options.instance_directory().unwrap().exists());
        assert_eq!(
            std::fs::read_to_string(file).unwrap(),
            "native contents are parsed only by the agent"
        );
        let unsupported = directory.join("arbitrary.conf");
        std::fs::write(&unsupported, "{}").unwrap();
        options.agent.native_config = Some(unsupported);
        assert!(
            options
                .validate()
                .unwrap_err()
                .to_string()
                .contains("not copied or renamed")
        );
        options.agent.native_config = Some(directory.join("missing"));
        assert!(options.validate().is_err());
    }
}

#[test]
fn unspecified_configuration_is_left_to_native_discovery_and_skills_remain_source_paths() {
    let temp = tempfile::tempdir().unwrap();
    let mut options = options(temp.path());
    options.agent.env.insert(
        "CODEX_HOME".into(),
        temp.path().join("not-yet-created").display().to_string(),
    );
    assert!(options.native_config_directory().unwrap().is_none());
    let skills = temp.path().join("skills");
    std::fs::create_dir(&skills).unwrap();
    // 不在 a2alinker 中解析或复制技能，由原生 agent 处理其发现规则。
    std::fs::write(skills.join("SKILL.md"), "native skill format").unwrap();
    options.skills.directories.push(skills.clone());
    options.resolve().unwrap();
    assert_eq!(options.skills.directories, vec![skills]);
    options
        .skills
        .directories
        .push(temp.path().join("missing-skills"));
    assert!(options.validate().is_err());
}
