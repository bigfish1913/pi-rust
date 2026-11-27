//! Library images owned by one local Host. Each registration still owns its
//! own loader reference, callback context, and shutdown receipt.

use super::{open_library, PluginLoadError};
use libloading::Library;
use std::collections::HashMap;
use std::ffi::CStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

/// Keeps opted-in library images warm across sessions of a local Host.
#[derive(Default)]
pub struct SharedPluginImages {
    images: Mutex<HashMap<PathBuf, Arc<ImageSlot>>>,
}

type ImageSlot = Mutex<Option<CachedImage>>;

struct CachedImage {
    version: FileVersion,
    shared: Option<Arc<SharedImage>>,
}

struct SharedImage {
    module: PathBuf,
    // Close the handle before removing the file.
    _library: Library,
    _stage: TempDir,
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct FileVersion {
    device: u64,
    inode: u64,
    size: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

impl FileVersion {
    fn read(path: &Path) -> std::io::Result<Self> {
        let metadata = fs::metadata(path)?;
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            size: metadata.size(),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        })
    }
}

impl SharedPluginImages {
    pub(super) fn open(
        &self,
        path: &Path,
        identity: &Path,
    ) -> Result<(Library, Arc<dyn Send + Sync>), PluginLoadError> {
        let stage_error = |source| PluginLoadError::Stage {
            path: path.to_path_buf(),
            source,
        };
        let slot = self
            .images
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(identity.to_path_buf())
            .or_default()
            .clone();
        // One cold mapping per version, even if multiple sessions open at
        // once. Registrations and library destructors run outside this lock.
        let mut entry = slot.lock().unwrap_or_else(|e| e.into_inner());
        let version = FileVersion::read(identity).map_err(stage_error)?;
        if let Some(cached) = entry.as_ref().filter(|cached| cached.version == version) {
            let shared = cached.shared.clone();
            drop(entry);
            return match shared {
                Some(image) => Ok((open_library(path, &image.module)?, image)),
                None => {
                    let (stage, module) = stage_library(path, identity, version)?;
                    Ok((open_library(path, &module)?, Arc::new(stage)))
                }
            };
        }

        let (stage, module) = stage_library(path, identity, version)?;
        let library = open_library(path, &module)?;
        let (retired, opened) = if supports_shared_sessions(&library, &module) {
            let image = Arc::new(SharedImage {
                module,
                _library: library,
                _stage: stage,
            });
            let retired = entry.replace(CachedImage {
                version,
                shared: Some(image.clone()),
            });
            drop(entry);
            let opened = open_library(path, &image.module)
                .map(|library| (library, image as Arc<dyn Send + Sync>));
            (retired, opened)
        } else {
            let retired = entry.replace(CachedImage {
                version,
                shared: None,
            });
            drop(entry);
            (
                retired,
                Ok((library, Arc::new(stage) as Arc<dyn Send + Sync>)),
            )
        };
        drop(retired);
        opened
    }
}

fn stage_library(
    path: &Path,
    identity: &Path,
    version: FileVersion,
) -> Result<(TempDir, PathBuf), PluginLoadError> {
    let stage_error = |source| PluginLoadError::Stage {
        path: path.to_path_buf(),
        source,
    };
    let stage = tempfile::Builder::new()
        .prefix("rpi-host-extensions-")
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir()
        .map_err(stage_error)?;
    let name = path.file_name().ok_or_else(|| {
        stage_error(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "extension has no filename",
        ))
    })?;
    let module = stage.path().join(name);
    fs::copy(identity, &module).map_err(stage_error)?;
    if FileVersion::read(identity).ok() != Some(version) {
        return Err(stage_error(std::io::Error::other(
            "extension changed while it was being staged",
        )));
    }
    Ok((stage, module))
}

fn supports_shared_sessions(library: &Library, module: &Path) -> bool {
    // dlsym can also find symbols in dependencies. Only an export from this
    // staged image is allowed to opt the extension into shared registration.
    let Ok(marker) =
        (unsafe { library.get::<extern "C" fn()>(rpi_plugin_sdk::SHARED_SESSIONS_SYMBOL_V1) })
    else {
        return false;
    };
    let mut info = std::mem::MaybeUninit::<libc::Dl_info>::uninit();
    // SAFETY: the function address belongs to a live library, and dladdr
    // initializes info on success. Its C string is valid while the image lives.
    let pathname = unsafe {
        if libc::dladdr((*marker as *const ()).cast(), info.as_mut_ptr()) == 0 {
            return false;
        }
        let info = info.assume_init();
        if info.dli_fname.is_null() {
            return false;
        }
        CStr::from_ptr(info.dli_fname)
    };
    let image = Path::new(std::ffi::OsStr::from_bytes(pathname.to_bytes()));
    image == module
        || matches!((image.canonicalize(), module.canonicalize()), (Ok(image), Ok(module)) if image == module)
}
