//! Регрессия численной совместимости с эталонной реализацией openWakeWord.
//!
//! Эталонные эмбеддинги получены независимой Go-реализацией того же
//! feature-backbone на детерминированном тоне 440 Гц. Допуск 0.25 отражает
//! разный порядок суммирования f32; форма и число окон должны совпадать
//! точно. Сами веса головы модели в репозиторий не входят.

use std::path::Path;

use jarvis_wakeword::features::Features;

#[test]
fn embeddings_match_reference_pipeline() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let assets = root.join("../../assets/oww.bin");
    if !assets.exists() {
        eprintln!("oww.bin отсутствует — проверка совместимости пропущена");
        return;
    }
    let want: Vec<Vec<f32>> = serde_json::from_str(
        &std::fs::read_to_string(root.join("tests/data/embeddings-go.json")).unwrap(),
    )
    .unwrap();
    let mut f = Features::load(&assets).unwrap();
    let pcm: Vec<i16> = (0..32_000)
        .map(|i| ((2.0 * std::f32::consts::PI * 440.0 * i as f32 / 16_000.0).sin() * 3000.0) as i16)
        .collect();
    let mut got = Vec::new();
    for chunk in pcm.chunks(333) {
        got.extend(f.push(chunk));
    }
    assert_eq!(got.len(), want.len(), "число окон");
    let mut worst = 0.0f32;
    for (a, b) in got.iter().zip(&want) {
        assert_eq!(a.len(), b.len());
        for (x, y) in a.iter().zip(b) {
            worst = worst.max((x - y).abs());
        }
    }
    assert!(worst < 0.25, "максимальное расхождение {worst}");
}
