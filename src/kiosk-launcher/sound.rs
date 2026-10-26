//! Shop-floor tones through the tablet speakers. The mixer is raised to full
//! before each cue. Playback is `aplay` of raw stereo S16_LE at 44100 Hz.

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::thread;

const RATE: u32 = 44_100;
const AMPLITUDE: f32 = 0.55;

static SPEAKER: Mutex<()> = Mutex::new(());

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cue {
    /// The scanner finished a code.
    Scanned,
    /// A configuration was found and parsed.
    Ready,
    /// There is no configuration.
    Missing,
}

pub fn startup_cue(has_config: bool) -> Cue {
    if has_config {
        Cue::Ready
    } else {
        Cue::Missing
    }
}

pub fn scan_cues(parsed: bool) -> [Cue; 2] {
    if parsed {
        [Cue::Scanned, Cue::Ready]
    } else {
        [Cue::Scanned, Cue::Missing]
    }
}

pub fn play(cues: &[Cue]) {
    if cues.is_empty() {
        return;
    }
    let cues = cues.to_vec();
    thread::spawn(move || {
        let _guard = SPEAKER.lock().unwrap_or_else(|err| err.into_inner());
        raise_volume();
        let bytes = pcm_bytes(&cues);
        for device in [None, Some("plughw:0,0")] {
            if play_device(device, &bytes) {
                break;
            }
        }
    });
}

fn raise_volume() {
    let Ok(output) = Command::new("amixer").arg("scontrols").output() else {
        return;
    };
    if !output.status.success() {
        return;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    for name in control_names(&text) {
        let _ = Command::new("amixer")
            .args(["-q", "sset", &name, "100%", "unmute"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

fn control_names(text: &str) -> Vec<String> {
    let mut names = Vec::new();
    for line in text.lines() {
        let Some(start) = line.find('\'') else {
            continue;
        };
        let rest = &line[start + 1..];
        let Some(end) = rest.find('\'') else {
            continue;
        };
        let name = &rest[..end];
        if name.is_empty() || names.iter().any(|have| have == name) {
            continue;
        }
        names.push(name.to_string());
    }
    names
}

struct Tone {
    hz: f32,
    ms: u32,
}

fn tones(cue: Cue) -> Vec<Tone> {
    match cue {
        Cue::Scanned => vec![Tone { hz: 988.0, ms: 70 }],
        Cue::Ready => vec![Tone { hz: 784.0, ms: 220 }],
        Cue::Missing => vec![
            Tone { hz: 494.0, ms: 140 },
            Tone { hz: 494.0, ms: 140 },
            Tone { hz: 494.0, ms: 140 },
        ],
    }
}

fn tone_count(cues: &[Cue]) -> usize {
    cues.iter().map(|cue| tones(*cue).len()).sum()
}

fn pcm_bytes(cues: &[Cue]) -> Vec<u8> {
    let mut samples = Vec::new();
    for (index, cue) in cues.iter().enumerate() {
        if index > 0 {
            push_silence(&mut samples, 160);
        }
        for (tone_index, tone) in tones(*cue).iter().enumerate() {
            if tone_index > 0 {
                push_silence(&mut samples, 100);
            }
            push_tone(&mut samples, tone.hz, tone.ms);
        }
    }
    let mut bytes = Vec::with_capacity(samples.len() * 2);
    for sample in samples {
        bytes.extend(sample.to_le_bytes());
    }
    bytes
}

fn push_silence(samples: &mut Vec<i16>, ms: u32) {
    let count = RATE * ms / 1000;
    samples.extend(std::iter::repeat(0).take(count as usize * 2));
}

fn push_tone(samples: &mut Vec<i16>, hz: f32, ms: u32) {
    let count = RATE * ms / 1000;
    let attack = (RATE / 200).max(1);
    let release = (RATE / 40).max(1);
    for i in 0..count {
        let up = (i as f32 / attack as f32).min(1.0);
        let down = if count > release {
            ((count - i) as f32 / release as f32).min(1.0)
        } else {
            1.0
        };
        let t = i as f32 / RATE as f32;
        let value = (t * hz * std::f32::consts::TAU).sin() * AMPLITUDE * up * down;
        let sample = (value * 32767.0).round().clamp(-32767.0, 32767.0) as i16;
        samples.push(sample);
        samples.push(sample);
    }
}

fn play_device(device: Option<&str>, bytes: &[u8]) -> bool {
    let mut command = Command::new("aplay");
    command.args(["-q", "-f", "S16_LE", "-c", "2", "-r", "44100"]);
    if let Some(device) = device {
        command.args(["-D", device]);
    }
    let Ok(mut child) = command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let Some(mut stdin) = child.stdin.take() else {
        let _ = child.wait();
        return false;
    };
    if stdin.write_all(bytes).is_err() {
        drop(stdin);
        let _ = child.wait();
        return false;
    }
    drop(stdin);
    child.wait().is_ok_and(|status| status.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cues_match_the_shop_floor() {
        assert_eq!(tone_count(&[startup_cue(true)]), 1);
        assert_eq!(tone_count(&[startup_cue(false)]), 3);
        assert_eq!(tone_count(&scan_cues(true)), 2);
        assert_eq!(tone_count(&scan_cues(false)), 4);
        let ready = pcm_bytes(&[Cue::Ready]);
        let missing = pcm_bytes(&[Cue::Missing]);
        assert!(missing.len() > ready.len() * 2);
        assert!(ready.chunks(2).any(|sample| sample != [0, 0]));
    }

    #[test]
    fn control_names_are_unique() {
        let text = "\
Simple mixer control 'Master',0
Simple mixer control 'PCM',0
Simple mixer control 'Master',0
Simple mixer control 'Headphone',0
";
        assert_eq!(
            control_names(text),
            ["Master".to_string(), "PCM".to_string(), "Headphone".to_string()]
        );
    }
}
