//! Receive Android pairing links on the UI thread; keep trust decisions in the receiver.
use bevy::prelude::*;
use jni::{
    JValue, JavaVM,
    errors::Result,
    jni_sig, jni_str,
    objects::{JObject, JString},
    refs::Global,
};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Default, Resource)]
pub(super) struct Enrollment {
    pub paste: bool,
    pub scan: bool,
    pending: Arc<AtomicBool>,
    value: Arc<Mutex<Option<EnrollmentUpdate>>>,
    last: Option<Instant>,
}
pub(super) struct EnrollmentUpdate {
    pub link: Option<String>,
    pub name: String,
    pub development: bool,
}
impl Enrollment {
    pub fn poll(&mut self) -> Option<EnrollmentUpdate> {
        if (self.paste
            || self.scan
            || self
                .last
                .is_none_or(|last| last.elapsed() >= Duration::from_millis(500)))
            && !self.pending.swap(true, Ordering::AcqRel)
        {
            if let Some(android) = bevy::android::ANDROID_APP.get() {
                let app = android.clone();
                let pending = self.pending.clone();
                let value = self.value.clone();
                let paste = std::mem::take(&mut self.paste);
                let scan = std::mem::take(&mut self.scan);
                self.last = Some(Instant::now());
                android.run_on_java_main_thread(Box::new(move || {
                    // SAFETY: AndroidApp retains the process VM and its own activity global.
                    let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) };
                    let result = vm.attach_current_thread(|env| -> Result<EnrollmentUpdate> {
                        let raw = app.activity_as_ptr().cast();
                        // SAFETY: Borrow the app-owned global while its owner is captured here.
                        let activity = unsafe { env.as_cast_raw::<Global<JObject>>(&raw)? };
                        if scan {
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
                                &[JValue::Bool(paste)],
                            )?
                            .l()?;
                        let link = if link.is_null() {
                            None
                        } else {
                            Some(env.cast_local::<JString>(link)?.try_to_string(env)?)
                        };
                        let development = env
                            .call_method(
                                &activity,
                                jni_str!("developmentMode"),
                                jni_sig!("()Z"),
                                &[],
                            )?
                            .z()?;
                        let name = env
                            .call_method(
                                activity,
                                jni_str!("deviceName"),
                                jni_sig!("()Ljava/lang/String;"),
                                &[],
                            )?
                            .l()?;
                        let name = env.cast_local::<JString>(name)?.try_to_string(env)?;
                        Ok(EnrollmentUpdate {
                            link,
                            name,
                            development,
                        })
                    });
                    match result {
                        Ok(update) => {
                            if let Ok(mut value) = value.lock() {
                                *value = Some(update);
                            }
                        }
                        Err(error) => warn!(%error, "Android pairing entrypoint failed"),
                    }
                    pending.store(false, Ordering::Release);
                }));
            } else {
                self.pending.store(false, Ordering::Release);
            }
        }
        self.value.lock().ok().and_then(|mut value| value.take())
    }
}
