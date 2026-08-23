mod decode;
mod encode;
mod error;
mod normalize;
mod resample;

pub use error::TranscodeError;

use base32::encode;
use sha1::{Digest, Sha1};

#[derive(Debug, Clone)]
pub struct TranscodeResult {
    pub filename: String,
    pub data: Box<[u8]>,
}

pub fn compute_filename(content: &[u8]) -> String {
    let mut hasher = Sha1::new();
    hasher.update(content);
    let hash = hasher.finalize();
    let encoded = encode(base32::Alphabet::Rfc4648 { padding: false }, &hash);
    format!(
        "{}.wav",
        encoded.chars().take(8).collect::<String>().to_uppercase()
    )
}

use audio_file_utils::metadata::{Metadata, MetadataString};
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{MetadataOptions, StandardTagKey};
use symphonia::core::probe::Hint;
use symphonia::default;

const OUT_RATE: u32 = 44100;

pub fn extract_metadata(input: &[u8]) -> Metadata {
    let cursor = std::io::Cursor::new(input.to_vec());
    let mss = MediaSourceStream::new(Box::new(cursor), Default::default());
    let hint = Hint::new();
    let meta_opts: MetadataOptions = Default::default();
    let fmt_opts: FormatOptions = Default::default();
    let mut probed = match default::get_probe().format(&hint, mss, &fmt_opts, &meta_opts) {
        Ok(p) => p,
        Err(_) => return Metadata::default(),
    };

    let mut artist = None;
    let mut title = None;
    let mut album = None;

    extract_metadata_from_container(&mut probed, &mut artist, &mut title, &mut album);
    extract_metadata_outside_container(probed, &mut artist, &mut title, &mut album);

    // RIFF INFO values may include their NUL terminator.
    let strip_nul = |s: Option<String>| -> Option<String> {
        s.map(|s| s.trim_end_matches('\0').to_string())
    };
    let artist = strip_nul(artist);
    let title = strip_nul(title);
    let album = strip_nul(album);

    let artist_str = artist.unwrap_or("Unknown".to_string());
    let title_str = title.unwrap_or("Unknown".to_string());
    let album_str = album.unwrap_or("Unknown".to_string());

    Metadata {
        artist: clamp_to_metadata_string(&artist_str),
        title: clamp_to_metadata_string(&title_str),
        album: clamp_to_metadata_string(&album_str),
    }
}

/// Tags found by the probe outside the container (e.g., ID3v2 preceding an MP3 stream),
/// used to fill any fields the container did not provide.
fn extract_metadata_outside_container(
    mut probed: symphonia::core::probe::ProbeResult,
    artist: &mut Option<String>,
    title: &mut Option<String>,
    album: &mut Option<String>,
) {
    if let Some(meta) = probed.metadata.get()
        && let Some(metadata) = meta.current()
    {
        for tag in metadata.tags() {
            match tag.std_key {
                Some(StandardTagKey::Artist) => {
                    artist.get_or_insert(tag.value.to_string());
                }
                Some(StandardTagKey::TrackTitle) => {
                    title.get_or_insert(tag.value.to_string());
                }
                Some(StandardTagKey::Album) => {
                    album.get_or_insert(tag.value.to_string());
                }
                _ => {}
            }
        }
    }
}

/// Tags embedded in the container itself (e.g., Vorbis comments in OGG, LIST INFO in WAV).
fn extract_metadata_from_container(
    probed: &mut symphonia::core::probe::ProbeResult,
    artist: &mut Option<String>,
    title: &mut Option<String>,
    album: &mut Option<String>,
) {
    let mut meta_log = probed.format.metadata();
    if let Some(metadata) = meta_log.skip_to_latest() {
        for tag in metadata.tags() {
            match tag.std_key {
                Some(StandardTagKey::Artist) => {
                    artist.get_or_insert(tag.value.to_string());
                }
                Some(StandardTagKey::TrackTitle) => {
                    title.get_or_insert(tag.value.to_string());
                }
                Some(StandardTagKey::Album) => {
                    album.get_or_insert(tag.value.to_string());
                }
                _ => {}
            }
        }
    }
}

/// Truncate a string to the metadata field capacity (31 bytes), on a UTF-8 char boundary.
fn clamp_to_metadata_string(s: &str) -> MetadataString {
    if s.len() <= 31 {
        return s
            .try_into()
            .unwrap_or_else(|_| "Unknown".try_into().unwrap());
    }

    let mut end = 31;
    while !s.is_char_boundary(end) {
        end -= 1;
    }

    s[..end]
        .try_into()
        .unwrap_or_else(|_| "Unknown".try_into().unwrap())
}

pub async fn decode_and_normalize(
    input: Box<[u8]>,
    mut progress: impl FnMut(usize, usize) + Clone,
) -> Result<TranscodeResult, TranscodeError> {
    let metadata = extract_metadata(&input);

    // Compute filename before we move the input
    let filename = compute_filename(&input);

    let make_progress = |from: usize, to: usize| {
        let mut progress = progress.clone();
        progress(from, 100);
        move |current: usize, total: usize| {
            let out_range = to - from;
            let out_current = from + current * out_range / total;
            progress(out_current, 100);
        }
    };

    let (downmixed, sample_rate) = decode::decode_to_mono(input, make_progress(0, 33))?;
    let mut samples = resample::resample(
        &downmixed,
        sample_rate as usize,
        OUT_RATE as usize,
        make_progress(34, 67),
    )?;

    // Spotify -14 lufs
    normalize::loudness_normalize(&mut samples, OUT_RATE, 1, -14.0, make_progress(68, 85));

    let samples = to_i16(samples.into());
    progress(90, 100);

    let file = encode::encode_ima_adpcm_wav(&samples, OUT_RATE, &metadata).await?;

    progress(100, 100);
    Ok(TranscodeResult {
        filename,
        data: file,
    })
}

fn to_i16(samples: Box<[f32]>) -> Box<[i16]> {
    let max = i16::MAX - 1;
    let result: Vec<i16> = samples
        .into_iter()
        .map(|x| (x.clamp(-1.0, 1.0) * max as f32) as i16)
        .collect();

    result.into()
}

#[cfg(test)]
mod tests;
