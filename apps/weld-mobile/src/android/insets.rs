//! Sample Android view insets on its UI thread, including edge-to-edge cutouts.
use bevy::prelude::*;
use jni::{Env, JavaVM, errors::Result, jni_sig, jni_str, objects::JObject, refs::Global};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Clone, Copy)]
struct Sample {
    size: [u32; 2],
    edges: [i32; 4],
}

#[derive(Default, Resource)]
pub(super) struct Insets {
    sample: Arc<Mutex<Option<Sample>>>,
    pending: Arc<AtomicBool>,
    last: Option<(Instant, [u32; 2])>,
    warned: Arc<AtomicBool>,
}

impl Insets {
    pub fn content(&mut self, size: [u32; 2]) -> Option<[i32; 4]> {
        if self.last.is_none_or(|(at, previous)| {
            previous != size || at.elapsed() >= Duration::from_millis(500)
        }) && !self.pending.swap(true, Ordering::AcqRel)
        {
            if let Some(android) = bevy::android::ANDROID_APP.get() {
                self.last = Some((Instant::now(), size));
                let app = android.clone();
                let sample = self.sample.clone();
                let pending = self.pending.clone();
                let warned = self.warned.clone();
                android.run_on_java_main_thread(Box::new(move || {
                    // SAFETY: AndroidApp owns this VM and remains alive through the call.
                    let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) };
                    let result = vm.attach_current_thread(|env| -> Result<Option<Sample>> {
                        let raw = app.activity_as_ptr().cast();
                        // SAFETY: Borrow the app-owned global reference for this local frame.
                        let activity = unsafe { env.as_cast_raw::<Global<JObject>>(&raw)? };
                        sample_insets(env, activity.as_ref())
                    });
                    match result {
                        Ok(value) => retain_sample(&sample, value),
                        Err(error) if !warned.swap(true, Ordering::AcqRel) => {
                            warn!(%error, "could not read Android safe-area insets")
                        }
                        Err(_) => {}
                    }
                    pending.store(false, Ordering::Release);
                }));
            } else {
                self.pending.store(false, Ordering::Release);
            }
        }
        self.sample.lock().ok().and_then(|sample| {
            let sample = sample.filter(|sample| sample.size == size)?;
            Some([
                sample.edges[0],
                sample.edges[1],
                size[0] as i32 - sample.edges[2],
                size[1] as i32 - sample.edges[3],
            ])
        })
    }
}

fn retain_sample(slot: &Mutex<Option<Sample>>, value: Option<Sample>) {
    if let Some(value) = value
        && let Ok(mut sample) = slot.lock()
    {
        *sample = Some(value);
    }
}

fn sample_insets(env: &mut Env<'_>, activity: &JObject<'_>) -> Result<Option<Sample>> {
    let window = env
        .call_method(
            activity,
            jni_str!("getWindow"),
            jni_sig!("()Landroid/view/Window;"),
            &[],
        )?
        .l()?;
    let view = env
        .call_method(
            &window,
            jni_str!("getDecorView"),
            jni_sig!("()Landroid/view/View;"),
            &[],
        )?
        .l()?;
    let insets = env
        .call_method(
            &view,
            jni_str!("getRootWindowInsets"),
            jni_sig!("()Landroid/view/WindowInsets;"),
            &[],
        )?
        .l()?;
    if insets.is_null() {
        return Ok(None);
    }
    let width = env
        .call_method(&view, jni_str!("getWidth"), jni_sig!("()I"), &[])?
        .i()?
        .max(0) as u32;
    let height = env
        .call_method(&view, jni_str!("getHeight"), jni_sig!("()I"), &[])?
        .i()?
        .max(0) as u32;
    // Stable bars remain reserved when transient UI is hidden. These methods
    // also work on API 28, the receiver's minimum Android version.
    let mut edges = [0; 4];
    for (edge, method) in edges.iter_mut().zip([
        jni_str!("getStableInsetLeft"),
        jni_str!("getStableInsetTop"),
        jni_str!("getStableInsetRight"),
        jni_str!("getStableInsetBottom"),
    ]) {
        *edge = env
            .call_method(&insets, method, jni_sig!("()I"), &[])?
            .i()?;
    }
    let cutout = env
        .call_method(
            &insets,
            jni_str!("getDisplayCutout"),
            jni_sig!("()Landroid/view/DisplayCutout;"),
            &[],
        )?
        .l()?;
    if !cutout.is_null() {
        for (edge, method) in edges.iter_mut().zip([
            jni_str!("getSafeInsetLeft"),
            jni_str!("getSafeInsetTop"),
            jni_str!("getSafeInsetRight"),
            jni_str!("getSafeInsetBottom"),
        ]) {
            *edge = (*edge).max(
                env.call_method(&cutout, method, jni_sig!("()I"), &[])?
                    .i()?,
            );
        }
    }
    Ok(Some(Sample {
        size: [width, height],
        edges,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_missing_insets_retain_last_valid_sample() {
        let cache = Mutex::new(None);
        retain_sample(
            &cache,
            Some(Sample {
                size: [1344, 2992],
                edges: [0, 151, 0, 72],
            }),
        );
        retain_sample(&cache, None);
        let sample = cache.lock().expect("cache").expect("retained sample");
        assert_eq!(sample.size, [1344, 2992]);
        assert_eq!(sample.edges, [0, 151, 0, 72]);
    }
}
