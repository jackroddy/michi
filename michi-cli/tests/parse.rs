use michi_cli::ast::{Item, PipelineItem, StepItem};

#[test]
fn the_worked_example_parses() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/cases/syntax.michi");
    let source = std::fs::read_to_string(path).unwrap();
    let file = match michi_cli::parse(&source) {
        Ok(file) => file,
        Err(err) => panic!("{}", err.render("syntax.michi", &source)),
    };
    assert_eq!(file.items.len(), 2);
    let Item::Pipeline(pipeline) = &file.items[1] else {
        panic!("second item is the pipeline");
    };
    assert_eq!(pipeline.name.text, "bench");
    assert_eq!(pipeline.attrs.len(), 5);

    let steps: Vec<_> = pipeline
        .items
        .iter()
        .filter_map(|item| match item {
            PipelineItem::Step(step) => Some(step),
            _ => None,
        })
        .collect();
    assert_eq!(steps.len(), 6);

    let bench = steps
        .iter()
        .find(|s| s.name.as_ref().unwrap().text == "bench")
        .unwrap();
    let kinds: Vec<&str> = bench
        .body
        .iter()
        .map(|item| match item {
            StepItem::Command(c) if c.raw => "raw",
            StepItem::Command(_) => "cmd",
            StepItem::Sweep(_) => "sweep",
        })
        .collect();
    assert_eq!(kinds, ["cmd", "sweep", "sweep", "raw"]);
}
