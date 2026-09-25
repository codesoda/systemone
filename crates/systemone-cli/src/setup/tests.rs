use std::{collections::BTreeSet, fs, path::PathBuf};

use super::{prompt::Answer::*, prompt::ScriptedPrompter, *};
use crate::args::SetupArgs;

struct FakeServices {
    set_env: BTreeSet<String>,
    space: Option<u64>,
    models: Vec<ModelStatus>,
    downloads: Vec<(String, PathBuf)>,
    pulls: Vec<String>,
    tests: Vec<String>,
    fail_test: bool,
}

impl Default for FakeServices {
    fn default() -> Self {
        Self {
            set_env: BTreeSet::new(),
            space: Some(1 << 40),
            models: vec![
                model("qwen3-0.6b", 639_000_000, false),
                model("qwen3.5-4b", 1_800_000_000, true),
            ],
            downloads: Vec::new(),
            pulls: Vec::new(),
            tests: Vec::new(),
            fail_test: false,
        }
    }
}

fn model(id: &str, bytes: u64, cached: bool) -> ModelStatus {
    ModelStatus {
        id: id.to_owned(),
        source: "hf".to_owned(),
        revision: "0".repeat(40),
        file: format!("{id}.gguf"),
        bytes,
        sha256: "a".repeat(64),
        cached,
        verified: cached,
        cache_status: String::new(),
        path: None,
        detail: None,
    }
}

impl Services for FakeServices {
    fn env_is_set(&self, name: &str) -> bool {
        self.set_env.contains(name)
    }
    fn available_space(&self, _directory: &Path) -> Option<u64> {
        self.space
    }
    fn openjev_models(&mut self, _device: &str) -> Result<Vec<ModelStatus>, CliError> {
        Ok(self.models.clone())
    }
    fn pull_openjev(&mut self, _device: &str, model: &ModelStatus) -> Result<(), CliError> {
        self.pulls.push(model.id.clone());
        Ok(())
    }
    fn download(&mut self, plan: &DownloadPlan, directory: &Path) -> Result<(), CliError> {
        self.downloads
            .push((plan.label.clone(), directory.to_path_buf()));
        Ok(())
    }
    fn test_decision(&mut self, _config: &Config, backend: &str) -> Result<TestResult, CliError> {
        self.tests.push(backend.to_owned());
        if self.fail_test {
            return Err(CliError::runtime("load", "weights missing"));
        }
        Ok(TestResult {
            model: "m".to_owned(),
            probability_true: 0.9,
            seconds: 0.1,
        })
    }
}

struct Fixture {
    _root: tempfile::TempDir,
    home: PathBuf,
    context: Context,
}

impl Fixture {
    fn new(build: Build) -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let project = root.path().join("project");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&project).unwrap();
        let context = Context {
            user_file: Some(home.join(".systemone").join(systemone_config::FILE_NAME)),
            project_file: project.join(systemone_config::FILE_NAME),
            home: Some(home.clone()),
            environment: Some(Vec::new()),
            build,
        };
        Self {
            _root: root,
            home,
            context,
        }
    }

    fn user_file(&self) -> PathBuf {
        self.context.user_file.clone().unwrap()
    }

    fn run(
        &self,
        args: &SetupArgs,
        answers: &[prompt::Answer],
        services: &mut FakeServices,
    ) -> (Result<Summary, CliError>, ScriptedPrompter) {
        let mut prompter = ScriptedPrompter::new(answers);
        let result = run(args, &self.context, &mut prompter, services);
        (result, prompter)
    }
}

fn everything() -> Build {
    Build {
        openjev: vec!["metal", "cpu"],
        laya: vec!["cpu"],
        kev: vec!["cpu"],
        gliner2: true,
    }
}

fn hosted_only() -> Build {
    Build {
        openjev: Vec::new(),
        laya: Vec::new(),
        kev: Vec::new(),
        gliner2: false,
    }
}

#[test]
fn typesafe_stores_only_the_variable_name_and_explains_an_unset_key() {
    let fixture = Fixture::new(hosted_only());
    let mut services = FakeServices::default();
    let (result, prompter) = fixture.run(
        &SetupArgs::default(),
        &[
            Pick("User config"),
            Pick("Add"),
            Pick("typesafe"),
            Accept,            // name
            Text("MY_TS_KEY"), // env var name
            Yes,               // default backend
            Yes,               // write
        ],
        &mut services,
    );
    let summary = result.unwrap();
    assert_eq!(prompter.remaining(), 0);
    assert_eq!(summary.backend, "typesafe");
    assert!(summary.written && summary.default);
    assert_eq!(summary.download, "not_needed");
    assert_eq!(summary.test, "skipped");
    let text = fs::read_to_string(fixture.user_file()).unwrap();
    assert_eq!(
        text,
        "default_backend = \"typesafe\"\n\n[backends.typesafe]\nkind = \"typesafe\"\nenabled = true\n\n[backends.typesafe.settings]\napi_key_env = \"MY_TS_KEY\"\n"
    );
    assert!(
        prompter.shown().contains("MY_TS_KEY is not set"),
        "{}",
        prompter.shown()
    );
    assert!(prompter.shown().contains("never the key"));
}

#[test]
fn hosted_test_decision_warns_about_billing_and_defaults_to_no() {
    let fixture = Fixture::new(hosted_only());
    let mut services = FakeServices::default();
    services.set_env.insert("TYPESAFE_API_KEY".to_owned());
    let (result, prompter) = fixture.run(
        &SetupArgs {
            user: true,
            kind: Some("typesafe".to_owned()),
            ..SetupArgs::default()
        },
        &[Accept, Accept, Yes, Yes, Accept],
        &mut services,
    );
    assert_eq!(result.unwrap().test, "declined");
    assert!(prompter.shown().contains("billed"), "{}", prompter.shown());
    assert!(prompter.shown().contains("TYPESAFE_API_KEY is set"));
    assert!(services.tests.is_empty());
}

#[test]
fn invalid_env_names_are_asked_again() {
    let fixture = Fixture::new(hosted_only());
    let (result, prompter) = fixture.run(
        &SetupArgs {
            user: true,
            kind: Some("typesafe".to_owned()),
            ..SetupArgs::default()
        },
        &[Accept, Text("sk-live=abc"), Text("KEY_2"), No, Yes],
        &mut FakeServices::default(),
    );
    result.unwrap();
    assert!(
        prompter
            .shown()
            .contains("Use letters, digits and underscores")
    );
    let text = fs::read_to_string(fixture.user_file()).unwrap();
    assert!(text.contains("api_key_env = \"KEY_2\""));
    assert!(!text.contains("sk-live"));
}

#[test]
fn kinds_this_build_cannot_run_are_explained_and_asked_again() {
    let fixture = Fixture::new(hosted_only());
    let (result, prompter) = fixture.run(
        &SetupArgs {
            user: true,
            ..SetupArgs::default()
        },
        &[
            Pick("Add"),
            Pick("laya"),
            Pick("vercel"),
            Pick("typesafe"),
            Accept,
            Accept,
            Yes,
            Yes,
        ],
        &mut FakeServices::default(),
    );
    result.unwrap();
    let shown = prompter.shown();
    assert!(
        shown.contains(
            "laya is not available: needs a build with --features laya-cpu or laya-metal"
        ),
        "{shown}"
    );
    assert!(
        shown.contains("vercel is not available: planned"),
        "{shown}"
    );
}

#[test]
fn a_kind_flag_for_an_unavailable_kind_is_a_validation_error() {
    let fixture = Fixture::new(hosted_only());
    let (result, _) = fixture.run(
        &SetupArgs {
            user: true,
            kind: Some("gliner2".to_owned()),
            ..SetupArgs::default()
        },
        &[],
        &mut FakeServices::default(),
    );
    let error = result.unwrap_err();
    assert_eq!(error.code(), "validation");
    assert!(error.to_string().contains("--features gliner2"), "{error}");
}

#[test]
fn taken_names_are_refused() {
    let fixture = Fixture::new(everything());
    let (result, prompter) = fixture.run(
        &SetupArgs {
            user: true,
            ..SetupArgs::default()
        },
        &[
            Pick("Add"),
            Pick("gliner2"),
            Text("local"),
            Text("Bad_Name"),
            Text("classifier"),
            Accept, // profile
            Accept, // model dir
            Yes,    // default
            No,     // write
        ],
        &mut FakeServices::default(),
    );
    let summary = result.unwrap();
    assert!(!summary.written);
    let shown = prompter.shown();
    assert!(
        shown.contains("A backend named local already exists"),
        "{shown}"
    );
    assert!(!fixture.user_file().exists(), "declining must not write");
}

#[test]
fn gliner2_downloads_into_the_default_directory() {
    let fixture = Fixture::new(everything());
    let mut services = FakeServices::default();
    let (result, prompter) = fixture.run(
        &SetupArgs {
            project: true,
            kind: Some("gliner2".to_owned()),
            ..SetupArgs::default()
        },
        &[
            Accept, // name local-gliner2
            Pick("small"),
            Accept, // model dir
            Yes,    // default
            Yes,    // write
            Yes,    // download
            No,     // test
        ],
        &mut services,
    );
    let summary = result.unwrap();
    assert_eq!(summary.backend, "local-gliner2");
    assert_eq!(summary.download, "downloaded");
    assert_eq!(summary.test, "declined");
    let expected = fixture
        .home
        .join(".systemone/models/gliner2/gliner2.5-small-v1");
    assert_eq!(
        services.downloads,
        [("gliner2.5-small-v1".to_owned(), expected)]
    );
    let text = fs::read_to_string(&fixture.context.project_file).unwrap();
    assert!(
        text.contains("model_dir = \"~/.systemone/models/gliner2/gliner2.5-small-v1\""),
        "{text}"
    );
    assert!(prompter.shown().contains("free"), "{}", prompter.shown());
}

#[test]
fn present_files_skip_the_download_and_offer_the_test() {
    let fixture = Fixture::new(everything());
    let directory = fixture.home.join("models/base");
    let plan = systemone_gliner2::download::download_plan("base").unwrap();
    for file in &plan.files {
        let path = directory.join(&file.path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Sparse files with the pinned sizes: the size check passes.
        fs::File::create(&path)
            .unwrap()
            .set_len(file.bytes)
            .unwrap();
    }
    let mut services = FakeServices::default();
    let (result, prompter) = fixture.run(
        &SetupArgs {
            user: true,
            kind: Some("gliner2".to_owned()),
            ..SetupArgs::default()
        },
        &[Accept, Accept, Text("~/models/base"), Yes, Yes, Yes],
        &mut services,
    );
    let summary = result.unwrap();
    assert_eq!(summary.download, "present");
    assert_eq!(summary.test, "passed");
    assert!(services.downloads.is_empty());
    assert_eq!(services.tests, ["local-gliner2"]);
    assert!(
        prompter.shown().contains("Test passed"),
        "{}",
        prompter.shown()
    );
}

#[test]
fn low_disk_space_defaults_to_not_downloading() {
    let fixture = Fixture::new(everything());
    let mut services = FakeServices {
        space: Some(10),
        ..FakeServices::default()
    };
    let (result, prompter) = fixture.run(
        &SetupArgs {
            user: true,
            kind: Some("kev".to_owned()),
            ..SetupArgs::default()
        },
        &[Accept, Accept, Accept, Accept, Yes, Accept],
        &mut services,
    );
    let summary = result.unwrap();
    assert_eq!(summary.download, "declined");
    assert!(services.downloads.is_empty());
    let shown = prompter.shown();
    assert!(shown.contains("not enough free space"), "{shown}");
    assert!(shown.contains("s1 setup --backend local-kev"), "{shown}");
    let text = fs::read_to_string(fixture.user_file()).unwrap();
    // Only the CPU checkpoint is offered on a CPU-only kev build.
    assert!(text.contains("model = \"kev-0.6b\""), "{text}");
    assert!(text.contains("device = \"cpu\""), "{text}");
}

#[test]
fn no_download_flag_skips_the_download() {
    let fixture = Fixture::new(everything());
    let mut services = FakeServices::default();
    let (result, _) = fixture.run(
        &SetupArgs {
            user: true,
            kind: Some("gliner2".to_owned()),
            no_download: true,
            ..SetupArgs::default()
        },
        &[Accept, Accept, Accept, Yes, Yes],
        &mut services,
    );
    assert_eq!(result.unwrap().download, "skipped");
    assert!(services.downloads.is_empty());
}

#[test]
fn editing_the_builtin_local_backend_writes_an_override_and_pulls_the_model() {
    let fixture = Fixture::new(everything());
    let project = &fixture.context.project_file;
    fs::write(
        project,
        "# project settings\n[server]\nport = 9000 # keep\n",
    )
    .unwrap();
    let mut services = FakeServices::default();
    let (result, prompter) = fixture.run(
        &SetupArgs {
            project: true,
            ..SetupArgs::default()
        },
        &[
            Pick("Edit local"),
            Pick("cpu"),
            Pick("qwen3-0.6b"),
            // Already the default backend: no question.
            Yes, // write
            Yes, // download
            No,  // test
        ],
        &mut services,
    );
    let summary = result.unwrap();
    assert!(summary.default);
    assert_eq!(summary.download, "downloaded");
    assert_eq!(services.pulls, ["qwen3-0.6b"]);
    let text = fs::read_to_string(project).unwrap();
    assert!(
        text.starts_with("# project settings\n[server]\nport = 9000 # keep\n"),
        "{text}"
    );
    assert!(
        text.contains(
            "[backends.local]\nkind = \"openjev\"\nenabled = true\nmodel = \"qwen3-0.6b\""
        ),
        "{text}"
    );
    assert!(summary.backup.is_some());
    assert!(
        prompter.shown().contains("+ [backends.local]"),
        "{}",
        prompter.shown()
    );
}

#[test]
fn a_cached_openjev_model_is_not_downloaded_again() {
    let fixture = Fixture::new(everything());
    let mut services = FakeServices::default();
    let (result, _) = fixture.run(
        &SetupArgs {
            user: true,
            backend: Some("local".to_owned()),
            ..SetupArgs::default()
        },
        &[Pick("metal"), Pick("qwen3.5-4b"), Yes, No],
        &mut services,
    );
    let summary = result.unwrap();
    assert_eq!(summary.download, "present");
    assert!(services.pulls.is_empty());
}

#[test]
fn a_config_that_would_fail_validation_is_never_written() {
    let fixture = Fixture::new(everything());
    let original = "[backends.broken]\nkind = \"laya\"\nenabled = true\n\n[backends.broken.settings]\nprofile = \"klingon\"\n";
    let user = fixture.user_file();
    fs::create_dir_all(user.parent().unwrap()).unwrap();
    fs::write(&user, original).unwrap();
    let (result, _) = fixture.run(
        &SetupArgs {
            user: true,
            kind: Some("typesafe".to_owned()),
            ..SetupArgs::default()
        },
        &[Accept, Accept, Yes],
        &mut FakeServices::default(),
    );
    let error = result.unwrap_err();
    assert!(error.to_string().contains("nothing was written"), "{error}");
    assert!(error.to_string().contains("klingon"), "{error}");
    assert_eq!(fs::read_to_string(&user).unwrap(), original);
}

#[test]
fn a_failed_test_is_reported_in_the_summary() {
    let fixture = Fixture::new(everything());
    let mut services = FakeServices {
        fail_test: true,
        models: vec![model("qwen3-0.6b", 1, true)],
        ..FakeServices::default()
    };
    let (result, prompter) = fixture.run(
        &SetupArgs {
            user: true,
            backend: Some("local".to_owned()),
            ..SetupArgs::default()
        },
        &[Accept, Accept, Yes, Yes],
        &mut services,
    );
    assert_eq!(result.unwrap().test, "failed");
    assert!(prompter.shown().contains("Test failed: weights missing"));
}

#[test]
fn without_a_home_directory_setup_uses_the_project_file() {
    let mut fixture = Fixture::new(hosted_only());
    fixture.context.user_file = None;
    fixture.context.home = None;
    let (result, prompter) = fixture.run(
        &SetupArgs {
            kind: Some("typesafe".to_owned()),
            ..SetupArgs::default()
        },
        &[Accept, Accept, Yes, Yes],
        &mut FakeServices::default(),
    );
    assert_eq!(
        result.unwrap().path,
        fixture.context.project_file.display().to_string()
    );
    assert!(prompter.shown().contains("No home directory"));
}

#[test]
fn defaults_prompter_runs_setup_without_questions() {
    let fixture = Fixture::new(hosted_only());
    let mut transcript = Vec::new();
    let summary = run(
        &SetupArgs {
            yes: true,
            ..SetupArgs::default()
        },
        &fixture.context,
        &mut prompt::DefaultsPrompter::new(&mut transcript),
        &mut FakeServices::default(),
    )
    .unwrap();
    assert!(summary.written);
    assert_eq!(summary.kind, "typesafe");
    assert_eq!(summary.test, "skipped");
    assert!(fixture.user_file().exists());
    let transcript = String::from_utf8(transcript).unwrap();
    assert!(
        transcript.contains("Which config file should setup write?"),
        "{transcript}"
    );
}

#[test]
fn name_suggestions_avoid_existing_backends() {
    let known = vec!["local".to_owned(), "local-laya".to_owned()];
    assert_eq!(suggest_name(ProviderKind::OpenJev, &known), "local-openjev");
    assert_eq!(suggest_name(ProviderKind::Laya, &known), "local-laya-2");
    assert_eq!(suggest_name(ProviderKind::Typesafe, &known), "typesafe");
}

#[test]
fn setup_without_a_terminal_needs_yes() {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let code = crate::run_with_io(
        ["s1", "setup"],
        &mut std::io::empty(),
        false,
        &mut stdout,
        &mut stderr,
    );
    assert_eq!(code, 2);
    assert!(stdout.is_empty());
    let stderr = String::from_utf8(stderr).unwrap();
    assert!(stderr.contains("needs a terminal"), "{stderr}");
}
