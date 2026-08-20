//! 仓储层集成测试：分子/任务全生命周期 + 幂等 + 过滤（设计文档 §3.5）。

use xtbp_core::job::{Job, JobParams, JobStatus};
use xtbp_core::molecule::{Charge, Molecule, Multiplicity};
use xtbp_core::result::{Broadening, ScalarResult, Spectrum, Transition};
use xtbp_core::time::now_unix;
use xtbp_store::{JobFilter, Store};

async fn fresh_store() -> (Store, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("test.db");
    let store = Store::open(&db).await.unwrap();
    (store, dir)
}

fn benzene() -> Molecule {
    Molecule::new("C1=CC=CC=C1", Charge(0), Multiplicity(1), now_unix())
}

#[tokio::test]
async fn molecule_ensure_dedups_by_smiles() {
    let (store, _dir) = fresh_store().await;
    let m1 = store.ensure_molecule(&benzene()).await.unwrap();
    let m2 = store.ensure_molecule(&benzene()).await.unwrap();
    assert_eq!(m1.id, m2.id);
    assert_eq!(store.list_molecules(10).await.unwrap().len(), 1);
}

#[tokio::test]
async fn molecule_dedups_by_inchikey_after_fill() {
    let (store, _dir) = fresh_store().await;
    let mut m = benzene();
    m.inchikey = "UHOVQNZJYSORNB-UHFFFAOYSA-N".into();
    let kept = store.ensure_molecule(&m).await.unwrap();
    store.set_inchikey(&kept.id, &m.inchikey).await.unwrap();
    let again = store.ensure_molecule(&m).await.unwrap();
    assert_eq!(kept.id, again.id);
}

#[tokio::test]
async fn job_lifecycle_persists_every_state() {
    let (store, _dir) = fresh_store().await;
    let mol = store.ensure_molecule(&benzene()).await.unwrap();
    let mut job = Job::new(
        mol.id,
        "opt",
        JobParams::default(),
        "hash-1".into(),
        None,
        0,
    );
    job.transition(JobStatus::Queued).unwrap();
    store.insert_job(&job).await.unwrap();
    job.transition(JobStatus::Running).unwrap();
    store.update_job(&job).await.unwrap();
    job.transition(JobStatus::Parsing).unwrap();
    store.update_job(&job).await.unwrap();
    job.transition(JobStatus::Done).unwrap();
    store.update_job(&job).await.unwrap();

    let loaded = store.get_job(&job.id).await.unwrap().unwrap();
    assert_eq!(loaded.status, JobStatus::Done);
    assert!(loaded.started_at.is_some());
    assert!(loaded.finished_at.is_some());
    assert_eq!(
        loaded.params.method.family,
        xtbp_core::MethodFamily::Gfn2Xtb
    );
}

#[tokio::test]
async fn idempotent_submit_reuses_done_job() {
    let (store, _dir) = fresh_store().await;
    let mol = store.ensure_molecule(&benzene()).await.unwrap();
    let mut job = Job::new(
        mol.id,
        "opt",
        JobParams::default(),
        "hash-42".into(),
        None,
        0,
    );
    store.insert_job(&job).await.unwrap();
    // 未完成 → 不命中
    assert!(
        store
            .find_done_by_content_hash("hash-42")
            .await
            .unwrap()
            .is_none()
    );
    job.transition(JobStatus::Queued).unwrap();
    job.transition(JobStatus::Running).unwrap();
    job.transition(JobStatus::Parsing).unwrap();
    job.transition(JobStatus::Done).unwrap();
    store.update_job(&job).await.unwrap();
    // 完成 → 命中（重试不产生重复计算）
    let hit = store.find_done_by_content_hash("hash-42").await.unwrap();
    assert_eq!(hit.unwrap().id, job.id);
}

#[tokio::test]
async fn results_spectra_artifacts_roundtrip() {
    let (store, _dir) = fresh_store().await;
    let mol = store.ensure_molecule(&benzene()).await.unwrap();
    let job = Job::new(mol.id, "excited", JobParams::default(), "h".into(), None, 0);
    store.insert_job(&job).await.unwrap();

    store
        .put_result(
            &job.id,
            &ScalarResult {
                key: "total_energy".into(),
                value: -15.8796,
                unit: "Eh".into(),
                tier: "screening".into(),
            },
        )
        .await
        .unwrap();
    // 覆盖写
    store
        .put_result(
            &job.id,
            &ScalarResult {
                key: "total_energy".into(),
                value: -15.8800,
                unit: "Eh".into(),
                tier: "screening".into(),
            },
        )
        .await
        .unwrap();
    let results = store.results_for_job(&job.id).await.unwrap();
    assert_eq!(results.len(), 1);
    assert!((results[0].value + 15.8800).abs() < 1e-6);

    let spectrum = Spectrum::broaden(
        &[Transition {
            state: 1,
            energy_ev: 4.0,
            wavelength_nm: 300.0,
            oscillator_strength: 0.5,
            assignment: None,
        }],
        Broadening::Gaussian { sigma_ev: 0.4 },
    );
    store
        .put_spectrum(&job.id, "stda-gaussian-0.4ev", &spectrum)
        .await
        .unwrap();
    let loaded = store
        .spectrum_for_job(&job.id, "stda-gaussian-0.4ev")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(loaded.transitions.len(), 1);

    store
        .put_artifact(&job.id, "xyz-opt", "output/xtbopt.xyz", "abc123")
        .await
        .unwrap();
    let arts = store.artifacts_for_job(&job.id).await.unwrap();
    assert_eq!(arts.len(), 1);
    assert_eq!(arts[0].0, "xyz-opt");
}

#[tokio::test]
async fn job_filters_and_children() {
    let (store, _dir) = fresh_store().await;
    let mol = store.ensure_molecule(&benzene()).await.unwrap();
    let parent = Job::new(mol.id, "opt", JobParams::default(), "p".into(), None, 0);
    store.insert_job(&parent).await.unwrap();
    let child = Job::new(
        mol.id,
        "excited",
        JobParams::default(),
        "c".into(),
        Some(parent.id),
        0,
    );
    store.insert_job(&child).await.unwrap();

    assert_eq!(store.children_of(&parent.id).await.unwrap().len(), 1);
    let all = store.list_jobs(&JobFilter::all()).await.unwrap();
    assert_eq!(all.len(), 2);
    let by_wf = store
        .list_jobs(&JobFilter {
            workflow: Some("excited".into()),
            ..JobFilter::all()
        })
        .await
        .unwrap();
    assert_eq!(by_wf.len(), 1);
    assert_eq!(store.list_non_terminal_jobs().await.unwrap().len(), 2);
}

#[tokio::test]
async fn cancel_terminal_noop_and_cancel_active_works() {
    let (store, _dir) = fresh_store().await;
    let mol = store.ensure_molecule(&benzene()).await.unwrap();
    let job = Job::new(mol.id, "opt", JobParams::default(), "x".into(), None, 0);
    store.insert_job(&job).await.unwrap();
    let cancelled = store.try_cancel(&job.id).await.unwrap().unwrap();
    assert_eq!(cancelled.status, JobStatus::Cancelled);
    // 再次取消：终态无变化
    let again = store.try_cancel(&job.id).await.unwrap().unwrap();
    assert_eq!(again.status, JobStatus::Cancelled);
    // 非法状态转移被拒
    let mut bad = job;
    bad.status = JobStatus::Done;
    assert!(bad.transition(JobStatus::Queued).is_err());
}
