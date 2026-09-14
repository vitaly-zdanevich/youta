//! Fixed offline inputs and raw samples for the opt-in release baseline probes.

use std::hint::black_box;
use std::time::Instant;

use serde::Deserialize;

/// Versioned fixture recipe; process-only parameters are read by the Python runner.
#[derive(Deserialize)]
pub(crate) struct Fixture {
    pub(crate) rows: usize,
    pub(crate) description_paragraphs: usize,
    pub(crate) samples: usize,
    pub(crate) warmup_iterations: usize,
    pub(crate) navigation_iterations: usize,
    pub(crate) render_iterations: usize,
    pub(crate) terminal_columns: u16,
    pub(crate) terminal_rows: u16,
}

impl Fixture {
    /// Loads the checked-in recipe rather than selecting a machine-dependent workload.
    pub(crate) fn load() -> Self {
        serde_json::from_str(include_str!("../../tests/fixtures/performance.json"))
            .expect("performance fixture recipe")
    }

    /// Generates repeatable Unicode, paragraph, timestamp, and link wrapping inputs.
    pub(crate) fn description(&self) -> String {
        (0..self.description_paragraphs)
            .map(|index| {
                format!(
                    "Paragraph {index:04}: Offline café 音楽 fixture with a long description. \
                     Read https://example.invalid/fixture/{index:04} and #offline notes.\n\
                     01:23 A deterministic chapter heading\n\n"
                )
            })
            .collect()
    }

    /// Warms the operation, then records amortized wall time without speed assertions.
    pub(crate) fn measure(&self, iterations: usize, mut operation: impl FnMut()) -> Vec<f64> {
        for _ in 0..self.warmup_iterations {
            operation();
        }
        (0..self.samples)
            .map(|_| {
                let started = Instant::now();
                for _ in 0..iterations {
                    operation();
                }
                black_box(started.elapsed().as_secs_f64() / iterations as f64)
            })
            .collect()
    }
}

/// Saves raw measurements only when the ignored probe was explicitly requested.
pub(crate) fn write_probe(
    name: &str,
    scope: &str,
    iterations: usize,
    description_bytes: usize,
    samples: Vec<f64>,
) {
    let directory = std::env::var_os("YOUTA_PERFORMANCE_PROBE_DIR")
        .expect("set YOUTA_PERFORMANCE_PROBE_DIR when running ignored performance probes");
    let directory = std::path::PathBuf::from(directory);
    std::fs::create_dir_all(&directory).expect("performance probe directory");
    // The diagnostic list omits the structural controller feature; include it
    // here so the baseline verifies the complete reduced profile explicitly.
    let mut compiled_features = crate::diagnostics::enabled_compile_features();
    if cfg!(feature = "controller") {
        compiled_features.push("controller");
        compiled_features.sort_unstable();
        compiled_features.dedup();
    }
    let document = serde_json::json!({
        "scope": scope,
        "iterations_per_sample": iterations,
        "samples_seconds_per_operation": samples,
        "compiled_features": compiled_features,
        "debug_assertions": cfg!(debug_assertions),
        "description_utf8_bytes": description_bytes,
        "build_revision": crate::build_info::current_build_sha(),
        "fixture_definition": serde_json::from_str::<serde_json::Value>(
            include_str!("../../tests/fixtures/performance.json")
        ).expect("fixture definition"),
        // The runner verifies these compile-time inputs against the checkout,
        // then removes the text before publishing the compact public report.
        "source_definitions": {
            "src/app/tests/performance.rs": include_str!("../app/tests/performance.rs"),
            "src/tui/performance.rs": include_str!("../tui/performance.rs"),
            "src/test_support/performance.rs": include_str!("performance.rs"),
        },
    });
    std::fs::write(
        directory.join(format!("{name}.json")),
        serde_json::to_vec_pretty(&document).expect("serializable probe"),
    )
    .expect("write performance probe");
}
