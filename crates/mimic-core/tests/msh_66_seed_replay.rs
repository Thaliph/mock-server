//! MSH-66: the seeds of the fuzz target `mesh_head` (`fuzz/fuzz_targets/mesh_head.rs`) run
//! through the head reader on a stable toolchain, whole and cut into small reads, with and
//! without `Upgrade` heads leaving. The checks mirror the target; keep both in step.

use std::path::Path;

use mimic_core::mesh::head::{Reader, Stop};

struct Outcome {
    stop: Option<Stop>,
    pass: Vec<u8>,
    held: Vec<u8>,
    pending: usize,
}

fn run(input: &[u8], upgrades: bool, mut seed: u8, cut: bool) -> Outcome {
    let mut reader = Reader::new();
    let mut pass = Vec::new();
    let mut stop = None;
    let mut rest = input;
    while !rest.is_empty() {
        let size = if cut {
            seed = seed.wrapping_mul(97).wrapping_add(13);
            usize::from(seed % 23) + 1
        } else {
            rest.len()
        };
        let (chunk, tail) = rest.split_at(size.min(rest.len()));
        rest = tail;
        stop = stop.or(reader.feed(chunk, upgrades, &mut pass));
    }
    Outcome {
        stop,
        pass,
        held: reader.held().to_vec(),
        pending: reader.pending(),
    }
}

fn check(name: &str, input: &[u8], upgrades: bool, seed: u8) {
    let whole = run(input, upgrades, seed, false);
    assert!(
        input.starts_with(&whole.pass),
        "{name}: what passes is a prefix"
    );
    if whole.stop.is_some() {
        assert_eq!(whole.pending, 0, "{name}");
        assert_eq!(&input[whole.pass.len()..], &whole.held[..], "{name}: lost");
    } else {
        assert_eq!(
            whole.pass.len() + whole.pending,
            input.len(),
            "{name}: lost"
        );
    }
    let cut = run(input, upgrades, seed, true);
    assert_eq!(
        cut.stop, whole.stop,
        "{name}: the verdict follows the reads"
    );
    assert_eq!(cut.pass, whole.pass, "{name}");
    assert_eq!(cut.held, whole.held, "{name}");
    assert_eq!(cut.pending, whole.pending, "{name}");
}

#[test]
fn msh_66_every_seed_of_the_fuzz_target_holds_its_properties() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/seeds/mesh_head");
    let mut seen = 0;
    for entry in std::fs::read_dir(&dir).expect("the seed directory") {
        let path = entry.unwrap().path();
        let data = std::fs::read(&path).unwrap();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        // A seed is the bytes the target takes: flags, read-size seed, input.
        let [flags, seed, input @ ..] = &data[..] else {
            continue;
        };
        check(&name, input, flags & 1 == 1, *seed);
        // The same input under every flag and a few other cuts.
        for upgrades in [false, true] {
            for seed in [0u8, 1, 7, 200] {
                check(&name, input, upgrades, seed);
                check(&name, &data, upgrades, seed);
            }
        }
        seen += 1;
    }
    assert!(seen >= 5, "the seeds were found ({seen})");
}
