//! The app state builds over each engine the app can start with (`lib.rs` engine selection).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use opensvn_ipc::AppError;

use super::fixture::state_with_engine;
use crate::app_state::EngineSource;
use crate::engine::{EngineFacade, FakeEngine};

#[tokio::test]
async fn state_builds_over_a_ready_fake_engine() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Arc::new(FakeEngine::demo(Duration::ZERO).await.unwrap());
    let state = state_with_engine(dir.path(), EngineSource::Ready(engine)).unwrap();
    assert!(state.wcs.list().await.unwrap().is_empty());
}

#[tokio::test]
async fn engine_builder_receives_the_app_services() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Arc::new(FakeEngine::demo(Duration::ZERO).await.unwrap());
    let seen = Arc::new(Mutex::new(None));
    let source = {
        let seen = Arc::clone(&seen);
        EngineSource::Build(Box::new(move |services| {
            *seen.lock().unwrap() = Some((
                services.data_dir.clone(),
                services.settings.get().wc_read_subversion_config,
            ));
            let engine: Arc<dyn EngineFacade> = engine;
            Ok(engine)
        }))
    };
    state_with_engine(dir.path(), source).unwrap();
    let (data_dir, read_config) = seen.lock().unwrap().take().expect("builder ran");
    assert_eq!(data_dir, dir.path());
    assert!(read_config, "default of wc.readSubversionConfig");
}

#[tokio::test]
async fn a_failing_engine_builder_fails_the_start() {
    let dir = tempfile::tempdir().unwrap();
    let source = EngineSource::Build(Box::new(|_| {
        Err::<Arc<dyn EngineFacade>, _>(crate::errors::invalid_argument("no engine"))
    }));
    let err: AppError = state_with_engine(dir.path(), source).err().expect("error");
    assert_eq!(err.message, "no engine");
}

#[cfg(feature = "real-engine")]
mod real {
    use std::sync::Arc;
    use std::time::Duration;

    use opensvn_ipc::SettingsPatch;

    use super::state_with_engine;
    use crate::app_state::{
        ENGINE_STATE_SUBDIR, EngineServices, build_real_engine, real_engine_source,
    };
    use crate::auth::AuthBroker;
    use crate::db::AppDb;
    use crate::engine::real::default_subversion_config_dir;
    use crate::ops::events::RecordingEvents;
    use crate::settings::SettingsService;

    #[tokio::test]
    async fn state_builds_over_the_real_engine() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_engine(dir.path(), real_engine_source()).unwrap();
        assert!(dir.path().join(ENGINE_STATE_SUBDIR).is_dir());
        assert!(state.wcs.list().await.unwrap().is_empty());
        // Nothing of the fake engine's demo data: no working copy is registered and the demo
        // folder is not a working copy for this engine.
        let demo = crate::engine::fake::demo_wc_root();
        assert!(
            state
                .wcs
                .add_existing(&demo.to_string_lossy())
                .await
                .is_err()
        );
        assert!(state.wcs.list().await.unwrap().is_empty());
    }

    fn services(dir: &std::path::Path) -> EngineServices {
        let db = AppDb::open(dir).unwrap();
        EngineServices {
            auth: Arc::new(AuthBroker::new(
                Arc::new(RecordingEvents::default()),
                db.clone(),
                Arc::new(secure_store::MemoryStore::new()),
            )),
            settings: Arc::new(SettingsService::load(db.clone(), None).unwrap()),
            db,
            data_dir: dir.to_path_buf(),
        }
    }

    fn set_read_config(services: &EngineServices, on: bool) {
        let patch: SettingsPatch =
            serde_json::from_value(serde_json::json!({ "wc.readSubversionConfig": on })).unwrap();
        services.settings.apply_patch(&patch).unwrap();
    }

    async fn wait_for(
        engine: &crate::engine::real::RealEngine,
        expected: Option<std::path::PathBuf>,
    ) {
        for _ in 0..400 {
            if engine.subversion_config_dir() == expected {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(engine.subversion_config_dir(), expected);
    }

    #[tokio::test]
    async fn subversion_config_dir_follows_the_setting() {
        let dir = tempfile::tempdir().unwrap();
        let services = services(dir.path());
        assert!(
            default_subversion_config_dir().is_some(),
            "HOME/APPDATA set"
        );

        // On by default: read from svn's per-user directory from the start.
        let engine = build_real_engine(&services).unwrap();
        assert_eq!(
            engine.subversion_config_dir(),
            default_subversion_config_dir()
        );

        set_read_config(&services, false);
        wait_for(&engine, None).await;
        set_read_config(&services, true);
        wait_for(&engine, default_subversion_config_dir()).await;
    }

    #[tokio::test]
    async fn setting_off_at_start_uses_built_in_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let services = services(dir.path());
        set_read_config(&services, false);
        let engine = build_real_engine(&services).unwrap();
        assert_eq!(engine.subversion_config_dir(), None);
    }

    #[tokio::test]
    async fn global_ignores_setting_replaces_the_configured_list_while_not_empty() {
        let dir = tempfile::tempdir().unwrap();
        let services = services(dir.path());
        set_read_config(&services, false);
        let engine = build_real_engine(&services).unwrap();
        let built_in = svn_client::local::AddConfig::default().global_ignores;
        assert_eq!(engine.effective_global_ignores(), built_in);

        let set = |v: &str| {
            let patch: SettingsPatch =
                serde_json::from_value(serde_json::json!({ "wc.globalIgnores": v })).unwrap();
            services.settings.apply_patch(&patch).unwrap();
        };
        let wait = |expected: Vec<String>| {
            let engine = engine.clone();
            async move {
                for _ in 0..400 {
                    if engine.effective_global_ignores() == expected {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                assert_eq!(engine.effective_global_ignores(), expected);
            }
        };
        set("*.tmp\n  build ");
        wait(vec!["*.tmp".into(), "build".into()]).await;
        // Blank again: back to the configured (here built-in) list.
        set("  ");
        wait(built_in).await;
    }
}
