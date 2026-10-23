//! 运行选项和技能的文件系统测试：用临时目录验证路径、摘要、筛选和非法输入。
//! 测试中的 unwrap 使意外错误直接导致用例失败，便于定位与报告。

use std::path::{Path, PathBuf};

use a2alinker::{
    adapters,
    options::{AgentKind, AuthMode, Options},
    skills::SkillSet,
};

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
fn defaults_match_native_pi_and_paths_resolve_against_the_current_process() {
    let mut cfg = Options::default();
    assert_eq!(cfg.agent.kind, AgentKind::Pi);
    assert_eq!(cfg.identity.name, "Alan");
    assert_eq!(cfg.server.listen, "127.0.0.1:10100".parse().unwrap());
    assert_eq!(cfg.storage.directory, PathBuf::from("~/.rein/storage"));
    assert!(cfg.skills.inherit_user);
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
fn native_skills_do_not_inject_an_empty_explicit_root() {
    let temp = tempfile::tempdir().unwrap();
    let cfg = options(temp.path());
    let snapshot = SkillSet::discover(&cfg)
        .unwrap()
        .snapshot(&cfg.storage.directory)
        .unwrap();
    assert!(snapshot.root.is_none());
}

#[test]
fn codex_inherited_skills_preserve_default_native_configuration() {
    let temp = tempfile::tempdir().unwrap();
    let user = temp.path().join("user-codex");
    std::fs::create_dir_all(&user).unwrap();
    let source = user.join("config.toml");
    let content = "[[skills.config]]\npath = '/example/disabled/SKILL.md'\nenabled = false\n";
    std::fs::write(&source, content).unwrap();
    let mut cfg = options(temp.path());
    cfg.agent
        .env
        .insert("CODEX_HOME".into(), user.display().to_string());
    cfg.agent.inherit_auth = false;
    assert_eq!(cfg.effective_native_config().unwrap(), Some(source.clone()));
    let fingerprint = cfg.fingerprint("").unwrap();
    let home = adapters::prepare_home(&cfg, "").unwrap();
    assert_eq!(
        std::fs::read_to_string(home.join("config.toml")).unwrap(),
        content
    );
    assert_eq!(std::fs::read_to_string(&source).unwrap(), content);
    std::fs::write(&source, content.replace("false", "true")).unwrap();
    assert_ne!(cfg.fingerprint("").unwrap(), fingerprint);
    cfg.skills.inherit_user = false;
    assert_eq!(cfg.effective_native_config().unwrap(), Some(source));
    let explicit = temp.path().join("explicit.toml");
    std::fs::write(&explicit, "").unwrap();
    cfg.agent.native_config = Some(explicit.clone());
    assert_eq!(cfg.effective_native_config().unwrap(), Some(explicit));
}

#[cfg(unix)]
#[test]
fn private_agent_home_links_native_skills_only_when_inheritance_is_enabled() {
    for kind in [AgentKind::Pi, AgentKind::Codex] {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("user-agent");
        skill(&source.join("skills/native-review"), "native-review");
        std::fs::create_dir_all(source.join("git")).unwrap();
        std::fs::create_dir_all(source.join("npm")).unwrap();
        for name in ["extensions", "prompts", "themes"] {
            std::fs::create_dir_all(source.join(name)).unwrap();
        }
        std::fs::write(source.join("trust.json"), "{}").unwrap();
        let mut cfg = options(temp.path());
        cfg.agent.kind = kind;
        cfg.agent.inherit_auth = false;
        let env_key = if kind == AgentKind::Pi {
            "PI_CODING_AGENT_DIR"
        } else {
            "CODEX_HOME"
        };
        cfg.agent
            .env
            .insert(env_key.into(), source.display().to_string());
        let home = adapters::prepare_home(&cfg, "").unwrap();
        assert!(
            std::fs::symlink_metadata(home.join("skills"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            home.join("skills/native-review/SKILL.md")
                .canonicalize()
                .unwrap(),
            source
                .join("skills/native-review/SKILL.md")
                .canonicalize()
                .unwrap()
        );
        assert_eq!(home.join("git").exists(), kind == AgentKind::Pi);
        assert_eq!(home.join("npm").exists(), kind == AgentKind::Pi);
        assert_eq!(home.join("trust.json").exists(), kind == AgentKind::Pi);
        for name in ["extensions", "prompts", "themes"] {
            assert_eq!(home.join(name).exists(), kind == AgentKind::Pi);
        }
        assert_eq!(adapters::prepare_home(&cfg, "").unwrap(), home);
        cfg.skills.inherit_user = false;
        let managed = adapters::prepare_home(&cfg, "").unwrap();
        assert!(!managed.join("skills").exists());
        assert_eq!(managed.join("git").exists(), kind == AgentKind::Pi);
        cfg.agent.allow_extensions = false;
        let restricted = adapters::prepare_home(&cfg, "").unwrap();
        assert!(!restricted.join("extensions").exists());
    }
}

#[test]
fn skill_packages_are_snapshotted_with_supporting_files_and_detect_changes() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("skills/review");
    skill(&root, "review");
    let mut cfg = options(temp.path());
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
    let mut cfg = options(temp.path());
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
    let mut cfg = options(temp.path());
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
    let mut cfg = options(temp.path());
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
    let mut cfg = options(temp.path());
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
    let mut cfg = options(temp.path());
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
fn native_configuration_follows_the_selected_agent() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("settings.json");
    std::fs::write(&source, "{}").unwrap();
    let mut cfg = options(temp.path());
    cfg.agent.env.insert(
        "PI_CODING_AGENT_DIR".into(),
        temp.path().display().to_string(),
    );
    assert!(cfg.effective_native_config().unwrap().is_none());
    cfg.agent.kind = AgentKind::Pi;
    assert_eq!(cfg.effective_native_config().unwrap(), Some(source));
}

#[cfg(unix)]
#[test]
fn symlinks_inside_skill_packages_are_rejected() {
    let temp = tempfile::tempdir().unwrap();
    skill(&temp.path().join("skill"), "review");
    std::os::unix::fs::symlink("/etc", temp.path().join("skill/references")).unwrap();
    let mut cfg = options(temp.path());
    cfg.skills.directories.push(temp.path().join("skill"));
    assert!(SkillSet::discover(&cfg).is_err());
}

#[test]
fn pi_default_models_are_copied_hashed_and_keep_private_permissions() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("pi-user");
    std::fs::create_dir(&source).unwrap();
    let models = source.join("models.json");
    let first =
        r#"{"providers":{"example":{"baseUrl":"https://first.invalid","apiKey":"test-token"}}}"#;
    std::fs::write(&models, first).unwrap();
    let mut cfg = options(temp.path());
    cfg.agent.kind = AgentKind::Pi;
    cfg.agent.inherit_auth = false;
    cfg.agent
        .env
        .insert("PI_CODING_AGENT_DIR".into(), source.display().to_string());
    // 没有 settings.json 时，也要继承用户定义的模型列表。
    assert!(cfg.effective_native_config().unwrap().is_none());
    assert_eq!(cfg.native_model_config().unwrap(), Some(models.clone()));
    let digest = cfg.fingerprint("").unwrap();
    let home = adapters::prepare_home(&cfg, "").unwrap();
    assert_eq!(
        std::fs::read_to_string(home.join("models.json")).unwrap(),
        first
    );
    assert_eq!(std::fs::read_to_string(&models).unwrap(), first);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(home.join("models.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    std::fs::write(&models, first.replace("first.invalid", "second.invalid")).unwrap();
    assert_ne!(digest, cfg.fingerprint("").unwrap());
    let changed = adapters::prepare_home(&cfg, "").unwrap();
    assert_ne!(home, changed);
    assert_eq!(
        std::fs::read_to_string(home.join("models.json")).unwrap(),
        first
    );
}

#[test]
fn pi_explicit_file_and_directory_configs_include_their_models() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("dedicated-pi");
    std::fs::create_dir(&source).unwrap();
    let settings = source.join("settings.json");
    let models = source.join("models.json");
    std::fs::write(&settings, r#"{"defaultProvider":"example"}"#).unwrap();
    std::fs::write(&models, r#"{"providers":{"example":{}}}"#).unwrap();
    let mut cfg = options(temp.path());
    cfg.agent.kind = AgentKind::Pi;
    cfg.agent.inherit_auth = false;
    cfg.agent.native_config = Some(settings);
    assert_eq!(cfg.native_model_config().unwrap(), Some(models.clone()));
    let home = adapters::prepare_home(&cfg, "").unwrap();
    assert_eq!(
        std::fs::read(home.join("models.json")).unwrap(),
        std::fs::read(&models).unwrap()
    );
    cfg.agent.native_config = Some(source);
    assert!(cfg.native_model_config().unwrap().is_none());
    let home = adapters::prepare_home(&cfg, "").unwrap();
    assert_eq!(
        std::fs::read(home.join("models.json")).unwrap(),
        std::fs::read(models).unwrap()
    );
}

#[test]
fn pi_rejects_a_directory_in_place_of_models_json() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("pi-user");
    std::fs::create_dir_all(source.join("models.json")).unwrap();
    let mut cfg = options(temp.path());
    cfg.agent.kind = AgentKind::Pi;
    cfg.agent
        .env
        .insert("PI_CODING_AGENT_DIR".into(), source.display().to_string());
    assert!(cfg.validate().is_err());
}

#[test]
fn fixture_skill_package_is_available_to_both_adapters() {
    let temp = tempfile::tempdir().unwrap();
    for kind in [AgentKind::Pi, AgentKind::Codex] {
        let mut cfg = options(temp.path());
        cfg.agent.kind = kind;
        cfg.skills.directories =
            vec![Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/skills")];
        assert_eq!(SkillSet::discover(&cfg).unwrap().skills.len(), 1);
    }
}

#[test]
fn skill_snapshots_cannot_recursively_copy_their_own_storage_directory() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("skill");
    skill(&root, "review");
    let mut cfg = options(temp.path());
    cfg.skills.directories.push(root.clone());
    cfg.storage.directory = root.join("data");
    let discovered = SkillSet::discover(&cfg).unwrap();
    assert!(discovered.snapshot(&cfg.storage.directory).is_err());
}
