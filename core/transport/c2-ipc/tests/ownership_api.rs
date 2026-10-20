//! R02 unique-write-ownership compile gate: each fixture under
//! `tests/fixtures/ownership/` becomes the source of one external consumer
//! crate (real `c2-mem`/`c2-ipc`/`c2-server`) and is compiled, never
//! executed. `Compiles` is the legal owner round-trip and proves the
//! consumer's dependencies resolve, so a rejection can never hide behind a
//! missing crate or unresolved import. `Fails(&codes)` requires rejection
//! with only the listed rustc error codes: the controls are rejected by
//! today's borrow/move rules, while each `vuln_*` names the codes the
//! approved R02 migration is expected to produce; on the unmodified
//! baseline the `vuln_*` fixtures still compile, so the test stays red
//! until the migration lands.

use std::path::{Path, PathBuf};
use std::process::Command;

enum Probe {
    Compiles,
    Fails(&'static [&'static str]),
}

const CASES: &[(&str, Probe)] = &[
    ("positive_owner_roundtrip", Probe::Compiles),
    (
        "control_same_handle_double_mut_borrow",
        Probe::Fails(&["E0499"]),
    ),
    (
        "control_move_handle_during_borrow",
        Probe::Fails(&["E0505", "E0502"]),
    ),
    (
        "vuln_same_pool_duplicate_handle_double_write",
        Probe::Fails(&["E0616", "E0451"]),
    ),
    (
        "vuln_same_pool_duplicate_handle_read_write",
        Probe::Fails(&["E0616", "E0451"]),
    ),
    (
        "vuln_second_mapping_double_write",
        Probe::Fails(&["E0616", "E0451"]),
    ),
    (
        "vuln_shared_allocator_free_during_borrow",
        Probe::Fails(&["E0133"]),
    ),
    (
        "vuln_peer_free_at_during_owner_borrow",
        Probe::Fails(&["E0133"]),
    ),
    (
        "vuln_raw_coordinate_copy_during_borrow",
        Probe::Fails(&["E0133"]),
    ),
    (
        "vuln_request_data_shm_bare_coordinates",
        Probe::Fails(&["E0624"]),
    ),
    (
        "vuln_response_data_shm_bare_coordinates",
        Probe::Fails(&["E0624"]),
    ),
    (
        "vuln_response_meta_shm_alloc_publication",
        Probe::Fails(&["E0624"]),
    ),
    (
        "vuln_connection_server_pool_raw_access",
        Probe::Fails(&["E0624", "E0616"]),
    ),
    (
        "vuln_client_pool_slot_substitution",
        Probe::Fails(&["E0599"]),
    ),
    (
        "vuln_connection_pool_replacement",
        Probe::Fails(&["E0599", "E0624"]),
    ),
    (
        "vuln_dedicated_rebind_after_destroy",
        Probe::Fails(&["E0133"]),
    ),
    (
        "vuln_injected_pool_destroy_during_fill",
        Probe::Fails(&["E0308"]),
    ),
    (
        "vuln_prealloc_published_span_expansion",
        Probe::Fails(&["E0061"]),
    ),
];

struct Temp(PathBuf);

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn r02_unique_write_ownership_boundary_gate() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("repository root")
        .to_path_buf();
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let session = Temp(std::env::temp_dir().join(format!(
        "c2-r02-ownership-gate-{}-{unique}",
        std::process::id()
    )));
    let consumer = session.0.join("consumer");
    std::fs::create_dir_all(consumer.join("src")).expect("consumer directories");
    std::fs::write(
        consumer.join("Cargo.toml"),
        format!(
            "[package]\nname = \"r02-ownership-probe\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\
             publish = false\n\n[dependencies]\nc2-mem = {{ path = {:?} }}\n\
             c2-ipc = {{ path = {:?} }}\nc2-server = {{ path = {:?} }}\nparking_lot = \"0.12\"\n\n\
             [profile.dev]\ndebug = 0\nincremental = false\n",
            repository.join("core/foundation/c2-mem"),
            repository.join("core/transport/c2-ipc"),
            repository.join("core/transport/c2-server"),
        ),
    )
    .expect("consumer manifest");
    // Seed locked versions from the tested Core workspace so --offline only
    // has to add the temporary root package.
    std::fs::copy(
        repository.join("core/Cargo.lock"),
        consumer.join("Cargo.lock"),
    )
    .expect("seed lock");

    let mut failures = Vec::new();
    for (index, (name, expectation)) in CASES.iter().enumerate() {
        let source = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/ownership")
                .join(format!("{name}.rs")),
        )
        .unwrap_or_else(|error| panic!("missing fixture {name}: {error}"));
        std::fs::write(consumer.join("src/main.rs"), source).expect("probe source");
        let mut arguments = vec!["check", "--quiet", "--offline"];
        // The first case (positive control) settles the consumer lock; the
        // rest run against the frozen lock.
        if index > 0 {
            arguments.push("--locked");
        }
        let output = Command::new(env!("CARGO"))
            .args(&arguments)
            .current_dir(&consumer)
            .env("CARGO_TARGET_DIR", session.0.join("target"))
            .env("CARGO_INCREMENTAL", "0")
            .env("CARGO_TERM_COLOR", "never")
            .output()
            .expect("nested cargo check");
        let stderr = String::from_utf8_lossy(&output.stderr);
        let ok = match expectation {
            Probe::Compiles => output.status.success(),
            Probe::Fails(codes) => {
                let errors: Vec<_> = stderr
                    .lines()
                    .filter_map(|line| line.strip_prefix("error[")?.split_once(']'))
                    .map(|(code, _)| code)
                    .collect();
                !output.status.success()
                    && !errors.is_empty()
                    && errors.iter().all(|code| codes.contains(code))
            }
        };
        let exit = output.status.code().unwrap_or(-1);
        println!("[r02-gate] {name}: exit={exit} ok={ok}");
        if !ok {
            let errors: Vec<&str> = stderr
                .lines()
                .filter(|line| line.contains("error"))
                .take(4)
                .collect();
            println!("[r02-gate] {name} diagnostics: {}", errors.join("\n"));
            failures.push(*name);
        }
    }
    std::fs::remove_dir_all(&session.0).expect("remove ownership probe build artifacts");
    assert!(
        failures.is_empty(),
        "R02 ownership gate failures: {failures:?}\n\
         On the unmodified baseline the vuln_* probes still compile, which is the\n\
         recorded red baseline; the controls must already fail with their listed\n\
         rustc error codes, and the positive round-trip must always compile."
    );
}
