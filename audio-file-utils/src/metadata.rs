use core::iter::repeat_n;

use embedded_io_async::{ErrorType, Read, ReadExactError, Write};
use heapless::{String, Vec};
use thiserror::Error;

pub type MetadataString = String<31>;

#[derive(Clone, Debug, PartialEq)]
pub struct Metadata {
    pub artist: MetadataString,
    pub title: MetadataString,
    pub album: MetadataString,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MetadataAndDuration {
    pub metadata: Metadata,
    pub duration: usize,
}

#[derive(Error, Debug)]
#[error("Error while extracting metadata")]
pub enum Error<E> {
    Read(ReadExactError<E>),
    Write(E),
    InvalidFileType,
}

impl Default for Metadata {
    fn default() -> Self {
        let default: String<31> = "Unknown".try_into().unwrap();
        Self {
            artist: default.clone(),
            title: default.clone(),
            album: default,
        }
    }
}

pub const INFO_CHUNK_SIZE: usize = 4 + 40 * 3;

pub async fn extract_metadata<R>(mut reader: R) -> Result<Metadata, Error<<R as ErrorType>::Error>>
where
    R: Read,
{
    let mut buf = [0u8; 12];
    reader.read_exact(&mut buf).await.map_err(Error::Read)?;
    if &buf[0..4] != b"RIFF" || &buf[8..12] != b"WAVE" {
        return Err(Error::InvalidFileType);
    }

    let mut artist: Option<String<31>> = None;
    let mut title: Option<String<31>> = None;
    let mut album: Option<String<31>> = None;

    loop {
        let mut chunk_header = [0u8; 8];
        // check for end of file
        if reader.read_exact(&mut chunk_header).await.is_err() {
            break;
        }

        let chunk_id = &chunk_header[0..4];
        let chunk_size = u32::from_le_bytes(chunk_header[4..8].try_into().unwrap()) as usize;

        if chunk_id == b"LIST" {
            let mut list_type = [0u8; 4];
            reader
                .read_exact(&mut list_type)
                .await
                .map_err(Error::Read)?;
            if &list_type == b"INFO" {
                parse_info(
                    &mut reader,
                    chunk_size.saturating_sub(4),
                    &mut artist,
                    &mut title,
                    &mut album,
                )
                .await?;
            }

            return Ok(Metadata {
                artist: artist.unwrap_or("Unknown".try_into().unwrap()),
                title: title.unwrap_or("Unknown".try_into().unwrap()),
                album: album.unwrap_or("Unknown".try_into().unwrap()),
            });
        } else {
            // Skip chunk
            skip(&mut reader, chunk_size).await?;
        }
    }

    Ok(Metadata {
        artist: "Unknown".try_into().unwrap(),
        title: "Unknown".try_into().unwrap(),
        album: "Unknown".try_into().unwrap(),
    })
}

/// Maximum size of the LIST/INFO body we are willing to buffer. Our writer emits a
/// fixed `INFO_CHUNK_SIZE` (124) body, but we allow some slack to be safe.
const INFO_BUF_SIZE: usize = 132;

/// Parse a LIST "INFO" body (already advanced past the 8-byte LIST header and the
/// 4-byte "INFO" list type). Both spec-conformant (word-aligned, with a pad byte
/// after odd-sized subchunks) and legacy (unpadded) encodings are supported: after
/// reading each subchunk we detect the presence of a pad byte by checking whether
/// the next bytes begin a known INFO subchunk ID.
async fn parse_info<R>(
    reader: &mut R,
    info_size: usize,
    artist: &mut Option<String<31>>,
    title: &mut Option<String<31>>,
    album: &mut Option<String<31>>,
) -> Result<(), Error<<R as ErrorType>::Error>>
where
    R: Read,
{
    let mut buf = [0u8; INFO_BUF_SIZE];
    let mut len = 0usize;
    let target = info_size.min(INFO_BUF_SIZE);
    while len < target {
        let n = reader
            .read(&mut buf[len..target])
            .await
            .map_err(|e| Error::Read(ReadExactError::Other(e)))?;
        if n == 0 {
            break;
        }
        len += n;
    }

    let data = &buf[..len];
    let mut off = 0usize;

    while off + 8 <= data.len() {
        let sub_id = &data[off..off + 4];
        let sub_size = u32::from_le_bytes(data[off + 4..off + 8].try_into().unwrap()) as usize;
        off += 8;

        // Copy the (clamped) text payload, bounded by the available buffer.
        let text_size = sub_size.min(31);
        let mut data_vec: Vec<u8, 31> = Vec::from_iter(repeat_n(0, 31));
        let copy_len = data.len().saturating_sub(off).min(text_size);
        data_vec[..copy_len].copy_from_slice(&data[off..off + copy_len]);
        while data_vec.last() == Some(&0) {
            data_vec.pop();
        }

        let text_str = core::str::from_utf8(&data_vec).unwrap_or("Unknown");
        let text: String<31> =
            text_str.try_into().unwrap_or("Unknown".try_into().unwrap());
        match sub_id {
            b"IART" => *artist = Some(text),
            b"INAM" => *title = Some(text),
            b"IPRD" => *album = Some(text),
            _ => {}
        }

        // Advance past the whole subchunk payload.
        off = off.saturating_add(sub_size).min(data.len());

        // Detect an optional pad byte after an odd-sized subchunk. If the next four
        // bytes begin a known INFO subchunk ID, the data was written without a pad
        // byte (legacy format); otherwise there is a single pad byte to skip.
        if sub_size & 0x1 == 1
            && off < data.len()
            && !is_known_info_id(&data[off..data.len().min(off + 4)])
        {
            off += 1;
        }
    }

    Ok(())
}

fn is_known_info_id(candidate: &[u8]) -> bool {
    candidate == b"IART" || candidate == b"INAM" || candidate == b"IPRD"
}

async fn skip<R>(reader: &mut R, mut size: usize) -> Result<(), Error<<R as ErrorType>::Error>>
where
    R: Read,
{
    let mut buf = [0u8; 16];
    while size > 0 {
        let to_read = size.min(16);
        reader
            .read_exact(&mut buf[..to_read])
            .await
            .map_err(Error::Read)?;
        size -= to_read;
    }

    Ok(())
}

/// Write a single INFO subchunk. Per the RIFF spec, chunk data is padded to an
/// even byte count; the pad byte is not included in the size field.
async fn write_info_subchunk<W>(
    writer: &mut W,
    id: &[u8; 4],
    data: &[u8],
) -> Result<usize, Error<<W as ErrorType>::Error>>
where
    W: Write,
{
    writer.write_all(id).await.map_err(Error::Write)?;
    let len = data.len().min(31);
    writer
        .write_all(&((len + 1) as u32).to_le_bytes())
        .await
        .map_err(Error::Write)?;
    writer.write_all(&data[..len]).await.map_err(Error::Write)?;
    writer.write_all(&[0]).await.map_err(Error::Write)?;

    // Word-align: emit one pad byte when the data length (including NUL) is odd.
    let mut written = 8 + len + 1;
    if (len + 1) & 0x1 == 1 {
        writer.write_all(&[0]).await.map_err(Error::Write)?;
        written += 1;
    }

    Ok(written)
}

pub async fn write_info_chunk<W>(
    mut writer: W,
    metadata: &Metadata,
) -> Result<(), Error<<W as ErrorType>::Error>>
where
    W: Write,
{
    writer.write_all(b"INFO").await.map_err(Error::Write)?;

    let artist_data = metadata.artist.as_bytes();
    let title_data = metadata.title.as_bytes();
    let album_data = metadata.album.as_bytes();

    // Pad the LIST data with zeros
    let written = 4
        + write_info_subchunk(&mut writer, b"IART", artist_data).await?
        + write_info_subchunk(&mut writer, b"INAM", title_data).await?
        + write_info_subchunk(&mut writer, b"IPRD", album_data).await?;
    let padding = INFO_CHUNK_SIZE - written;
    for _ in 0..padding {
        writer.write_all(&[0]).await.map_err(Error::Write)?;
    }

    Ok(())
}

#[cfg(test)]
#[allow(unused_must_use)] // heapless Vec operations return Result; ignored in test builders
mod tests {
    extern crate std;

    use super::*;

    /// Blocking executor for `embedded-io-async` futures. Delegates to `futures`
    /// so we don't need to hand-roll a waker/pin in test code.
    fn block_on<F: futures::Future>(fut: F) -> F::Output {
        futures::executor::block_on(fut)
    }

    /// Wrap a full INFO body (incl. the leading "INFO" fourCC) in a minimal IMA-ADPCM WAV.
    fn embed_info_in_wav(info: &[u8], out: &mut Vec<u8, 256>) {
        out.extend_from_slice(b"RIFF");
        let riff_size_pos = out.len(); // after "RIFF", points at the size field
        out.extend_from_slice(&0u32.to_le_bytes()); // patched below
        out.extend_from_slice(b"WAVE");

        // fmt chunk (20 bytes, IMA ADPCM)
        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&20u32.to_le_bytes());
        out.extend_from_slice(&0x0011u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&44100u32.to_le_bytes());
        out.extend_from_slice(&22579u32.to_le_bytes());
        out.extend_from_slice(&1024u16.to_le_bytes());
        out.extend_from_slice(&4u16.to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&2041u16.to_le_bytes());

        // LIST/INFO chunk
        out.extend_from_slice(b"LIST");
        let list_size_pos = out.len();
        out.extend_from_slice(&0u32.to_le_bytes()); // patched below
        let list_payload_start = out.len();
        out.extend_from_slice(info);

        // data chunk + small payload
        out.extend_from_slice(b"data");
        let data_size_pos = out.len();
        out.extend_from_slice(&0u32.to_le_bytes()); // patched below
        out.extend_from_slice(&[0u8; 16]);

        let list_payload_size = (out.len() - list_payload_start) as u32;
        out[list_size_pos..list_size_pos + 4].copy_from_slice(&list_payload_size.to_le_bytes());
        out[data_size_pos..data_size_pos + 4].copy_from_slice(&16u32.to_le_bytes());
        let riff_size = (out.len() - 8) as u32;
        out[riff_size_pos..riff_size_pos + 4].copy_from_slice(&riff_size.to_le_bytes());
    }

    /// Build a minimal IMA-ADPCM WAV with a LIST/INFO chunk.
    /// When `aligned == true`, odd-sized INFO subchunks get a trailing pad byte
    /// (the format written since the metadata alignment fix); when `false`, no
    /// pad bytes are emitted (the old transcoder format).
    fn build_wav(artist: &str, title: &str, album: &str, aligned: bool) -> Vec<u8, 256> {
        fn push_subchunk(info: &mut Vec<u8, 256>, id: &[u8; 4], val: &str, aligned: bool) {
            let data = val.as_bytes();
            let size = (data.len() + 1) as u32; // data + NUL terminator
            info.extend_from_slice(id);
            info.extend_from_slice(&size.to_le_bytes());
            info.extend_from_slice(data);
            info.push(0);
            if aligned && (size & 0x1) == 1 {
                info.push(0);
            }
        }

        let mut info: Vec<u8, 256> = Vec::new();
        info.extend_from_slice(b"INFO");
        push_subchunk(&mut info, b"IART", artist, aligned);
        push_subchunk(&mut info, b"INAM", title, aligned);
        push_subchunk(&mut info, b"IPRD", album, aligned);
        // Pad the INFO body (including the leading "INFO" fourCC) out to the fixed
        // INFO_CHUNK_SIZE, matching the writer's layout.
        while info.len() < INFO_CHUNK_SIZE {
            info.push(0);
        }

        let mut wav: Vec<u8, 256> = Vec::new();
        embed_info_in_wav(&info, &mut wav);
        wav
    }

    fn parse(wav: &[u8]) -> Metadata {
        let mut reader: &[u8] = wav;
        block_on(extract_metadata(&mut reader)).unwrap_or_default()
    }

    #[test]
    fn writer_writes_aligned_chunks_of_fixed_size() {
        let metadata = Metadata {
            artist: "Artist".try_into().unwrap(),
            title: "Title".try_into().unwrap(),
            album: "Album".try_into().unwrap(),
        };
        use std::vec::Vec as StdVec;
        let mut out: StdVec<u8> = StdVec::new();
        block_on(write_info_chunk(&mut out, &metadata)).unwrap();

        // Total written must be exactly the fixed INFO_CHUNK_SIZE.
        assert_eq!(out.len(), INFO_CHUNK_SIZE);
        // Starts with the INFO list type.
        assert_eq!(&out[0..4], b"INFO");

        // IART: "Artist" (6) => size 7 (odd) => header + 7 data + 1 pad byte.
        assert_eq!(&out[4..8], b"IART");
        assert_eq!(u32::from_le_bytes(out[8..12].try_into().unwrap()), 7);
        assert_eq!(&out[12..18], b"Artist");
        assert_eq!(out[18], 0); // NUL
        assert_eq!(out[19], 0); // word-alignment pad
        // Next subchunk header must start at a word boundary (offset 20).
        assert_eq!(&out[20..24], b"INAM");

        // INAM: "Title" (5) => size 6 (even) => no pad byte.
        assert_eq!(u32::from_le_bytes(out[24..28].try_into().unwrap()), 6);
        assert_eq!(&out[28..33], b"Title");
        assert_eq!(out[33], 0); // NUL (no pad — size even)
        assert_eq!(&out[34..38], b"IPRD");

        // Trailing bytes after the three subchunks are zero padding.
        let written = 4 + (8 + 7 + 1) + (8 + 6) + (8 + 6);
        assert_eq!(written, 48);
        assert!(out[written..].iter().all(|&b| b == 0));
    }

    #[test]
    fn writer_writer_round_trips_through_reader() {
        let metadata = Metadata {
            artist: "Abcdefgh".try_into().unwrap(),
            title: "X".try_into().unwrap(),
            album: "Unknown".try_into().unwrap(),
        };
        // Serialize via the real writer.
        use std::vec::Vec as StdVec;
        let mut info: StdVec<u8> = StdVec::new();
        block_on(write_info_chunk(&mut info, &metadata)).unwrap();

        // Wrap and read back.
        let mut wav: Vec<u8, 256> = Vec::new();
        embed_info_in_wav(&info, &mut wav);
        let m = parse(&wav);

        assert_eq!(m.artist.as_str(), "Abcdefgh");
        assert_eq!(m.title.as_str(), "X");
        assert_eq!(m.album.as_str(), "Unknown");
    }

    #[test]
    fn reads_new_aligned_format() {
        let wav = build_wav("Artist", "Title", "Album", true);
        let m = parse(&wav);
        assert_eq!(m.artist.as_str(), "Artist");
        assert_eq!(m.title.as_str(), "Title");
        assert_eq!(m.album.as_str(), "Album");
    }

    #[test]
    fn reads_new_aligned_odd_fields() {
        // Fields whose (len + 1) is odd -> each gets a pad byte in the aligned format.
        let wav = build_wav("Abcdefgh", "X", "Unknown", true);
        let m = parse(&wav);
        assert_eq!(m.artist.as_str(), "Abcdefgh");
        assert_eq!(m.title.as_str(), "X");
        assert_eq!(m.album.as_str(), "Unknown");
    }

    #[test]
    fn reads_old_unaligned_format() {
        let wav = build_wav("Artist", "Title", "Album", false);
        let m = parse(&wav);
        assert_eq!(m.artist.as_str(), "Artist");
        assert_eq!(m.title.as_str(), "Title");
        assert_eq!(m.album.as_str(), "Album");
    }

    #[test]
    fn reads_old_unaligned_odd_fields() {
        // Old format omits pad bytes entirely; this previously caused a huge phantom
        // skip (sub_size ~1.4 GB) and very slow reads.
        let wav = build_wav("Abcdefgh", "X", "Unknown", false);
        let m = parse(&wav);
        assert_eq!(m.artist.as_str(), "Abcdefgh");
        assert_eq!(m.title.as_str(), "X");
        assert_eq!(m.album.as_str(), "Unknown");
    }
}
