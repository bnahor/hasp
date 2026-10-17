use imara_diff::{Algorithm, Diff, InternedInput};
fn case(name: &str, lines: usize, every: usize) {
    let old: String = (0..lines)
        .map(|i| format!("let v{i} = compute({i});\n"))
        .collect();
    let new: String = (0..lines)
        .map(|i| {
            if every > 0 && i % every == 0 {
                format!("let v{i} = fast({i});\n")
            } else {
                format!("let v{i} = compute({i});\n")
            }
        })
        .collect();
    let input = InternedInput::new(old.as_str(), new.as_str());
    let t = std::time::Instant::now();
    let m = Diff::compute(Algorithm::Myers, &input);
    let myers = t.elapsed();
    let hunks = m.hunks().count();
    let t = std::time::Instant::now();
    let _h = Diff::compute(Algorithm::Histogram, &input);
    let hist = t.elapsed();
    println!(
        "{name:<28} lines {lines:>7} hunks {hunks:>6} lines*hunks {:>12}  myers {:>9.2?}  histogram {:>9.2?}",
        lines * hunks,
        myers,
        hist
    );
}
fn main() {
    case("small edit", 1_000, 200);
    case("typical file, 50 edits", 2_000, 40);
    case("big file, few edits", 50_000, 5_000);
    case("big file, 500 edits", 50_000, 100);
    case("big file, 2k edits", 50_000, 25);
    case("huge file, 200 edits", 200_000, 1_000);
    case("huge file, 2k edits", 200_000, 100);
    case("huge file, 20k edits", 200_000, 10);
    case("whole-file rewrite", 50_000, 1);
}
