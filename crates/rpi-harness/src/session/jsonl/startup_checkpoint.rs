//! Disposable native startup image. The fixed-size envelope lets the scanner
//! prove the source prefix before reading or decoding the state image.
use super::{FileSystem, Sha256};
use crate::session::jsonl::cold_frames::startup_image::StartupImage;
use std::{io, sync::Arc};

pub(in crate::session::jsonl) const TEMP_SUFFIX: &str = ".startup-checkpoint.tmp";

// Bump when replay/reducer semantics or the image representation changes.
const MAGIC: [u8; 8] = *b"RPISTRT1";
const BOUNDARY_LEN: usize = 56;
const HEADER_LEN: usize = BOUNDARY_LEN + 32;
// Runtime records can retain substantial tool arguments and deferred writes.
const MAX_IMAGE: usize = 128 * 1024 * 1024;
pub(in crate::session::jsonl) const SUFFIX: &str = ".startup-checkpoint";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Boundary {
    pub(super) bytes: u64,
    pub(super) lines: usize,
    pub(super) digest: [u8; 32],
}

impl Boundary {
    fn encode(self) -> [u8; BOUNDARY_LEN] {
        let mut bytes = [0; BOUNDARY_LEN];
        bytes[..8].copy_from_slice(&MAGIC);
        bytes[8..16].copy_from_slice(&self.bytes.to_le_bytes());
        bytes[16..24].copy_from_slice(&(self.lines as u64).to_le_bytes());
        bytes[24..].copy_from_slice(&self.digest);
        bytes
    }

    fn decode(bytes: &[u8; HEADER_LEN]) -> Option<Self> {
        if bytes[..8] != MAGIC {
            return None;
        }
        Some(Self {
            bytes: u64::from_le_bytes(bytes[8..16].try_into().ok()?),
            lines: u64::from_le_bytes(bytes[16..24].try_into().ok()?)
                .try_into()
                .ok()?,
            digest: bytes[24..BOUNDARY_LEN].try_into().ok()?,
        })
    }
}

#[derive(Clone, Copy)]
pub(super) struct Update {
    pub(super) boundary: Boundary,
    pub(super) reused: Option<Boundary>,
}

impl Update {
    pub(super) fn should_publish(self) -> bool {
        self.reused.is_none_or(|prior| {
            self.boundary.bytes - prior.bytes >= 4 * 1024 * 1024
                || self.boundary.lines - prior.lines >= 4096
        })
    }
}

pub(super) struct Checkpoint {
    pub(super) boundary: Boundary,
    reader: std::fs::File,
    length: usize,
    header: [u8; HEADER_LEN],
}

impl Checkpoint {
    pub(super) async fn open(fs: &Arc<dyn FileSystem>, path: &str) -> Option<Self> {
        let path = fs.native_file_path(path)?;
        tokio::task::spawn_blocking(move || {
            use std::io::Read;
            let mut reader = native::open_image(&path).ok()?;
            let length: usize = reader.metadata().ok()?.len().try_into().ok()?;
            if length <= HEADER_LEN || length > HEADER_LEN + MAX_IMAGE {
                return None;
            }
            let mut header = [0; HEADER_LEN];
            reader.read_exact(&mut header).ok()?;
            let boundary = Boundary::decode(&header)?;
            if boundary.lines == 0 || boundary.lines as u64 > boundary.bytes {
                return None;
            }
            Some(Self {
                boundary,
                reader,
                length,
                header,
            })
        })
        .await
        .ok()?
    }

    pub(super) async fn image(mut self) -> Option<StartupImage> {
        tokio::task::spawn_blocking(move || {
            use std::io::Read;
            let mut bytes = vec![0; self.length - HEADER_LEN];
            self.reader.read_exact(&mut bytes).ok()?;
            let mut hash = Sha256::new();
            hash.update(&self.header[..BOUNDARY_LEN]);
            hash.update(&bytes);
            if hash.finalize() != self.header[BOUNDARY_LEN..] {
                return None;
            }
            serde_json::from_slice(&bytes).ok()
        })
        .await
        .ok()?
    }
}

struct BoundedImage(Vec<u8>);

impl io::Write for BoundedImage {
    fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        if input.len() > (HEADER_LEN + MAX_IMAGE) - self.0.len() {
            return Err(io::Error::other(
                "startup checkpoint image exceeds byte limit",
            ));
        }
        self.0.extend_from_slice(input);
        Ok(input.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) async fn publish(
    fs: &Arc<dyn FileSystem>,
    path: &str,
    boundary: Boundary,
    image: StartupImage,
) {
    let Some(path) = fs.native_file_path(path) else {
        return;
    };
    let _ = tokio::task::spawn_blocking(move || {
        let mut output = BoundedImage(vec![0; HEADER_LEN]);
        if let Err(error) = serde_json::to_writer(&mut output, &image) {
            tracing::debug!(%error, "could not encode session startup checkpoint");
            return;
        }
        let mut bytes = output.0;
        bytes[..BOUNDARY_LEN].copy_from_slice(&boundary.encode());
        let mut hash = Sha256::new();
        hash.update(&bytes[..BOUNDARY_LEN]);
        hash.update(&bytes[HEADER_LEN..]);
        bytes[BOUNDARY_LEN..HEADER_LEN].copy_from_slice(&hash.finalize());
        if let Err(error) = native::publish(&path, &bytes) {
            tracing::debug!(%error, "could not publish session startup checkpoint");
        }
    })
    .await;
}

#[cfg(unix)]
mod native {
    use super::*;
    use rustix::fs::{open, openat, renameat, unlinkat, AtFlags, Mode, OFlags};
    use std::{ffi::OsString, fs::File, io::Write, os::unix::fs::MetadataExt, path::Path};

    fn address(path: &Path, suffix: &str) -> io::Result<(File, OsString)> {
        let parent = path.parent().ok_or(io::ErrorKind::InvalidInput)?;
        let mut name = path
            .file_name()
            .ok_or(io::ErrorKind::InvalidInput)?
            .to_os_string();
        name.push(suffix);
        let directory = File::from(open(
            parent,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?);
        let info = directory.metadata()?;
        if info.uid() != rustix::process::geteuid().as_raw() || info.mode() & 0o022 != 0 {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        Ok((directory, name))
    }

    pub(super) fn open_image(path: &Path) -> io::Result<File> {
        let (directory, name) = address(path, SUFFIX)?;
        let file = File::from(openat(
            &directory,
            &name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )?);
        let info = file.metadata()?;
        if !info.is_file()
            || info.uid() != rustix::process::geteuid().as_raw()
            || info.mode() & 0o077 != 0
            || info.nlink() != 1
        {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        Ok(file)
    }

    pub(super) fn publish(path: &Path, bytes: &[u8]) -> io::Result<()> {
        let (directory, name) = address(path, SUFFIX)?;
        let mut temp = path
            .file_name()
            .ok_or(io::ErrorKind::InvalidInput)?
            .to_os_string();
        temp.push(TEMP_SUFFIX);
        match unlinkat(&directory, &temp, AtFlags::empty()) {
            Ok(()) | Err(rustix::io::Errno::NOENT) => {}
            Err(error) => return Err(error.into()),
        }
        let flags =
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let mut file = File::from(openat(&directory, &temp, flags, Mode::RUSR | Mode::WUSR)?);
        let result = (|| {
            file.write_all(bytes)?;
            // It is safe to lose this rebuildable image on power failure.
            renameat(&directory, &temp, &directory, &name)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = unlinkat(&directory, &temp, AtFlags::empty());
        }
        result
    }
}

#[cfg(not(unix))]
mod native {
    use std::{fs::File, io, path::Path};

    pub(super) fn open_image(_: &Path) -> io::Result<File> {
        Err(io::ErrorKind::Unsupported.into())
    }

    pub(super) fn publish(_: &Path, _: &[u8]) -> io::Result<()> {
        Err(io::ErrorKind::Unsupported.into())
    }
}
