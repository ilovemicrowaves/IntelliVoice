/// Mix masked music and voice frames with configurable gains.
///
/// `output[i] = masked_music[i] * music_gain + voice[i] * voice_gain`
///
/// Gains should already be in linear scale (use `db_to_gain` to convert).
pub fn mix_frame(
    masked_music: &[f32],
    voice: &[f32],
    music_gain: f32,
    voice_gain: f32,
    output: &mut [f32],
) {
    let len = masked_music.len().min(voice.len()).min(output.len());
    for i in 0..len {
        output[i] = masked_music[i] * music_gain + voice[i] * voice_gain;
    }
}

/// Convert dB to linear gain: 10^(db/20)
pub fn db_to_gain(db: f32) -> f32 {
    10.0_f32.powf(db / 20.0)
}
