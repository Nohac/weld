//! Platform entrypoints return owned events to the application router.
#[derive(Clone, Default)]
pub struct Requests {
    pub scan: bool,
    pub paste: bool,
    pub background: bool,
    pub streaming: bool,
}
#[derive(Default)]
pub struct Update {
    pub link: Option<String>,
    pub name: String,
    pub back: bool,
    pub insets: [i32; 4],
}

#[cfg(target_os = "android")]
pub use crate::android::Bridge;
#[cfg(target_os = "linux")]
#[derive(Default)]
pub struct Bridge {}
#[cfg(target_os = "linux")]
impl Bridge {
    pub const AVAILABLE: bool = false;
    pub fn poll(&mut self, _: &mut Requests) -> Option<Update> {
        None
    }
}
