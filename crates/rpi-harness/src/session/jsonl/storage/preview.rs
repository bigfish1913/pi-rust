//! Optional bounded transcript index. The history journal supplies every
//! message body, and restore does not consult this index.
use super::*;
use rpi_tools::env::{NativeAppendMetadata, ReadFile};
use serde::Serialize;
use std::{
    fs::Metadata,
    io::{self, Read},
    path::{Path, PathBuf},
    time::SystemTime,
};

const LIMIT: usize = 40;
const MAX_INDEX: usize = 64 * 1024;
#[cfg(unix)]
const MAX_HEADER: usize = 64 * 1024;
const MAX_ROWS: usize = 8 * 1024 * 1024;
const VERSION: u32 = 2;
pub(in crate::session::jsonl) const SUFFIX: &str = ".preview.json";
pub(in crate::session::jsonl) const TEMP_SUFFIX: &str = ".preview.json.tmp";
pub(in crate::session::jsonl) const STAMP_SUFFIX: &str = ".preview.stamp";

#[derive(Clone)]
pub(in crate::session::jsonl) struct Source {
    path: PathBuf,
    stamp: SourceStamp,
    pub attempted: bool,
    span_repair: Option<SpanRepair>,
    header_rewrite: Option<HeaderRewrite>,
    published: Option<Arc<PublishedIndex>>,
}

#[derive(Clone)]
struct SpanRepair {
    last_old_sequence: u64,
    skipped: Arc<[(u64, u64)]>,
}

pub(in crate::session::jsonl) struct RepairLayout {
    length: u64,
    skipped: Vec<(u64, u64)>,
}

impl RepairLayout {
    pub fn from_spans(spans: &[(u64, u64)]) -> Option<Self> {
        let mut length = 0;
        let mut removed = 0;
        let mut skipped = Vec::new();
        for &(offset, count) in spans {
            let gap = offset.checked_sub(length)?;
            if gap != removed {
                skipped.push((offset, gap));
                removed = gap;
            }
            length = length.checked_add(count)?.checked_add(1)?;
        }
        Some(Self { length, skipped })
    }
}

#[derive(Clone, Copy)]
struct HeaderRewrite {
    last_old_sequence: u64,
    old_len: u64,
    new_len: u64,
}

impl Source {
    pub fn replay(path: PathBuf, reader: &dyn ReadFile, length: u64) -> Option<Self> {
        let stamp = SourceStamp::new(&reader.native_metadata()?).ok()?;
        if stamp.length != length {
            return None;
        }
        let source = Self {
            path,
            stamp,
            attempted: false,
            span_repair: None,
            header_rewrite: None,
            published: None,
        };
        source.matches(reader).then_some(source)
    }

    pub fn created(path: PathBuf, expected: &[u8]) -> Option<Self> {
        let (directory, name) = native::address(&path).ok()?;
        let (mut file, stamp) = directory.open(name).ok()?;
        if stamp.length != expected.len() as u64 {
            return None;
        }
        let mut actual = vec![0; expected.len()];
        file.read_exact(&mut actual).ok()?;
        if actual != expected || SourceStamp::new(&file.metadata().ok()?).ok()? != stamp {
            return None;
        }
        let source = Self {
            path,
            stamp,
            attempted: false,
            span_repair: None,
            header_rewrite: None,
            published: None,
        };
        source.is_current().then_some(source)
    }

    pub fn matches(&self, reader: &dyn ReadFile) -> bool {
        reader
            .native_metadata()
            .and_then(|meta| SourceStamp::new(&meta).ok())
            .as_ref()
            == Some(&self.stamp)
            && self.is_current()
    }

    pub fn is_current(&self) -> bool {
        native::address(&self.path)
            .and_then(|(directory, name)| directory.open(name))
            .is_ok_and(|(_, stamp)| self.stamp == stamp)
    }

    pub fn appended(
        &self,
        receipt: &AppendReceipt,
        metadata: &NativeAppendMetadata,
    ) -> Option<Self> {
        let before = SourceStamp::new(&metadata.before).ok()?;
        let after = SourceStamp::new(&metadata.after).ok()?;
        (self.stamp == before
            && receipt.offset == before.length
            && before.length.checked_add(receipt.length as u64) == Some(after.length))
        .then(|| Self {
            path: self.path.clone(),
            stamp: after,
            attempted: false,
            span_repair: self.span_repair.clone(),
            header_rewrite: self.header_rewrite,
            published: self.published.clone(),
        })
    }

    pub fn after_repair(
        &self,
        reader: &dyn ReadFile,
        layout: RepairLayout,
        last_old_sequence: u64,
    ) -> Option<Self> {
        let mut source = Self::replay(self.path.clone(), reader, layout.length)?;
        if !layout.skipped.is_empty() {
            source.span_repair = Some(SpanRepair {
                last_old_sequence,
                skipped: layout.skipped.into(),
            });
        }
        Some(source)
    }

    pub fn final_newline_layout(&self) -> Option<RepairLayout> {
        Some(RepairLayout {
            length: self.stamp.length.checked_add(1)?,
            skipped: Vec::new(),
        })
    }

    pub fn after_header_rewrite(
        &self,
        reader: &dyn ReadFile,
        last_old_sequence: u64,
        old_len: u64,
        new_len: u64,
    ) -> Option<Self> {
        let length = self
            .stamp
            .length
            .checked_sub(old_len)?
            .checked_add(new_len)?;
        let mut source = Self::replay(self.path.clone(), reader, length)?;
        source.span_repair = self.span_repair.clone();
        source.header_rewrite = Some(HeaderRewrite {
            last_old_sequence,
            old_len,
            new_len,
        });
        Some(source)
    }

    pub fn publish(mut self, index: PreviewIndex) -> io::Result<Self> {
        self.published = Some(native::publish(&self.path, index, self.published)?);
        self.attempted = true;
        Ok(self)
    }
}

pub(in crate::session::jsonl) fn native_path(
    fs: &dyn rpi_tools::env::FileSystem,
    path: &str,
) -> Option<PathBuf> {
    let path = fs.native_file_path(path)?;
    let uuid = path.file_stem()?.to_str()?.rsplit_once('_')?.1;
    uuid::Uuid::parse_str(uuid).ok()?;
    (path.extension()?.to_str()? == "jsonl").then_some(path)
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct SourceStamp {
    length: u64,
    modified: SystemTime,
    created: Option<SystemTime>,
    #[cfg(unix)]
    inode: (u64, u64),
    #[cfg(unix)]
    changed: (i64, i64),
}

impl SourceStamp {
    fn new(metadata: &Metadata) -> io::Result<Self> {
        if !metadata.is_file() {
            return Err(invalid("preview source is not a regular file"));
        }
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Ok(Self {
            length: metadata.len(),
            modified: metadata.modified()?,
            created: metadata.created().ok(),
            #[cfg(unix)]
            inode: (metadata.dev(), metadata.ino()),
            #[cfg(unix)]
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        })
    }
}

#[derive(Deserialize, PartialEq, Serialize)]
struct Row {
    offset: u64,
    length: usize,
    digest: [u8; 32],
    id: String,
    seq: u64,
    // Only the top-level message correction affects the RPC projection.
    live_null: Option<NullField>,
}

#[derive(Clone, Copy, Deserialize, PartialEq, Serialize)]
enum NullField {
    ToolDetails,
    DeferredData,
}

impl Row {
    fn from_payload(payload: &JsonlPayload) -> Option<Self> {
        let JsonlPayload::Cold {
            source, live_nulls, ..
        } = payload
        else {
            return None;
        };
        Some(Self {
            offset: source.offset,
            length: source.length,
            digest: source.digest,
            id: source.id.clone(),
            seq: source.seq,
            live_null: live_nulls.message.map(|field| match field {
                MessageNull::ToolDetails => NullField::ToolDetails,
                MessageNull::DeferredData => NullField::DeferredData,
            }),
        })
    }

    fn decode(&self, bytes: &[u8]) -> io::Result<Entry> {
        if Sha256::digest(bytes) != self.digest {
            return Err(invalid("preview row digest changed"));
        }
        let line = std::str::from_utf8(bytes).map_err(io::Error::other)?;
        let SessionMutation::Entry { mut entry, .. } =
            parse_mutation(line).map_err(io::Error::other)?
        else {
            return Err(invalid("preview row is not a transcript entry"));
        };
        if entry.id() != self.id || entry.seq() != self.seq {
            return Err(invalid("preview row identity changed"));
        }
        match &mut entry {
            Entry::Message(entry) => {
                if let Some(field) = self.live_null {
                    match field {
                        NullField::ToolDetails => MessageNull::ToolDetails,
                        NullField::DeferredData => MessageNull::DeferredData,
                    }
                    .apply(&mut entry.message);
                }
            }
            Entry::Compaction(_) if self.live_null.is_none() => {}
            _ => return Err(invalid("preview row is not a transcript entry")),
        }
        Ok(entry)
    }
}

#[derive(Deserialize, Serialize)]
pub(in crate::session::jsonl) struct PreviewIndex {
    version: u32,
    session_id: String,
    source: SourceStamp,
    rows: Vec<Row>,
    has_more: bool,
}

#[cfg(unix)]
struct PublishedIndex {
    index: PreviewIndex,
    stamp: SourceStamp,
    digest: [u8; 32],
}

#[cfg(not(unix))]
enum PublishedIndex {}

#[cfg(unix)]
#[derive(Deserialize, Eq, PartialEq, Serialize)]
struct PreviewStamp {
    source: SourceStamp,
    index_digest: [u8; 32],
}

impl ResidentState {
    pub(in crate::session::jsonl) fn preview_index(
        &self,
        session_id: &str,
        source: &Source,
    ) -> Option<PreviewIndex> {
        let leaf = self.reducer.require_lane("main").ok()??;
        let mut next = self.reducer.nearest_transcript(&leaf);
        let mut rows = Vec::with_capacity(LIMIT);
        while rows.len() < LIMIT {
            let Some(index) = next else {
                break;
            };
            let entry = self.reducer.entry_meta(index)?;
            let mut row = Row::from_payload(self.reducer.payload(index)?)?;
            // Recovered entries still use the original reader. Translate
            // their offsets across any removed spans before the header shift.
            if let Some(repair) = source
                .span_repair
                .as_ref()
                .filter(|repair| row.seq <= repair.last_old_sequence)
            {
                let gaps = repair
                    .skipped
                    .partition_point(|(offset, _)| *offset <= row.offset);
                if gaps > 0 {
                    row.offset = row.offset.checked_sub(repair.skipped[gaps - 1].1)?;
                }
            }
            // Progress migration copies the body verbatim. The live cold
            // entries still point to the old file through their open handle.
            if let Some(rewrite) = source
                .header_rewrite
                .filter(|rewrite| row.seq <= rewrite.last_old_sequence)
            {
                row.offset = row
                    .offset
                    .checked_sub(rewrite.old_len)?
                    .checked_add(rewrite.new_len)?;
            }
            rows.push(row);
            next = entry
                .parent_id()
                .and_then(|id| self.reducer.nearest_transcript(id));
        }
        if rows.is_empty() {
            return None;
        }
        rows.reverse();
        Some(PreviewIndex {
            version: VERSION,
            session_id: session_id.to_owned(),
            source: source.stamp.clone(),
            rows,
            has_more: next.is_some(),
        })
    }
}

impl PreviewIndex {
    #[cfg(unix)]
    fn same_projection(&self, other: &Self) -> bool {
        self.version == other.version
            && self.session_id == other.session_id
            && self.rows == other.rows
            && self.has_more == other.has_more
    }

    fn validate(&self, header_len: u64) -> io::Result<()> {
        let valid = self.version == VERSION
            && self.rows.len() <= LIMIT
            && (!self.has_more || self.rows.len() == LIMIT);
        let mut remaining = MAX_ROWS;
        let mut end = header_len;
        let mut sequence = 0;
        if valid
            && self.rows.iter().all(|row| {
                if row.offset < end || row.seq <= sequence || row.length == 0 {
                    return false;
                }
                let Some(row_end) = row.offset.checked_add(row.length as u64) else {
                    return false;
                };
                let Some(budget) = remaining.checked_sub(row.length) else {
                    return false;
                };
                remaining = budget;
                end = row_end;
                sequence = row.seq;
                end <= self.source.length
            })
        {
            return Ok(());
        }
        Err(invalid("invalid or oversized session preview index"))
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

pub(in crate::session::jsonl) fn read(
    root: &Path,
    cwd: &Path,
    session_id: &str,
) -> io::Result<(Vec<Entry>, bool)> {
    uuid::Uuid::parse_str(session_id).map_err(io::Error::other)?;
    native::read(root, cwd, session_id)
}

#[cfg(unix)]
mod native {
    use super::*;
    use crate::session::jsonl::{codec::parse_header, jsonl_session_directory_name};
    use rustix::fs::{open, openat, renameat, unlinkat, AtFlags, Dir, Mode, OFlags};
    use std::{
        ffi::{OsStr, OsString},
        fs::{self, File},
        io::{BufRead, BufReader, Seek, SeekFrom, Write},
        os::unix::{ffi::OsStrExt, fs::MetadataExt},
        path::Component,
    };

    const DIRECTORY: OFlags = OFlags::RDONLY
        .union(OFlags::DIRECTORY)
        .union(OFlags::CLOEXEC)
        .union(OFlags::NOFOLLOW);
    const INPUT: OFlags = OFlags::RDONLY
        .union(OFlags::CLOEXEC)
        .union(OFlags::NOFOLLOW)
        .union(OFlags::NONBLOCK);

    pub(super) struct Directory(File);

    impl Directory {
        fn absolute(path: &Path) -> io::Result<Self> {
            let mut directory = Self(open("/", DIRECTORY, Mode::empty())?.into());
            for component in path.components() {
                match component {
                    Component::RootDir => {}
                    Component::Normal(name) => directory = directory.child(name)?,
                    _ => return Err(invalid("session preview directory is not canonical")),
                }
            }
            Ok(directory)
        }

        fn child(&self, name: &OsStr) -> io::Result<Self> {
            Ok(Self(
                openat(&self.0, name, DIRECTORY, Mode::empty())?.into(),
            ))
        }

        fn is_current(&self, path: &Path) -> io::Result<bool> {
            fn identity(file: &File) -> io::Result<(u64, u64)> {
                let metadata = file.metadata()?;
                Ok((metadata.dev(), metadata.ino()))
            }
            Ok(identity(&self.0)? == identity(&Self::absolute(path)?.0)?)
        }

        pub(super) fn open(&self, name: &OsStr) -> io::Result<(File, SourceStamp)> {
            let file = File::from(openat(&self.0, name, INPUT, Mode::empty())?);
            let stamp = SourceStamp::new(&file.metadata()?)?;
            Ok((file, stamp))
        }
    }

    pub(super) fn address(path: &Path) -> io::Result<(Directory, &OsStr)> {
        let parent = canonical_parent(path)?;
        let name = path
            .file_name()
            .ok_or_else(|| invalid("missing preview source filename"))?;
        Ok((Directory::absolute(&parent)?, name))
    }

    fn canonical_parent(path: &Path) -> io::Result<PathBuf> {
        fs::canonicalize(
            path.parent()
                .ok_or_else(|| invalid("missing preview source directory"))?,
        )
    }

    fn sidecar(name: &OsStr, suffix: &str) -> OsString {
        let mut name = name.to_os_string();
        name.push(suffix);
        name
    }

    fn read_bounded(mut file: File, limit: usize) -> io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take((limit + 1) as u64)
            .read_to_end(&mut bytes)?;
        if bytes.len() > limit {
            return Err(invalid("session preview sidecar exceeds byte limit"));
        }
        Ok(bytes)
    }

    fn read_stamp(directory: &Directory, name: &OsStr) -> io::Result<PreviewStamp> {
        let (file, _) = directory.open(&sidecar(name, STAMP_SUFFIX))?;
        serde_json::from_slice(&read_bounded(file, 1024)?).map_err(io::Error::other)
    }

    pub(super) fn publish(
        path: &Path,
        index: PreviewIndex,
        cached: Option<Arc<PublishedIndex>>,
    ) -> io::Result<Arc<PublishedIndex>> {
        index.validate(1)?;
        let parent = canonical_parent(path)?;
        let directory = Directory::absolute(&parent)?;
        let name = path
            .file_name()
            .ok_or_else(|| invalid("missing preview source filename"))?;
        if directory.open(name)?.1 != index.source {
            return Err(invalid("session changed before preview publication"));
        }
        let output = sidecar(name, SUFFIX);
        let already_loaded = cached.is_some();
        let existing = match cached {
            Some(cached)
                if cached.index.same_projection(&index)
                    && directory
                        .open(&output)
                        .is_ok_and(|(_, stamp)| stamp == cached.stamp) =>
            {
                Some(cached)
            }
            Some(_) => None,
            None => directory.open(&output).ok().and_then(|(file, stamp)| {
                let bytes = read_bounded(file, MAX_INDEX).ok()?;
                let previous: PreviewIndex = serde_json::from_slice(&bytes).ok()?;
                previous.same_projection(&index).then(|| {
                    Arc::new(PublishedIndex {
                        index: previous,
                        stamp,
                        digest: Sha256::digest(&bytes),
                    })
                })
            }),
        };
        if let Some(existing) = existing {
            if existing.index.source != index.source {
                // Keep the row index immutable while advancing its freshness
                // proof. Readers bind the proof to the exact index bytes.
                let proof = PreviewStamp {
                    source: index.source,
                    index_digest: existing.digest,
                };
                if already_loaded || !read_stamp(&directory, name).is_ok_and(|old| old == proof) {
                    let bytes = serde_json::to_vec(&proof).map_err(io::Error::other)?;
                    publish_sidecar(
                        &directory,
                        &parent,
                        name,
                        &sidecar(name, STAMP_SUFFIX),
                        &proof.source,
                        &bytes,
                    )?;
                }
            }
            return Ok(existing);
        }
        let bytes = serde_json::to_vec(&index).map_err(io::Error::other)?;
        if bytes.len() > MAX_INDEX {
            return Err(invalid("session preview index exceeds byte limit"));
        }
        let stamp = publish_sidecar(&directory, &parent, name, &output, &index.source, &bytes)?;
        Ok(Arc::new(PublishedIndex {
            index,
            stamp,
            digest: Sha256::digest(&bytes),
        }))
    }

    fn publish_sidecar(
        directory: &Directory,
        parent: &Path,
        name: &OsStr,
        output: &OsStr,
        source: &SourceStamp,
        bytes: &[u8],
    ) -> io::Result<SourceStamp> {
        let temp = sidecar(name, TEMP_SUFFIX);
        match unlinkat(&directory.0, &temp, AtFlags::empty()) {
            Ok(()) | Err(rustix::io::Errno::NOENT) => {}
            Err(error) => return Err(error.into()),
        }
        let flags =
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW;
        let mut file = File::from(openat(&directory.0, &temp, flags, Mode::RUSR | Mode::WUSR)?);
        let result = (|| {
            file.write_all(bytes)?;
            if &directory.open(name)?.1 != source || !directory.is_current(parent)? {
                return Err(invalid("session changed during preview publication"));
            }
            // The journal already crossed its durability boundary. The optional
            // index can be lost on power failure and is safe to regenerate.
            renameat(&directory.0, &temp, &directory.0, output)?;
            if directory.open(name).is_err() {
                let _ = unlinkat(&directory.0, output, AtFlags::empty());
                return Err(invalid("session disappeared during preview publication"));
            }
            SourceStamp::new(&file.metadata()?)
        })();
        if result.is_err() {
            let _ = unlinkat(&directory.0, &temp, AtFlags::empty());
        }
        result
    }

    pub(super) fn read(
        root: &Path,
        cwd: &Path,
        session_id: &str,
    ) -> io::Result<(Vec<Entry>, bool)> {
        let root = fs::canonicalize(root)?;
        let cwd = fs::canonicalize(cwd)?;
        let project = jsonl_session_directory_name(&cwd.to_string_lossy());
        let directory = Directory::absolute(&root)?.child(OsStr::new(&project))?;
        let suffix = format!("_{session_id}.jsonl");
        let mut candidate = None;
        for entry in Dir::read_from(&directory.0)? {
            let entry = entry?;
            let name = entry.file_name().to_bytes();
            if name.ends_with(suffix.as_bytes()) {
                if candidate.is_some() {
                    return Err(invalid("ambiguous session identity"));
                }
                candidate = Some(OsStr::from_bytes(name).to_os_string());
            }
        }
        let name = candidate.ok_or_else(|| invalid("session is unavailable"))?;
        let (index, _) = directory.open(&sidecar(&name, SUFFIX))?;
        let index_bytes = read_bounded(index, MAX_INDEX)?;
        let mut index: PreviewIndex =
            serde_json::from_slice(&index_bytes).map_err(io::Error::other)?;
        let (file, stamp) = directory.open(&name)?;
        if index.session_id != session_id {
            return Err(invalid("session preview identity changed"));
        }
        if index.source != stamp {
            let proof_matches = read_stamp(&directory, &name).is_ok_and(|proof| {
                proof.source == stamp && proof.index_digest == Sha256::digest(&index_bytes)
            });
            if !proof_matches {
                return Err(invalid("session preview is stale"));
            }
            index.source = stamp.clone();
        }

        let mut reader = BufReader::new(file);
        let mut header_bytes = Vec::new();
        reader
            .by_ref()
            .take(MAX_HEADER as u64)
            .read_until(b'\n', &mut header_bytes)?;
        if header_bytes.pop() != Some(b'\n') {
            return Err(invalid("missing or oversized session preview header"));
        }
        let header = parse_header(std::str::from_utf8(&header_bytes).map_err(io::Error::other)?)
            .map_err(io::Error::other)?;
        if header.id != session_id || fs::canonicalize(&header.cwd)? != cwd {
            return Err(invalid("session preview identity or project mismatch"));
        }
        let progress = sidecar(&name, ".progress.jsonl");
        let progress_stamp = header
            .progress_format
            .is_some()
            .then(|| directory.open(&progress))
            .transpose()?
            .map(|(_, stamp)| stamp);
        index.validate(header_bytes.len() as u64 + 1)?;
        let mut reader = reader.into_inner();
        let mut entries = Vec::with_capacity(index.rows.len());
        let mut bytes = Vec::new();
        for row in index.rows {
            bytes.resize(row.length, 0);
            reader.seek(SeekFrom::Start(row.offset))?;
            reader.read_exact(&mut bytes)?;
            entries.push(row.decode(&bytes)?);
        }
        if SourceStamp::new(&reader.metadata()?)? != stamp
            || directory.open(&name)?.1 != stamp
            || !directory.is_current(&root.join(project))?
            || (progress_stamp.is_some()
                && directory.open(&progress).ok().map(|(_, stamp)| stamp) != progress_stamp)
        {
            return Err(invalid("session changed during preview"));
        }
        Ok((entries, index.has_more))
    }
}

#[cfg(not(unix))]
mod native {
    use super::*;
    use std::{ffi::OsStr, fs::File};

    pub(super) struct Directory;

    impl Directory {
        pub(super) fn open(&self, _: &OsStr) -> io::Result<(File, SourceStamp)> {
            Err(io::ErrorKind::Unsupported.into())
        }
    }

    pub(super) fn address(_: &Path) -> io::Result<(Directory, &OsStr)> {
        Err(io::ErrorKind::Unsupported.into())
    }

    pub(super) fn publish(
        _: &Path,
        _: PreviewIndex,
        _: Option<Arc<PublishedIndex>>,
    ) -> io::Result<Arc<PublishedIndex>> {
        Err(io::ErrorKind::Unsupported.into())
    }

    pub(super) fn read(_: &Path, _: &Path, _: &str) -> io::Result<(Vec<Entry>, bool)> {
        Err(io::ErrorKind::Unsupported.into())
    }
}
