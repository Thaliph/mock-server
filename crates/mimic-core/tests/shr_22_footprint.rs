//! SHR-22 (#339): the heap footprint of a parsed report. The report of SHR-18 at the store
//! limits is read with `Report::from_json`; the live heap bytes plus 16 bytes per live block (a
//! malloc header) are compared with its JSON length, which must be at most 1.6 times.

mod common;

use mimic_core::discovery::report::Report;

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

#[test]
fn shr_22_parsed_report_takes_at_most_1_6_times_its_json() {
    let _profiler = dhat::Profiler::builder().testing().build();
    let json = common::full_report(0).to_json().expect("serialises");
    let before = dhat::HeapStats::get();
    let report = Report::from_json(&json).expect("the report reads");
    let after = dhat::HeapStats::get();
    let bytes = after.curr_bytes - before.curr_bytes;
    let blocks = after.curr_blocks - before.curr_blocks;
    let cost = bytes + 16 * blocks;
    #[allow(clippy::cast_precision_loss)]
    let ratio = cost as f64 / json.len() as f64;
    eprintln!(
        "SHR22 json={} live_bytes={bytes} live_blocks={blocks} ratio={ratio:.2}",
        json.len()
    );
    assert!(ratio <= 1.6, "the parsed report costs {ratio:.2} times its JSON");
    assert!(report.exchanges() > 0);
    assert!(json.len() > 20 << 20, "the report is at the store limits");
}
