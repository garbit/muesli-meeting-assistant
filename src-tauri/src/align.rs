//! Pure functions that turn sidecar outputs (ASR words, diarisation segments,
//! energy timeline) into speaker-attributed transcript segments.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Deserialize)]
pub struct AsrWord {
    pub w: String,
    pub s: f64,
    pub e: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AsrOutput {
    pub engine: String,
    pub text: String,
    pub words: Vec<AsrWord>,
    #[serde(default)]
    pub duration_s: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DiarSegment {
    pub speaker: String,
    pub s: f64,
    pub e: f64,
    #[serde(default)]
    pub q: f32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DiarOutput {
    pub engine: String,
    pub segments: Vec<DiarSegment>,
    #[serde(default)]
    pub speakers: HashMap<String, Vec<f32>>,
}

/// Mirrors the Swift `EnergyTimeline` (camelCase keys: `frameMs`, `mic`, `system`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnergyTimeline {
    pub frame_ms: u32,
    pub mic: Vec<f32>,
    pub system: Vec<f32>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DecodedAudio {
    pub mix_path: String,
    pub duration_ms: i64,
    pub sample_rate: u32,
    pub source_channels: u32,
    pub energy: Option<EnergyTimeline>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Turn {
    pub start_ms: i64,
    pub end_ms: i64,
    pub text: String,
    pub label: String,
    pub mic_ratio: Option<f64>,
}

/// Assign each word to the diarisation segment with the largest overlap (or the
/// nearest segment when none overlaps), then merge consecutive words of the same
/// speaker into turns. A turn is split on silences longer than `max_gap_s` or
/// when it exceeds `max_words`.
pub fn build_turns(words: &[AsrWord], diar: &[DiarSegment], max_gap_s: f64, max_words: usize) -> Vec<Turn> {
    if words.is_empty() {
        return Vec::new();
    }
    let mut labels: Vec<String> = Vec::with_capacity(words.len());
    for w in words {
        let mut best: Option<(&DiarSegment, f64)> = None;
        for d in diar {
            let overlap = (w.e.min(d.e) - w.s.max(d.s)).max(0.0);
            if overlap > 0.0 && best.map(|(_, o)| overlap > o).unwrap_or(true) {
                best = Some((d, overlap));
            }
        }
        let label = match best {
            Some((d, _)) => d.speaker.clone(),
            None => {
                // nearest segment by midpoint distance
                let mid = (w.s + w.e) / 2.0;
                diar.iter()
                    .min_by(|a, b| {
                        let da = if mid < a.s { a.s - mid } else if mid > a.e { mid - a.e } else { 0.0 };
                        let db = if mid < b.s { b.s - mid } else if mid > b.e { mid - b.e } else { 0.0 };
                        da.partial_cmp(&db).unwrap()
                    })
                    .map(|d| d.speaker.clone())
                    .unwrap_or_else(|| "S1".into())
            }
        };
        labels.push(label);
    }

    // Smooth single-word islands: a lone word whose neighbours agree on another
    // speaker is almost always an alignment artefact.
    for i in 1..labels.len().saturating_sub(1) {
        if labels[i - 1] == labels[i + 1] && labels[i] != labels[i - 1] {
            labels[i] = labels[i - 1].clone();
        }
    }

    let mut turns: Vec<Turn> = Vec::new();
    let mut cur: Option<(usize, usize)> = None; // word index range [start, end)
    let flush = |turns: &mut Vec<Turn>, range: (usize, usize)| {
        let ws = &words[range.0..range.1];
        let text = ws.iter().map(|w| w.w.as_str()).collect::<Vec<_>>().join(" ");
        turns.push(Turn {
            start_ms: (ws[0].s * 1000.0).round() as i64,
            end_ms: (ws[ws.len() - 1].e * 1000.0).round() as i64,
            text,
            label: labels[range.0].clone(),
            mic_ratio: None,
        });
    };
    for i in 0..words.len() {
        match cur {
            None => cur = Some((i, i + 1)),
            Some((start, end)) => {
                let same = labels[i] == labels[start];
                let gap = words[i].s - words[end - 1].e;
                if same && gap <= max_gap_s && (end - start) < max_words {
                    cur = Some((start, i + 1));
                } else {
                    flush(&mut turns, (start, end));
                    cur = Some((i, i + 1));
                }
            }
        }
    }
    if let Some(r) = cur {
        flush(&mut turns, r);
    }
    turns
}

/// Mean of mic / (mic + system) energy over a time range; None when the
/// timeline is absent or the range is silent.
pub fn mic_ratio(energy: Option<&EnergyTimeline>, start_ms: i64, end_ms: i64) -> Option<f64> {
    let e = energy?;
    let frame = e.frame_ms.max(1) as i64;
    let a = (start_ms / frame).max(0) as usize;
    let b = ((end_ms + frame - 1) / frame).max(0) as usize;
    let b = b.min(e.mic.len()).min(e.system.len());
    if a >= b {
        return None;
    }
    let mut num = 0.0f64;
    let mut den = 0.0f64;
    for i in a..b {
        let m = e.mic[i] as f64;
        let s = e.system[i] as f64;
        num += m;
        den += m + s;
    }
    if den <= 1e-6 { None } else { Some(num / den) }
}

/// Per-label `(speech_ms, mean mic ratio)`.
pub type SpeakerStats = HashMap<String, (i64, Option<f64>)>;

/// Per-label speech time and mean mic ratio, plus the label that is most
/// likely the local user ("Me"): highest mic ratio above `threshold`.
pub fn speaker_stats(turns: &[Turn]) -> (SpeakerStats, Option<String>) {
    let mut acc: HashMap<String, (i64, f64, i64)> = HashMap::new();
    for t in turns {
        let e = acc.entry(t.label.clone()).or_insert((0, 0.0, 0));
        let dur = (t.end_ms - t.start_ms).max(0);
        e.0 += dur;
        if let Some(r) = t.mic_ratio {
            e.1 += r * dur as f64;
            e.2 += dur;
        }
    }
    let stats: SpeakerStats = acc
        .into_iter()
        .map(|(k, (ms, w, n))| (k, (ms, if n > 0 { Some(w / n as f64) } else { None })))
        .collect();
    let me = stats
        .iter()
        .filter_map(|(k, (_, r))| r.map(|r| (k.clone(), r)))
        .filter(|(_, r)| *r >= 0.7)
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
        .map(|(k, _)| k);
    (stats, me)
}

/// Labels ordered by first appearance so `S2` can become `Speaker 1` when it
/// speaks first.
pub fn label_order(turns: &[Turn]) -> Vec<String> {
    let mut seen = Vec::new();
    for t in turns {
        if !seen.contains(&t.label) {
            seen.push(t.label.clone());
        }
    }
    seen
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(t: &str, s: f64, e: f64) -> AsrWord {
        AsrWord { w: t.into(), s, e }
    }

    #[test]
    fn words_follow_diarisation_and_merge_into_turns() {
        let words = vec![w("hello", 0.0, 0.4), w("there", 0.5, 0.9), w("hi", 2.0, 2.3), w("back", 2.4, 2.8), w("ok", 5.0, 5.2)];
        let diar = vec![
            DiarSegment { speaker: "S1".into(), s: 0.0, e: 1.0, q: 1.0 },
            DiarSegment { speaker: "S2".into(), s: 1.9, e: 3.0, q: 1.0 },
            DiarSegment { speaker: "S1".into(), s: 4.9, e: 5.5, q: 1.0 },
        ];
        let turns = build_turns(&words, &diar, 1.5, 200);
        assert_eq!(turns.len(), 3);
        assert_eq!(turns[0].text, "hello there");
        assert_eq!(turns[0].label, "S1");
        assert_eq!(turns[1].label, "S2");
        assert_eq!(turns[2].start_ms, 5000);
    }

    #[test]
    fn lone_word_islands_are_smoothed() {
        let words = vec![w("a", 0.0, 0.1), w("b", 0.2, 0.3), w("c", 0.4, 0.5)];
        let diar = vec![
            DiarSegment { speaker: "S1".into(), s: 0.0, e: 0.15, q: 1.0 },
            DiarSegment { speaker: "S2".into(), s: 0.15, e: 0.35, q: 1.0 },
            DiarSegment { speaker: "S1".into(), s: 0.35, e: 0.6, q: 1.0 },
        ];
        let turns = build_turns(&words, &diar, 1.5, 200);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].text, "a b c");
    }

    /// Contract test against the exact JSON the Swift `decode` method returns for a
    /// retained (mic + system) recording. A camelCase mismatch here broke every live
    /// recording once; keep this in sync with Decode/AudioDecoder.swift.
    #[test]
    fn decoded_audio_parses_the_sidecar_shape() {
        let json = r#"{"mixPath":"/tmp/mix16k.wav","durationMs":70346,"sampleRate":16000,"sourceChannels":2,
                       "energy":{"frameMs":100,"mic":[0.1,0.2],"system":[0.0,0.3]}}"#;
        let d: DecodedAudio = serde_json::from_str(json).unwrap();
        assert_eq!(d.energy.as_ref().unwrap().frame_ms, 100);
        assert_eq!(d.source_channels, 2);
        let mono = r#"{"mixPath":"/tmp/m.wav","durationMs":10,"sampleRate":16000,"sourceChannels":1,"energy":null}"#;
        assert!(serde_json::from_str::<DecodedAudio>(mono).unwrap().energy.is_none());
    }

    #[test]
    fn mic_ratio_and_me_detection() {
        let energy = EnergyTimeline { frame_ms: 100, mic: vec![0.9, 0.9, 0.0, 0.0], system: vec![0.1, 0.1, 0.8, 0.8] };
        assert!(mic_ratio(Some(&energy), 0, 200).unwrap() > 0.85);
        assert!(mic_ratio(Some(&energy), 200, 400).unwrap() < 0.05);
        let turns = vec![
            Turn { start_ms: 0, end_ms: 200, text: "x".into(), label: "S1".into(), mic_ratio: Some(0.9) },
            Turn { start_ms: 200, end_ms: 400, text: "y".into(), label: "S2".into(), mic_ratio: Some(0.0) },
        ];
        let (stats, me) = speaker_stats(&turns);
        assert_eq!(stats["S1"].0, 200);
        assert_eq!(me.as_deref(), Some("S1"));
    }
}
