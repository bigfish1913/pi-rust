//! Cold locations from one completed streaming replay, with no reader handle
//! or historical entry/frame bodies in the portable image.
use super::*;
use crate::session::state::StartupIndex;
use serde::Serialize;

#[derive(Deserialize, Serialize)]
struct LocationImage {
    seq: u64,
    stream_index: usize,
    append: bool,
    retired: bool,
    offset: u64,
    length: usize,
    #[serde(with = "digest")]
    digest: [u8; 32],
    id: String,
    lane: String,
    run: String,
}

impl From<&Location> for LocationImage {
    fn from(location: &Location) -> Self {
        Self {
            seq: location.seq,
            stream_index: location.stream_index,
            append: location.append,
            retired: location.retired,
            offset: location.offset,
            length: location.length,
            digest: location.digest,
            id: location.id.clone(),
            lane: location.lane.to_string(),
            run: location.run.to_string(),
        }
    }
}

impl LocationImage {
    fn fits(&self, start: u64, end: u64) -> bool {
        self.length != 0
            && self.offset >= start
            && self
                .offset
                .checked_add(self.length as u64)
                .is_some_and(|at| at < end)
    }

    fn bind(self, reader: &Arc<dyn ReadFile>, lane: Arc<str>, run: Arc<str>) -> Arc<Location> {
        Arc::new(Location {
            reader: reader.clone(),
            seq: self.seq,
            stream_index: self.stream_index,
            append: self.append,
            retired: self.retired,
            offset: self.offset,
            length: self.length,
            digest: self.digest,
            id: self.id,
            lane,
            run,
        })
    }
}

#[derive(Deserialize, Serialize)]
struct EntryImage {
    source: LocationImage,
    live_nulls: LiveNulls,
}

#[derive(Deserialize, Serialize)]
pub(in crate::session::jsonl) struct StartupImage {
    index: StartupIndex<EntryImage>,
    frames: Vec<LocationImage>,
}

impl ResidentState {
    pub(in crate::session::jsonl) fn startup_image(&self) -> Option<StartupImage> {
        if !self.pending.is_empty() {
            return None;
        }
        Some(StartupImage {
            index: self.reducer.startup_image(|entry| match entry {
                JsonlPayload::Cold {
                    source, live_nulls, ..
                } => Some(EntryImage {
                    source: LocationImage::from(source.as_ref()),
                    live_nulls: live_nulls.clone(),
                }),
                JsonlPayload::Resident(_) => None,
            })?,
            frames: self
                .cold
                .locations
                .iter()
                .map(|loc| LocationImage::from(loc.as_ref()))
                .collect(),
        })
    }
}

impl StartupImage {
    pub(in crate::session::jsonl) fn bind(
        self,
        reader: &Arc<dyn ReadFile>,
        header_end: u64,
        prefix_end: u64,
    ) -> Option<ResidentState> {
        let empty = Arc::<str>::from("");
        let mut prior_offset = header_end;
        let reducer = self.index.restore(|entry, meta| {
            let source = entry.source;
            if !source.fits(prior_offset, prefix_end)
                || source.id != meta.id
                || source.seq != meta.seq
                || source.append
                || source.retired
                || !source.lane.is_empty()
                || !source.run.is_empty()
            {
                return None;
            }
            prior_offset = source.offset + source.length as u64 + 1;
            Some(JsonlPayload::Cold {
                source: source.bind(reader, empty.clone(), empty.clone()),
                parent: meta.parent_id.clone(),
                live_nulls: entry.live_nulls,
            })
        })?;
        let mut state = ResidentState {
            reducer,
            ..Default::default()
        };
        let mut prior_seq = 0;
        let mut prior_offset = header_end;
        for frame in self.frames {
            if !frame.fits(prior_offset, prefix_end)
                || frame.seq <= prior_seq
                || frame.seq > state.sequence()
                || state.require_lane(&frame.lane).is_err()
                || state.validate_unused_id(&frame.id).is_ok()
                || (frame.retired && !frame.append)
            {
                return None;
            }
            prior_seq = frame.seq;
            prior_offset = frame.offset + frame.length as u64 + 1;
            let lane = frame_symbol(&mut state.last_frame_lane, &frame.lane);
            let run = frame_symbol(&mut state.last_frame_run, &frame.run);
            state.cold.push(frame.bind(reader, lane, run));
        }
        Some(state)
    }
}

// JSON arrays would expand each digest to 32 separate numbers. Base64 stays
// bounded and lets serde read each digest as a single string.
mod digest {
    use base64::{engine::general_purpose::STANDARD, Engine};
    use serde::{de::Error, Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(digest: &[u8; 32], out: S) -> Result<S::Ok, S::Error> {
        let mut encoded = [0u8; 44];
        STANDARD
            .encode_slice(digest, &mut encoded)
            .map_err(serde::ser::Error::custom)?;
        out.serialize_str(std::str::from_utf8(&encoded).expect("base64 is ASCII"))
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(input: D) -> Result<[u8; 32], D::Error> {
        let encoded = <&str>::deserialize(input)?;
        let mut digest = [0u8; 32];
        if STANDARD
            .decode_slice(encoded, &mut digest)
            .map_err(D::Error::custom)?
            != digest.len()
        {
            return Err(D::Error::custom("invalid location digest size"));
        }
        Ok(digest)
    }
}
