use super::*;
use crate::PluginCleanupStatus;
use rpi_plugin_sdk::{EventPayload, EventTag, PluginApi, StablePluginEvent};
use std::sync::Barrier;
use std::time::Duration;

fn fixture(name: &str, shared: bool, initial_count: i32) -> CdylibFixture {
    let marker = if shared {
        "#[no_mangle] pub extern \"C\" fn rpi_plugin_shared_sessions_v1() {}"
    } else {
        ""
    };
    build_cdylib_fixture(
        name,
        &format!(
            r#"
use std::ffi::c_void;
use std::sync::atomic::{{AtomicI32, Ordering}};

#[repr(C)]
struct Event {{
    tag: u32,
    payload: [usize; {payload_words}],
}}

extern "C" fn handle(_: Event, data: *mut c_void) -> i32 {{
    let count = unsafe {{ &*data.cast::<AtomicI32>() }};
    count.fetch_add(1, Ordering::Relaxed) + 1
}}

extern "C" fn begin(_: *mut c_void) {{}}
extern "C" fn finish(data: *mut c_void) -> i32 {{
    drop(unsafe {{ Box::from_raw(data.cast::<AtomicI32>()) }});
    0
}}

{marker}

#[no_mangle]
pub extern "C" fn rpi_plugin_register(api: *const u8) -> i32 {{
    type Lifecycle = extern "C" fn(extern "C" fn(*mut c_void), extern "C" fn(*mut c_void) -> i32, *mut c_void) -> i32;
    type Handler = extern "C" fn(u32, extern "C" fn(Event, *mut c_void) -> i32, *mut c_void) -> i32;
    let lifecycle = unsafe {{ (*api.add({life_offset}).cast::<Option<Lifecycle>>()).unwrap() }};
    let event = unsafe {{ (*api.add({event_offset}).cast::<Option<Handler>>()).unwrap() }};
    let data = Box::into_raw(Box::new(AtomicI32::new({initial_count}))).cast();
    let status = lifecycle(begin, finish, data);
    if status != 0 {{
        finish(data);
        return status;
    }}
    event({tag}, handle, data)
}}
"#,
            payload_words = std::mem::size_of::<EventPayload>() / std::mem::size_of::<usize>(),
            life_offset = std::mem::offset_of!(PluginApi, register_plugin_lifecycle),
            event_offset = std::mem::offset_of!(PluginApi, register_event_handler),
            tag = EventTag::SessionStart as u32,
        ),
    )
}

fn open(images: &SharedPluginImages, path: &Path) -> ExtensionSession {
    let diagnostics = Arc::new(CapturingDiag::default());
    let session = images.load_session(&[], &[path.to_path_buf()], diagnostics.clone(), None, None);
    assert_eq!(session.loaded_paths(), [path], "{:?}", diagnostics.warns);
    session
}

fn handler(session: &ExtensionSession) -> &crate::registry::RegisteredHandler {
    let [handler] = session
        .snapshot()
        .unwrap()
        .handlers_for(EventTag::SessionStart)
    else {
        panic!("expected one handler");
    };
    handler
}

fn count(session: &ExtensionSession) -> i32 {
    let handler = handler(session);
    (handler.handler)(
        StablePluginEvent::empty(EventTag::SessionStart),
        handler.user_data,
    )
}

async fn close(session: ExtensionSession) {
    let [receipt] = session.cleanup_receipts().try_into().expect("one receipt");
    session.begin_shutdown();
    drop(session);
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), receipt.wait())
            .await
            .unwrap(),
        PluginCleanupStatus::Released,
    );
}

#[tokio::test]
async fn shared_image_registers_independent_sessions_across_concurrent_open_and_upgrade() {
    let original = fixture("shared_image", true, 0);
    let replacement = fixture("shared_image_v2", true, 100);
    let images = SharedPluginImages::default();
    let barrier = Barrier::new(2);
    let (first, second) = std::thread::scope(|scope| {
        let open_first = scope.spawn(|| {
            barrier.wait();
            open(&images, &original.path)
        });
        barrier.wait();
        let second = open(&images, &original.path);
        (open_first.join().unwrap(), second)
    });
    let first_handler = handler(&first).handler as usize;
    assert_eq!(first_handler, handler(&second).handler as usize);
    assert_ne!(handler(&first).user_data, handler(&second).user_data);
    assert_eq!(count(&first), 1);
    assert_eq!(count(&first), 2);
    assert_eq!(count(&second), 1);
    close(first).await;
    assert_eq!(count(&second), 2);
    let reopened = open(&images, &original.path);
    assert_eq!(handler(&reopened).handler as usize, first_handler);
    assert_eq!(count(&reopened), 1);
    close(reopened).await;
    close(second).await;
    let reopened = open(&images, &original.path);
    assert_eq!(handler(&reopened).handler as usize, first_handler);
    assert_eq!(count(&reopened), 1);

    fs::rename(&replacement.path, &original.path).unwrap();
    let updated = open(&images, &original.path);
    assert_ne!(handler(&updated).handler as usize, first_handler);
    assert_eq!(count(&updated), 101);
    assert_eq!(count(&reopened), 2);
    close(updated).await;
    close(reopened).await;
}

#[tokio::test]
async fn legacy_plugins_keep_private_images() {
    let legacy = fixture("legacy_shared_host_image", false, 0);
    let images = SharedPluginImages::default();
    let first = open(&images, &legacy.path);
    let second = open(&images, &legacy.path);
    assert_ne!(
        handler(&first).handler as usize,
        handler(&second).handler as usize
    );
    assert_eq!(count(&first), 1);
    assert_eq!(count(&second), 1);
    close(first).await;
    assert_eq!(count(&second), 2);
    close(second).await;
}
