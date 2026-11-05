use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use rex_picker::{PickerEngine, PickerItem};
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

criterion_group!(benches, matching);
criterion_main!(benches);
