//! Times transcoded audio segments through the engine.
use flux_hls::{presets, AudioKey, EngineConfig, HlsEngine, PlanPrefs, Rendition};
#[tokio::main]
async fn main() {
    let a: Vec<String> = std::env::args().collect();
    let engine = HlsEngine::new(EngineConfig {
        audio_lookahead: 0,
        ..Default::default()
    });
    let asset = engine.prepare(&a[1]).await.unwrap();
    let plan = engine.plan(
        &asset,
        &presets::chrome_desktop(),
        &PlanPrefs {
            allow_direct_play: false,
            ..Default::default()
        },
    );
    let key = plan
        .audio
        .iter()
        .map(AudioKey::from_plan)
        .find(|k| k.path().contains('-'))
        .unwrap();
    for n in [0u32, 2, 1, 3] {
        let t = std::time::Instant::now();
        let b = engine
            .media_segment(&asset, &plan, &Rendition::Audio(key), n)
            .await
            .unwrap();
        println!("seg {n}: {} bytes in {:?}", b.len(), t.elapsed());
    }
    println!("{:?}", engine.transcode_stats());
}
