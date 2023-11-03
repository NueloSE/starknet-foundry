use crate::corelib::{corelib_path, predeployed_contracts};
use crate::runner::TestCase;
use camino::Utf8PathBuf;
use forge::run;
use forge::test_filter::TestsFilter;
use forge_runner::test_crate_summary::TestCrateSummary;
use forge_runner::{CancellationTokens, RunnerConfig, RunnerParams};
use std::path::PathBuf;
use tempfile::tempdir;
use tokio::runtime::Runtime;

#[must_use]
pub fn run_test_case(test: &TestCase) -> Vec<TestCrateSummary> {
    let rt = Runtime::new().expect("Could not instantiate Runtime");

    rt.block_on(run(
        &test.path().unwrap(),
        &String::from("src"),
        &test.path().unwrap().join("src"),
        &TestsFilter::from_flags(None, false, false, false),
        RunnerConfig::new(
            Utf8PathBuf::from_path_buf(PathBuf::from(tempdir().unwrap().path())).unwrap(),
            false,
            vec![],
            256,
            12345,
        ),
        RunnerParams::new(
            corelib_path(),
            test.contracts(&corelib_path()).unwrap(),
            Utf8PathBuf::from_path_buf(predeployed_contracts().to_path_buf()).unwrap(),
            test.env().clone(),
            test.linked_libraries(),
        ),
        CancellationTokens::new(),
    ))
    .expect("Runner fail")
}
