use std::sync::Arc;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use rex_picker::{
    PickerDocument, PickerEngine, PickerItem, PickerOptions, PreviewProvider, ui::FrameBench,
};
use serde_json::Value;

fn candidates(count: usize) -> Vec<PickerItem> {
    (0..count)
        .map(|index| PickerItem {
            id: format!("item-{index}"),
            label: format!("src/components/feature_{index}/model_picker.rs"),
            description: Some(format!("Candidate number {index}")),
            search_text: Some(format!("provider-{index} model-{index}")),
            preview: None,
            metadata: Value::Null,
        })
        .collect()
}

/// Full construction, indexing, and one query.
fn matching(c: &mut Criterion) {
    let mut group = c.benchmark_group("matching");
    for count in [100, 1_000, 5_000] {
        let items = candidates(count);
        group.bench_with_input(BenchmarkId::from_parameter(count), &items, |b, items| {
            b.iter(|| {
                let mut engine = PickerEngine::new(items.clone(), false);
                engine.set_query("cmp feat 42");
                engine.settle();
                engine.match_count()
            });
        });
    }
    group.finish();
}

/// Typing a query one character at a time against an indexed list, which is
/// the interactive hot path and exercises Nucleo's append optimization.
fn typing(c: &mut Criterion) {
    const QUERY: &str = "cmpfeat42";
    let mut group = c.benchmark_group("typing");
    for count in [1_000, 5_000] {
        let mut engine = PickerEngine::new(candidates(count), false);
        engine.settle();
        group.bench_function(BenchmarkId::from_parameter(count), |b| {
            b.iter(|| {
                let mut query = String::with_capacity(QUERY.len());
                for character in QUERY.chars() {
                    query.push(character);
                    engine.set_query(query.as_str());
                    engine.settle();
                }
                engine.set_query("");
                engine.settle();
                engine.match_count()
            });
        });
    }
    group.finish();
}

struct NoPreview;

impl PreviewProvider for NoPreview {
    fn load(&self, _: &PickerItem) -> anyhow::Result<Option<String>> {
        Ok(None)
    }
}

/// Rendering one 160x48 frame, with and without a 1,000-line preview.
fn rendering(c: &mut Criterion) {
    let document = PickerDocument {
        version: 1,
        options: PickerOptions {
            title: Some("Bench".to_owned()),
            multi: true,
            max_visible: 40,
            ..Default::default()
        },
        items: candidates(5_000),
    };
    let preview: String = (1..=1_000)
        .map(|line| format!("{line:>4} │ let value_{line} = compute(\"{line}\");\n"))
        .collect();

    let mut group = c.benchmark_group("render");
    let mut list = FrameBench::new(document.clone(), 160, 48, None).expect("frame bench");
    group.bench_function("list", |b| b.iter(|| list.draw().expect("draw")));

    let mut with_preview =
        FrameBench::new(document, 160, 48, Some(Arc::new(NoPreview))).expect("frame bench");
    with_preview.set_preview(preview);
    group.bench_function("list+preview", |b| {
        b.iter(|| with_preview.draw().expect("draw"))
    });
    group.finish();
}

criterion_group!(benches, matching, typing, rendering);
criterion_main!(benches);
