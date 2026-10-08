use crate::platform::{Requests, Update};
use jni::{
    JValue, JavaVM,
    errors::Result,
    jni_sig, jni_str,
    objects::{JIntArray, JObject, JString},
    refs::Global,
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

#[derive(Default)]
pub struct Bridge {
    pending: Arc<AtomicBool>,
    update: Arc<Mutex<Option<Update>>>,
}
impl Bridge {
    pub const AVAILABLE: bool = true;
    pub fn poll(&mut self, requests: &mut Requests) -> Option<Update> {
        if !self.pending.swap(true, Ordering::AcqRel) {
            let app = dioxus_native::current_android_app();
            let owner = app.clone();
            let pending = self.pending.clone();
            let update = self.update.clone();
            let request = std::mem::take(requests);
            requests.streaming = request.streaming;
            app.run_on_java_main_thread(Box::new(move || {
                // SAFETY: the captured AndroidApp owns the VM and activity reference.
                let vm = unsafe { JavaVM::from_raw(owner.vm_as_ptr().cast()) };
                let result = vm.attach_current_thread(|env| -> Result<Update> {
                    let raw = owner.activity_as_ptr().cast();
                    // SAFETY: borrow the activity global while its AndroidApp owner is captured.
                    let activity = unsafe { env.as_cast_raw::<Global<JObject>>(&raw)? };
                    env.call_method(
                        &activity,
                        jni_str!("setStreaming"),
                        jni_sig!("(Z)V"),
                        &[JValue::Bool(request.streaming)],
                    )?;
                    if request.background {
                        env.call_method(
                            &activity,
                            jni_str!("moveTaskToBack"),
                            jni_sig!("(Z)Z"),
                            &[JValue::Bool(true)],
                        )?;
                    }
                    if request.scan {
                        env.call_method(
                            &activity,
                            jni_str!("scanPairingCode"),
                            jni_sig!("()V"),
                            &[],
                        )?;
                    }
                    let link = env
                        .call_method(
                            &activity,
                            jni_str!("takePairingLink"),
                            jni_sig!("(Z)Ljava/lang/String;"),
                            &[JValue::Bool(request.paste)],
                        )?
                        .l()?;
                    let link = if link.is_null() {
                        None
                    } else {
                        Some(env.cast_local::<JString>(link)?.try_to_string(env)?)
                    };
                    let back = env
                        .call_method(&activity, jni_str!("takeBackRequest"), jni_sig!("()Z"), &[])?
                        .z()?;
                    let name = env
                        .call_method(
                            &activity,
                            jni_str!("deviceName"),
                            jni_sig!("()Ljava/lang/String;"),
                            &[],
                        )?
                        .l()?;
                    let name = env.cast_local::<JString>(name)?.try_to_string(env)?;
                    let values = env
                        .call_method(&activity, jni_str!("contentInsets"), jni_sig!("()[I"), &[])?
                        .l()?;
                    let values = env.cast_local::<JIntArray>(values)?;
                    let mut insets = [0; 4];
                    values.get_region(env, 0, &mut insets)?;
                    Ok(Update {
                        link,
                        name,
                        back,
                        insets,
                    })
                });
                match result {
                    Ok(mut next) => {
                        if let Ok(mut slot) = update.lock() {
                            if let Some(previous) = slot.take() {
                                next.back |= previous.back;
                                next.link = next.link.or(previous.link);
                            }
                            *slot = Some(next);
                        }
                    }
                    Err(error) => log::error!("Android controls failed: {error}"),
                }
                pending.store(false, Ordering::Release);
            }));
        }
        self.update.lock().ok().and_then(|mut slot| slot.take())
    }
}

#[unsafe(no_mangle)]
fn android_main(app: dioxus_native::AndroidApp) {
    android_logger::init_once(
        android_logger::Config::default()
            .with_tag("weld-connect")
            .with_max_level(log::LevelFilter::Info),
    );
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_env_filter(
            "info,iroh=warn,dioxus_signals=warn,weld_media_diag=debug,weld_network_diag=debug",
        )
        .with_writer(LogWriter::default)
        .finish();
    if let Err(error) = tracing::subscriber::set_global_default(subscriber) {
        log::warn!("tracing setup: {error}");
    }
    let Some(directory) = app.internal_data_path() else {
        log::error!("Android private data directory unavailable");
        return;
    };
    dioxus_native::set_android_app(app);
    if let Err(error) = crate::ui::launch(directory.join("connect"), "Weld Android".into()) {
        log::error!("client startup failed: {error:#}");
    }
}

#[derive(Default)]
struct LogWriter(Vec<u8>);
impl std::io::Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl Drop for LogWriter {
    fn drop(&mut self) {
        log::info!("{}", String::from_utf8_lossy(&self.0).trim_end());
    }
}
