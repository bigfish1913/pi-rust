//! Awaited observer bridge. Neither the executor nor a cancelled waiter owns
//! the lifetime of native code: the actual blocking worker holds call custody.
use crate::host::{AgentSpec, NativeExtensionCustody};
use rpi_ai::{
    provider::ProviderHookFuture, AssistantMessage, Context, Model, ProviderHooks,
    SimpleStreamOptions, SimpleStreamOptionsPatch,
};
use rpi_extensions::{ExtensionProviderContext, ExtensionProviderHooks};
use std::sync::{Arc, Weak};

pub(super) struct NativeProviderHooks {
    owner: Weak<NativeExtensionCustody>,
    context: ExtensionProviderContext,
    serial: Arc<tokio::sync::Mutex<()>>,
}
impl NativeProviderHooks {
    pub(super) fn new(owner: &Arc<NativeExtensionCustody>, spec: &AgentSpec) -> Self {
        Self {
            owner: Arc::downgrade(owner),
            context: ExtensionProviderContext {
                agent_id: Some(spec.id.to_string()),
                session_id: spec.session.as_ref().map(|session| session.id.clone()),
            },
            serial: Arc::new(tokio::sync::Mutex::new(())),
        }
    }
    fn worker<T: Send + 'static>(
        &self,
        action: impl FnOnce(&ExtensionProviderHooks) -> T + Send + 'static,
    ) -> ProviderHookFuture<'static, T> {
        let owner = self.owner.upgrade().expect("Host Provider owner lost");
        let context = self.context.clone();
        let serial = self.serial.clone();
        Box::pin(async move {
            // Acquire before enqueueing, and MOVE the guard into the worker.
            // Cancelling an opening request may drop its hook waiter, but an
            // Aborted after-response must never overtake that real callback.
            let serial = serial.lock_owned().await;
            tokio::task::spawn_blocking(move || {
                let _serial = serial;
                let call = owner.call().expect("Host Provider custody closed");
                let hooks = ExtensionProviderHooks::from_session(call.session())
                    .expect("Host Provider registry missing approved observers")
                    .with_context(context);
                action(&hooks)
                // Hook/keepalive drops before call scope, serial releases last.
            })
            .await
            .expect("Host Provider worker failed")
        })
    }
}
impl ProviderHooks for NativeProviderHooks {
    fn before_request_async<'a>(
        &'a self,
        model: &'a Model,
        context: &'a Context,
        options: &'a SimpleStreamOptions,
    ) -> ProviderHookFuture<'a, Option<SimpleStreamOptionsPatch>> {
        // Only an explicitly approved native observer pays for these snapshots;
        // ordinary Agents retain the borrowed-history/no-hook fast path.
        let model = model.clone();
        let context = context.clone();
        let options = options.clone();
        self.worker(move |hooks| hooks.before_request(&model, &context, &options))
    }
    fn after_response_async<'a>(
        &'a self,
        model: &'a Model,
        message: &'a AssistantMessage,
    ) -> ProviderHookFuture<'a, ()> {
        let model = model.clone();
        let message = message.clone();
        self.worker(move |hooks| hooks.after_response(&model, &message))
    }
}
