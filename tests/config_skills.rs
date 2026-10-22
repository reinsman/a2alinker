//! 配置和技能的文件系统测试：用临时目录验证路径、摘要、筛选和非法输入。
//! 测试中的 unwrap 使意外错误直接导致用例失败，便于定位与报告。

use std::path::{Path, PathBuf};

use a2alinker::{
    adapters,
    cli::{Cli, Command},
    config::{AgentKind, AuthMode, Config},
    skills::SkillSet,
};
use clap::Parser;

fn config(root: &Path) -> Config {
    let mut cfg = Config {
        schema_version: 1,
        ..Config::default()
    };
    cfg.agent.workspace = root.into();
    cfg.storage.directory = root.join("data");
    cfg
}

fn skill(root: &Path, name: &str) {
    std::fs::create_dir_all(root.join("scripts")).unwrap();
    std::fs::write(
        root.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: Review source code\n---\nRead scripts/check.sh\n"),
    )
    .unwrap();
    std::fs::write(root.join("scripts/check.sh"), "echo checked\n").unwrap();
}

#[test]
fn file_paths_are_relative_to_config_and_cli_paths_to_current_directory() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(temp.path().join("deploy")).unwrap();
    let path = temp.path().join("deploy/config.toml");
    std::fs::write(&path, "schema_version = 1\n[agent]\nkind = 'pi'\nworkspace = '..'\n[storage]\ndirectory = '../state'\n").unwrap();
    let cfg = Config::load(Some(&path)).unwrap();
    assert_eq!(cfg.agent.kind, AgentKind::Pi);
    assert_eq!(
        cfg.agent.workspace.canonicalize().unwrap(),
        temp.path().canonicalize().unwrap()
    );
    assert_eq!(cfg.storage.directory, temp.path().join("deploy/../state"));
    let cli = Cli::try_parse_from([
        "a2alinker",
        "serve",
        "--config",
        path.to_str().unwrap(),
        "--agent",
        "codex",
        "--workspace",
        ".",
    ])
    .unwrap();
    let Command::Serve(args) = cli.command else {
        panic!()
    };
    let cfg = args.resolve().unwrap();
    assert_eq!(cfg.agent.kind, AgentKind::Codex);
    assert_eq!(
        cfg.agent.workspace,
        std::env::current_dir().unwrap().join(".")
    );
}

#[test]
fn configuration_rejects_typos_unsecured_listeners_and_unsupported_parallelism() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("config.toml");
    std::fs::write(&path, "schema_version = 1\n[agent]\nworkspce = '.'\n").unwrap();
    assert!(Config::load(Some(&path)).is_err());
    let mut cfg = config(temp.path());
    cfg.server.listen = "0.0.0.0:8080".parse().unwrap();
    assert!(cfg.validate().is_err());
    cfg.auth.mode = AuthMode::Bearer;
    cfg.validate().unwrap();
    cfg.runtime.max_concurrent_tasks = 2;
    assert!(cfg.validate().is_err());
}

#[test]
fn skill_packages_are_snapshotted_with_supporting_files_and_detect_changes() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("skills/review");
    skill(&root, "review");
    let mut cfg = config(temp.path());
    cfg.skills.directories.push(root.clone());
    let discovered = SkillSet::discover(&cfg).unwrap();
    let original_digest = discovered.digest.clone();
    let snapshot = discovered.snapshot(&cfg.storage.directory).unwrap();
    assert_eq!(
        std::fs::read_to_string(
            snapshot.skills[0]
                .path
                .parent()
                .unwrap()
                .join("scripts/check.sh")
        )
        .unwrap(),
        "echo checked\n"
    );
    std::fs::write(root.join("scripts/check.sh"), "echo changed\n").unwrap();
    assert_ne!(SkillSet::discover(&cfg).unwrap().digest, original_digest);
    assert_eq!(
        std::fs::read_to_string(
            snapshot.skills[0]
                .path
                .parent()
                .unwrap()
                .join("scripts/check.sh")
        )
        .unwrap(),
        "echo checked\n"
    );
}

#[test]
fn duplicate_and_missing_skills_are_rejected_and_names_filter_explicit_packages() {
    let temp = tempfile::tempdir().unwrap();
    skill(&temp.path().join("a"), "review");
    skill(&temp.path().join("b"), "review");
    let mut cfg = config(temp.path());
    cfg.skills.directories = vec![temp.path().join("a"), temp.path().join("b")];
    assert!(SkillSet::discover(&cfg).is_err());
    cfg.skills.directories.pop();
    cfg.skills.enabled.push("missing".into());
    assert!(SkillSet::discover(&cfg).is_err());
    cfg.skills.enabled = vec!["review".into()];
    assert_eq!(SkillSet::discover(&cfg).unwrap().skills.len(), 1);
}

#[test]
fn native_configuration_content_changes_session_fingerprint() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("config.toml");
    std::fs::write(&source, "model = 'first'\n").unwrap();
    let mut cfg = config(temp.path());
    cfg.agent.native_config = Some(source.clone());
    let first = cfg.fingerprint("").unwrap();
    std::fs::write(source, "model = 'second'\n").unwrap();
    assert_ne!(first, cfg.fingerprint("").unwrap());
}

#[test]
fn pi_defaults_copy_user_settings_and_auth_and_track_content_changes() {
    let temp = tempfile::tempdir().unwrap();
    let user_home = temp.path().join("user-home");
    let directory = user_home.join(".pi/agent");
    std::fs::create_dir_all(&directory).unwrap();
    let source = directory.join("settings.json");
    let first_settings = r#"{"defaultProvider":"example","defaultModel":"first"}"#;
    std::fs::write(&source, first_settings).unwrap();
    std::fs::write(
        directory.join("auth.json"),
        r#"{"example":"test-credential"}"#,
    )
    .unwrap();
    let mut cfg = config(temp.path());
    cfg.agent.kind = AgentKind::Pi;
    cfg.agent
        .env
        .insert("HOME".into(), user_home.display().to_string());
    // 空值排除宿主机可能存在的覆盖项，测试仅访问临时目录中的用户配置。
    cfg.agent
        .env
        .insert("PI_CODING_AGENT_DIR".into(), String::new());
    assert_eq!(cfg.effective_native_config().unwrap(), Some(source.clone()));
    let first_digest = cfg.fingerprint("").unwrap();
    let first_home = adapters::prepare_home(&cfg, "").unwrap();
    assert_eq!(
        std::fs::read_to_string(first_home.join("settings.json")).unwrap(),
        first_settings
    );
    assert_eq!(
        std::fs::read(first_home.join("auth.json")).unwrap(),
        std::fs::read(directory.join("auth.json")).unwrap()
    );
    assert_eq!(std::fs::read_to_string(&source).unwrap(), first_settings);

    let second_settings = r#"{"defaultProvider":"example","defaultModel":"second"}"#;
    std::fs::write(&source, second_settings).unwrap();
    assert_ne!(first_digest, cfg.fingerprint("").unwrap());
    let second_home = adapters::prepare_home(&cfg, "").unwrap();
    assert_ne!(first_home, second_home);
    assert_eq!(
        std::fs::read_to_string(second_home.join("settings.json")).unwrap(),
        second_settings
    );
    assert_eq!(
        std::fs::read_to_string(first_home.join("settings.json")).unwrap(),
        first_settings
    );
    assert_eq!(std::fs::read_to_string(source).unwrap(), second_settings);
}

#[test]
fn pi_custom_directory_and_explicit_native_config_have_defined_precedence() {
    let temp = tempfile::tempdir().unwrap();
    let directory = temp.path().join("custom-pi");
    std::fs::create_dir_all(&directory).unwrap();
    let default = directory.join("settings.json");
    std::fs::write(&default, r#"{"defaultModel":"from-directory"}"#).unwrap();
    let mut cfg = config(temp.path());
    cfg.agent.kind = AgentKind::Pi;
    cfg.agent.env.insert(
        "PI_CODING_AGENT_DIR".into(),
        directory.display().to_string(),
    );
    assert_eq!(cfg.effective_native_config().unwrap(), Some(default));
    let home = adapters::prepare_home(&cfg, "").unwrap();
    assert_eq!(
        std::fs::read_to_string(home.join("settings.json")).unwrap(),
        r#"{"defaultModel":"from-directory"}"#
    );

    // 显式指定空配置也必须优先，用户可以主动选择不继承默认 settings。
    let explicit = temp.path().join("empty-settings.json");
    std::fs::write(&explicit, "{}").unwrap();
    cfg.agent.native_config = Some(explicit.clone());
    assert_eq!(cfg.effective_native_config().unwrap(), Some(explicit));
    let home = adapters::prepare_home(&cfg, "").unwrap();
    assert_eq!(
        std::fs::read_to_string(home.join("settings.json")).unwrap(),
        "{}"
    );
    cfg.agent.native_config = Some(temp.path().join("missing-settings.json"));
    assert!(cfg.validate().is_err());
}

#[test]
fn pi_missing_default_settings_use_built_in_defaults_but_directories_are_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let directory = temp.path().join("pi-user");
    let mut cfg = config(temp.path());
    cfg.agent.kind = AgentKind::Pi;
    cfg.agent.env.insert(
        "PI_CODING_AGENT_DIR".into(),
        directory.display().to_string(),
    );
    assert!(cfg.effective_native_config().unwrap().is_none());
    cfg.validate().unwrap();
    let home = adapters::prepare_home(&cfg, "").unwrap();
    assert!(!home.join("settings.json").exists());
    std::fs::create_dir_all(directory.join("settings.json")).unwrap();
    assert!(cfg.validate().is_err());
}

#[test]
fn pi_default_configuration_is_selected_after_cli_agent_override() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("settings.json");
    std::fs::write(&source, "{}").unwrap();
    let path = temp.path().join("gateway.toml");
    std::fs::write(
        &path,
        format!("schema_version = 1\n[agent]\nkind = 'codex'\nworkspace = '.'\n[agent.env]\nPI_CODING_AGENT_DIR = '{}'\n", temp.path().display()),
    ).unwrap();
    let cli = Cli::try_parse_from([
        "a2alinker",
        "config",
        "validate",
        "--config",
        path.to_str().unwrap(),
        "--agent",
        "pi",
    ])
    .unwrap();
    let Command::Config {
        command: a2alinker::cli::ConfigCommand::Validate(args),
    } = cli.command
    else {
        panic!()
    };
    let cfg = args.resolve().unwrap();
    assert_eq!(cfg.effective_native_config().unwrap(), Some(source));
}

#[cfg(unix)]
#[test]
fn symlinks_inside_skill_packages_are_rejected() {
    let temp = tempfile::tempdir().unwrap();
    skill(&temp.path().join("skill"), "review");
    std::os::unix::fs::symlink("/etc", temp.path().join("skill/references")).unwrap();
    let mut cfg = config(temp.path());
    cfg.skills.directories.push(temp.path().join("skill"));
    assert!(SkillSet::discover(&cfg).is_err());
}

#[test]
fn bundled_examples_validate_and_have_loadable_skills() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for example in ["pi.toml", "pi-nvm.toml", "codex.toml"] {
        let cfg = Config::load(Some(&root.join("examples").join(example))).unwrap();
        cfg.validate().unwrap();
        assert_eq!(SkillSet::discover(&cfg).unwrap().skills.len(), 1);
    }
}

#[test]
fn skill_snapshots_cannot_recursively_copy_their_own_storage_directory() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("skill");
    skill(&root, "review");
    let mut cfg = config(temp.path());
    cfg.skills.directories.push(root.clone());
    cfg.storage.directory = root.join("data");
    let discovered = SkillSet::discover(&cfg).unwrap();
    assert!(discovered.snapshot(&cfg.storage.directory).is_err());
}
